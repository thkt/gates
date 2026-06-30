use crate::audit;
use crate::circular;
use crate::clone;
use crate::coupling;
use crate::depgraph;
use crate::project::ProjectInfo;
use crate::resolve;
use crate::runner::{
    GATE_TIMEOUT, GateOutcome, ToolResult, join_or_skip, run_command, run_command_with_label,
};
use litmus::rules::{Issue, Severity};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::iter;
use std::panic::resume_unwind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

pub struct GateDefinition {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub hint: &'static str,
    pub condition: fn(&ProjectInfo) -> bool,
}

pub struct InstallInfo {
    pub name: &'static str,
    pub install: &'static str,
}

pub const INSTALL_COMMANDS: &[InstallInfo] = &[
    InstallInfo {
        name: "knip",
        install: "npm i -D knip",
    },
    InstallInfo {
        name: "tsgo",
        install: "npm i -g @typescript/native-preview",
    },
    InstallInfo {
        name: "oxlint",
        install: "npm i -D oxlint oxlint-tsgolint",
    },
];

/// `oxlint --type-aware` shells out to the project-local `tsgolint` backend. When it is
/// absent, oxlint exits nonzero with "Failed to find tsgolint executable" — which the
/// runner maps to a false `block` (it cannot tell a tool-launch failure from a real
/// violation). Gate the oxlint gate on tsgolint being resolvable so a tsconfig project
/// without it stays fail-open (skipped). Issue #31 AC: oxlint/tsgolint absence → skipped.
///
/// `resolve_bin` returns the bare name unchanged only on its not-found fallback, so any
/// other value means it resolved an executable `node_modules/.bin/tsgolint`.
fn tsgolint_available(root: &Path) -> bool {
    resolve::resolve_bin("tsgolint", root) != Path::new("tsgolint")
}

pub const GATES: &[GateDefinition] = &[
    GateDefinition {
        name: "knip",
        command: "knip",
        args: &[],
        hint: "Remove unused exports and dependencies.",
        condition: |p| p.has_package_json,
    },
    GateDefinition {
        name: "tsgo",
        command: "tsgo",
        args: &[],
        hint: "Fix type errors.",
        condition: |p| p.has_tsconfig,
    },
    // Type-aware lint (backend: tsgolint). `--max-warnings 0` promotes oxlint's
    // default-severity findings (type-aware rules emit as warnings) to a nonzero
    // exit so the gate blocks; `--type-check` is omitted to avoid double-reporting
    // type errors with the tsgo gate. Rule-set tuning is deferred (issue #31).
    // https://oxc.rs/docs/guide/usage/linter/type-aware.html
    GateDefinition {
        name: "oxlint",
        command: "oxlint",
        args: &["--type-aware", "--max-warnings", "0"],
        hint: "Fix type-aware lint violations (e.g. floating promises).",
        condition: |p| p.has_tsconfig && tsgolint_available(&p.root),
    },
    // dependency-cruiser auto-detects .dependency-cruiser.{js,cjs,mjs,json} since v13,
    // so --config is omitted; the condition gates on the same four formats it detects.
    // https://github.com/sverweij/dependency-cruiser/blob/main/doc/cli.md
    GateDefinition {
        name: "depcruise",
        command: "dependency-cruiser",
        args: &["src/"],
        hint: "Fix architecture boundary violations.",
        condition: |p| {
            ["js", "cjs", "mjs", "json"]
                .iter()
                .any(|ext| p.root.join(format!(".dependency-cruiser.{ext}")).exists())
        },
    },
];

pub struct ScriptGate {
    pub name: &'static str,
    pub command: String,
    pub hint: &'static str,
}

#[derive(Default)]
pub struct EnvOverrides {
    pub lint_cmd: Option<String>,
    pub type_cmd: Option<String>,
    pub unit_cmd: Option<String>,
    pub test_cmd: Option<String>,
    /// Directory for the audit log. Defaults to XDG resolution; tests inject a
    /// temp dir so they never touch the real `~/.local/share/gates`.
    pub audit_dir: Option<PathBuf>,
    /// Directory for the per-project filesystem-delta snapshot (issue #17).
    /// Defaults to the `snapshots/` subdir of the audit dir; tests inject a temp
    /// dir. `None` (XDG/HOME unset) makes reads "changed" and writes a no-op.
    pub snapshot_dir: Option<PathBuf>,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        Self::from_env_with(|key| env::var(key).ok())
    }

    /// Reads the command overrides through an injected getter so tests can
    /// exercise the env-name contract without mutating process-global state
    /// (`env::set_var` is `unsafe`, which this crate forbids). Override names
    /// carry the `GATES_` prefix to avoid colliding with unrelated CI/project
    /// vars (a bare `TEST_CMD=jest` would otherwise silently hijack the gate);
    /// no bare fallback is read, so the collision cannot reappear (#95).
    fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Self {
        let cmd = |key: &str| get(key).filter(|s| !s.is_empty());
        Self {
            lint_cmd: cmd("GATES_LINT_CMD"),
            type_cmd: cmd("GATES_TYPE_CMD"),
            unit_cmd: cmd("GATES_UNIT_CMD"),
            test_cmd: cmd("GATES_TEST_CMD"),
            audit_dir: audit::default_dir(),
            snapshot_dir: audit::default_dir().map(|d| d.join("snapshots")),
        }
    }
}

fn detect_run_prefix(project_dir: &Path) -> Option<String> {
    let candidates: &[(&str, &str)] = &[
        ("pnpm-lock.yaml", "pnpm run"),
        ("bun.lock", "bun run"),
        ("yarn.lock", "yarn run"),
        ("package-lock.json", "npm run"),
    ];
    for (lock_file, prefix) in candidates {
        if project_dir.join(lock_file).exists() {
            return Some((*prefix).into());
        }
    }
    None
}

pub fn detect_script_gates_with_overrides(
    overrides: &EnvOverrides,
    project_dir: &Path,
) -> Vec<ScriptGate> {
    let run_prefix = detect_run_prefix(project_dir);
    detect_script_gates_inner(overrides, project_dir, run_prefix.as_deref())
}

fn detect_script_gates_inner(
    overrides: &EnvOverrides,
    project_dir: &Path,
    run_prefix: Option<&str>,
) -> Vec<ScriptGate> {
    let mut gates = Vec::new();

    let lint_cmd = overrides.lint_cmd.clone();
    let type_cmd = overrides.type_cmd.clone();
    let unit_cmd = overrides.unit_cmd.clone();

    let scripts = read_package_scripts(project_dir);

    if let Some(cmd) = lint_cmd {
        gates.push(ScriptGate {
            name: "lint",
            command: cmd,
            hint: "Fix lint errors.",
        });
    } else if let Some(prefix) = run_prefix
        && scripts.contains("lint")
    {
        gates.push(ScriptGate {
            name: "lint",
            command: format!("{prefix} lint"),
            hint: "Fix lint errors.",
        });
    }

    let has_type_check = if let Some(cmd) = type_cmd {
        gates.push(ScriptGate {
            name: "type-check",
            command: cmd,
            hint: "Fix type errors.",
        });
        true
    } else if let Some(prefix) = run_prefix {
        if scripts.contains("test:type") {
            gates.push(ScriptGate {
                name: "type-check",
                command: format!("{prefix} test:type"),
                hint: "Fix type errors.",
            });
            true
        } else if scripts.contains("typecheck") {
            gates.push(ScriptGate {
                name: "type-check",
                command: format!("{prefix} typecheck"),
                hint: "Fix type errors.",
            });
            true
        } else {
            false
        }
    } else {
        false
    };

    // test:unit preferred; "test" fallback only without type-check
    if let Some(cmd) = unit_cmd {
        gates.push(ScriptGate {
            name: "test",
            command: cmd,
            hint: "Fix test failures.",
        });
    } else if let Some(prefix) = run_prefix {
        if scripts.contains("test:unit") {
            gates.push(ScriptGate {
                name: "test",
                command: format!("{prefix} test:unit"),
                hint: "Fix test failures.",
            });
        } else if !has_type_check && scripts.contains("test") {
            gates.push(ScriptGate {
                name: "test",
                command: format!("{prefix} test"),
                hint: "Fix test failures.",
            });
        }
    }

    gates
}

/// Run script gates with type-check → test cascade logic.
/// lint runs independently; if type-check fails, test is skipped.
pub fn run_script_gates(gates: &[ScriptGate], project_dir: &Path) -> Vec<ToolResult> {
    let mut results = Vec::new();

    let lint = gates.iter().find(|g| g.name == "lint");
    let type_check = gates.iter().find(|g| g.name == "type-check");
    let test = gates.iter().find(|g| g.name == "test");

    let lint_handle = lint.map(|g| {
        let cmd_str = g.command.clone();
        let hint = g.hint;
        let dir = project_dir.to_path_buf();
        thread::spawn(move || vec![run_shell_command("lint", &cmd_str, hint, &dir)])
    });

    if let Some(tc) = type_check {
        let tc_result = run_shell_command("type-check", &tc.command, tc.hint, project_dir);
        let tc_result = downgrade_if_unbootstrapped(tc_result);
        // Skip test on a real type failure (cascade) and also when type-check was
        // downgraded to an unbootstrapped-env Warned: test runs the same project
        // and would re-hit the identical missing-module noise (issue #89).
        let skip_test = tc_result.is_failure() || tc_result.is_warning();
        results.push(tc_result);

        if let Some(t) = test {
            if skip_test {
                results.push(ToolResult::skipped("test"));
            } else {
                results.push(run_shell_command("test", &t.command, t.hint, project_dir));
            }
        }
    } else if let Some(t) = test {
        results.push(run_shell_command("test", &t.command, t.hint, project_dir));
    }

    if let Some(handle) = lint_handle {
        results.extend(join_or_skip(handle.join(), &["lint"]));
    }

    results
}

fn run_shell_command(
    name: &'static str,
    cmd_str: &str,
    hint: &'static str,
    project_dir: &Path,
) -> ToolResult {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", cmd_str]).current_dir(project_dir);
    let label = cmd_str.to_owned();
    let mut result = run_command_with_label(name, cmd, GATE_TIMEOUT, Some(&label));
    result.hint = hint;
    result
}

fn read_package_scripts(project_dir: &Path) -> HashSet<String> {
    let path = project_dir.join("package.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return HashSet::new();
        }
        Err(e) => {
            eprintln!("gates: failed to read {}: {}", path.display(), e);
            return HashSet::new();
        }
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        eprintln!("gates: failed to parse {}", path.display());
        return HashSet::new();
    };
    let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) else {
        return HashSet::new();
    };
    scripts.keys().cloned().collect()
}

#[cfg(test)]
pub fn gate_by_name(name: &str) -> &'static GateDefinition {
    GATES
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("gate '{name}' not found"))
}

// litmus calibrates its parser's stack headroom against this size: its CLI runs
// `analyze_files` on a 256MiB worker (litmus `ANALYZER_STACK_SIZE`, src/main.rs).
// The library path we use here would otherwise run on a default ~2MiB gate thread,
// where right-associative forms with no brackets to bound recursion (`a=b=c`,
// ternary alternate, `**`, prefix-unary) overflow far sooner than litmus expects.
// Mirroring the size lifts the overflow floor to litmus's ~250k-level parity.
//
// This is probability reduction, not containment: a deeper overflow still aborts
// via SIGABRT, which is signal death that `join_or_skip` cannot catch and which
// takes the whole gates process down with it. True isolation needs a subprocess
// or an upstream litmus `analyze_files_isolated` entry (out of #94 scope).
//
// The value duplicates litmus's unexported `ANALYZER_STACK_SIZE`, which litmus
// couples to its `BRACKET_DEPTH_LIMIT`; re-verify this number whenever the pinned
// litmus rev (Cargo.toml) is bumped.
const ANALYZER_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Run `litmus::analyze_files` on a thread with `ANALYZER_STACK_SIZE`, matching
/// litmus's own worker so the library path has the same overflow floor as its CLI.
/// A child panic is re-raised on the caller so the gate thread's `join_or_skip`
/// still degrades it to skipped (fail-open). If the thread cannot be spawned we
/// fall back to an inline call, mirroring litmus's main-thread fallback.
fn analyze_files_on_deep_stack(files: &[PathBuf]) -> litmus::AnalysisResult {
    thread::scope(|scope| {
        match thread::Builder::new()
            .stack_size(ANALYZER_STACK_SIZE)
            .spawn_scoped(scope, || litmus::analyze_files(files))
        {
            Ok(handle) => match handle.join() {
                Ok(result) => result,
                Err(payload) => resume_unwind(payload),
            },
            Err(error) => {
                eprintln!("gates: litmus deep-stack thread spawn failed, running inline: {error}");
                litmus::analyze_files(files)
            }
        }
    })
}

pub fn run_litmus(project: &ProjectInfo) -> Vec<ToolResult> {
    if !project.has_package_json {
        return vec![ToolResult::skipped("litmus")];
    }

    let files = litmus::find_test_files(&project.root);
    if files.is_empty() {
        return vec![ToolResult::skipped("litmus")];
    }

    let result = analyze_files_on_deep_stack(&files);

    for error in &result.errors {
        eprintln!("gates: {error}");
    }

    if result.issues.is_empty() {
        return vec![ToolResult::passed("litmus")];
    }

    // litmus tags dummy-data / missing-act / snapshot-external as advisory
    // (`Severity::Warning`, exit 1 in its CLI) and everything else as blocking
    // (exit 2). Route each tier to gates' matching outcome so warnings reach the
    // human without blocking the AI, and surface both when they co-occur rather
    // than letting a blocking result swallow the warnings (gates can emit more
    // than one result per gate, e.g. run_graph_gates).
    // Match exhaustively rather than `== Severity::Warning`: `Severity` is not
    // `#[non_exhaustive]`, so a future litmus variant breaks compilation here and
    // forces a deliberate routing choice, instead of silently defaulting to the
    // blocking bucket and blocking the AI against gates' fail-open posture.
    let (warnings, blocking): (Vec<_>, Vec<_>) =
        result
            .issues
            .iter()
            .partition(|issue| match issue.severity() {
                Severity::Warning => true,
                Severity::Blocking => false,
            });

    let render = |issues: &[&Issue]| {
        issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut results = Vec::new();
    if !blocking.is_empty() {
        results.push(ToolResult::failed(
            "litmus",
            "Fix test quality issues (weak assertions, mock overuse, tautological tests).",
            &render(&blocking),
        ));
    }
    if !warnings.is_empty() {
        results.push(ToolResult::warned(
            "litmus",
            "Advisory test-quality warnings (litmus warning tier); not blocking.",
            &render(&warnings),
        ));
    }
    results
}

/// Build the dependency graph once and run the circular and coupling gates that
/// read from it, so the project is parsed a single time per hook invocation.
pub fn run_graph_gates(
    project: &ProjectInfo,
    circular_enabled: bool,
    coupling_enabled: bool,
    ca_threshold: Option<usize>,
) -> Vec<ToolResult> {
    let src_dir = project.root.join("src");
    if !project.has_package_json || !src_dir.is_dir() {
        let mut out = Vec::new();
        if circular_enabled {
            out.push(ToolResult::skipped("circular"));
        }
        if coupling_enabled {
            out.push(ToolResult::skipped("coupling"));
        }
        return out;
    }

    let graph = depgraph::build(&src_dir);
    let mut out = Vec::new();
    if circular_enabled {
        out.push(circular_result(&circular::detect_in(&graph, &src_dir)));
    }
    if coupling_enabled {
        out.push(coupling_result(&graph, &src_dir, ca_threshold));
    }
    out
}

fn circular_result(result: &circular::CircularResult) -> ToolResult {
    if result.cycles.is_empty() {
        return ToolResult::passed("circular");
    }

    let n = result.cycles.len();
    let header = format!(
        "Found {} circular {}:\n",
        n,
        if n == 1 { "dependency" } else { "dependencies" }
    );
    let body: String = result
        .cycles
        .iter()
        .map(|cycle| {
            cycle
                .iter()
                .map(String::as_str)
                .chain(iter::once(cycle[0].as_str()))
                .collect::<Vec<_>>()
                .join(" → ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::failed(
        "circular",
        "Break circular import dependencies.",
        &format!("{header}{body}"),
    )
}

fn coupling_result(
    graph: &depgraph::DependencyGraph,
    src_dir: &Path,
    ca_threshold: Option<usize>,
) -> ToolResult {
    let Some(threshold) = ca_threshold else {
        return ToolResult::skipped("coupling");
    };

    let result = coupling::analyze(graph, src_dir, threshold);
    if result.god_modules.is_empty() {
        return ToolResult::passed("coupling");
    }

    let n = result.god_modules.len();
    let header = format!("Found {n} god module(s) (Ca > {threshold}):\n");
    let body: String = result
        .god_modules
        .iter()
        .map(|m| format!("{}  Ca={} Ce={} I={:.2}", m.path, m.ca, m.ce, m.instability))
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::failed(
        "coupling",
        "Reduce afferent coupling (Ca) on the listed modules; split responsibilities or introduce an abstraction layer.",
        &format!("{header}{body}"),
    )
}

/// Number of clone groups to list in the failure report before truncating.
const MAX_REPORTED_GROUPS: usize = 10;

/// Detect Type 1/2 structural clones across the project's `src` tree and block
/// when the number of clone groups reaches `block_threshold`.
pub fn run_clone(
    project: &ProjectInfo,
    min_nodes: usize,
    min_lines: usize,
    block_threshold: usize,
) -> ToolResult {
    let src_dir = project.root.join("src");
    if !project.has_package_json || !src_dir.is_dir() {
        return ToolResult::skipped("clone");
    }

    // Declaration files (`*.d.ts`) carry only type shapes, which the gate's
    // "extract into a shared function" remedy cannot address, so exclude them.
    let files: Vec<PathBuf> = depgraph::collect_files(&src_dir)
        .into_iter()
        .filter(|p| !p.to_string_lossy().ends_with(".d.ts"))
        .collect();
    if files.is_empty() {
        return ToolResult::skipped("clone");
    }

    let result = clone::analyze(&files, &src_dir, min_nodes, min_lines);
    if result.groups.len() < block_threshold {
        return ToolResult::passed("clone");
    }

    ToolResult::failed(
        "clone",
        "Extract the duplicated structures into a shared function or module (DRY).",
        &clone_report(&result.groups),
    )
}

fn clone_report(groups: &[clone::CloneGroup]) -> String {
    let n = groups.len();
    let header = format!(
        "Found {n} structural clone group{}:",
        if n == 1 { "" } else { "s" }
    );
    let mut lines = vec![header];
    for (i, group) in groups.iter().take(MAX_REPORTED_GROUPS).enumerate() {
        let kind = if group.exact {
            "Type 1 (exact)"
        } else {
            "Type 2 (structural)"
        };
        lines.push(format!(
            "#{} {} — {} copies, {} nodes",
            i + 1,
            kind,
            group.instances.len(),
            group.node_count
        ));
        for inst in &group.instances {
            lines.push(format!(
                "    {}:{}-{}",
                inst.path, inst.start_line, inst.end_line
            ));
        }
    }
    if n > MAX_REPORTED_GROUPS {
        lines.push(format!("... and {} more group(s)", n - MAX_REPORTED_GROUPS));
    }
    lines.join("\n")
}

pub const DEFAULT_JSCPD_MIN_LINES: usize = 5;
pub const DEFAULT_JSCPD_MIN_TOKENS: usize = 50;
pub const DEFAULT_JSCPD_THRESHOLD: f64 = 10.0;

/// Default ignore globs: dependencies, plus test/spec files and generated/build
/// output, which the gate's "extract shared code" remedy does not apply to.
/// `node_modules` is normally excluded by jscpd's gitignore handling, but is
/// listed here so a repo that does not gitignore it still skips the scan (which
/// would otherwise blow the gate's timeout). jscpd's own docs use this glob.
/// `.git` is not covered by gitignore (it is the git dir itself); its sample
/// hooks are near-identical and would surface as clone groups, so exclude it.
pub const DEFAULT_JSCPD_IGNORE: &[&str] = &[
    "**/node_modules/**",
    "**/.git/**",
    "**/*.test.*",
    "**/*.spec.*",
    "**/generated/**",
    "**/dist/**",
];

const JSCPD_HINT: &str = "Extract the duplicated code into a shared function or module (DRY).";

/// Subset of jscpd's JSON report the gate reads. `duplicates` and its inner
/// fields default so a shape change in the (prose-documented) duplicate entries
/// degrades the file-pair listing rather than failing the parse and silently
/// skipping the gate; the load-bearing `percentage` stays required.
#[derive(Deserialize)]
pub struct JscpdReport {
    statistics: JscpdStatistics,
    #[serde(default)]
    duplicates: Vec<JscpdDuplicate>,
}

#[derive(Deserialize)]
struct JscpdStatistics {
    total: JscpdTotal,
}

#[derive(Deserialize)]
struct JscpdTotal {
    percentage: f64,
}

#[derive(Deserialize)]
struct JscpdDuplicate {
    #[serde(default)]
    lines: usize,
    #[serde(rename = "firstFile", default)]
    first_file: JscpdFile,
    #[serde(rename = "secondFile", default)]
    second_file: JscpdFile,
}

#[derive(Deserialize, Default)]
struct JscpdFile {
    #[serde(default)]
    name: String,
}

fn parse_jscpd_report(json: &str) -> Option<JscpdReport> {
    serde_json::from_str(json).ok()
}

/// Map a parsed report to an outcome. Duplication at or below `threshold`
/// passes; above it warns (advisory) or, when `block` is set, fails (blocks).
fn jscpd_outcome(report: &JscpdReport, threshold: f64, block: bool) -> ToolResult {
    if report.statistics.total.percentage <= threshold {
        return ToolResult::passed("jscpd");
    }
    let text = jscpd_report(report, threshold);
    if block {
        ToolResult::failed("jscpd", JSCPD_HINT, &text)
    } else {
        ToolResult::warned("jscpd", JSCPD_HINT, &text)
    }
}

fn jscpd_report(report: &JscpdReport, threshold: f64) -> String {
    let pct = report.statistics.total.percentage;
    let mut lines = vec![format!(
        "Found duplication: {pct}% (threshold {threshold}%)"
    )];
    for dup in report.duplicates.iter().take(MAX_REPORTED_GROUPS) {
        lines.push(format!(
            "    {} \u{2194} {} ({} lines)",
            dup.first_file.name, dup.second_file.name, dup.lines
        ));
    }
    let n = report.duplicates.len();
    if n > MAX_REPORTED_GROUPS {
        lines.push(format!(
            "    ... and {} more pair(s)",
            n - MAX_REPORTED_GROUPS
        ));
    }
    lines.join("\n")
}

/// Run jscpd (token-based Type 3 clone detection) over the project. jscpd writes
/// its JSON to a file, not stdout, so the report is directed to a temp dir and
/// read back. Fail-open: a missing binary, timeout, or unreadable/unparseable
/// report all skip rather than block.
pub fn run_jscpd(
    project: &ProjectInfo,
    min_lines: usize,
    min_tokens: usize,
    threshold: f64,
    block: bool,
    ignore: &[String],
) -> ToolResult {
    if !project.has_package_json {
        return ToolResult::skipped("jscpd");
    }

    // A fresh, randomly-named temp dir per run (tempfile, CWE-377): an
    // unprivileged process cannot predict the path to pre-create a symlink and
    // hijack the report read/write. A unique name per run also means no stale
    // report can survive across runs, and `TempDir`'s Drop removes the dir when
    // this function returns.
    let temp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gates: jscpd temp dir create failed: {e}");
            return ToolResult::skipped("jscpd");
        }
    };
    let out_dir = temp.path();

    let bin = resolve::resolve_bin("jscpd", &project.root);
    let mut cmd = Command::new(&bin);
    cmd.arg(&project.root)
        .args(["--reporters", "json"])
        .arg("--output")
        .arg(out_dir)
        .arg("--silent")
        .args(["--min-lines", &min_lines.to_string()])
        .args(["--min-tokens", &min_tokens.to_string()]);
    if !ignore.is_empty() {
        cmd.args(["--ignore", &ignore.join(",")]);
    }
    cmd.current_dir(&project.root);

    let result = run_command("jscpd", cmd, GATE_TIMEOUT);
    // `temp` (and thus `out_dir`) stays alive through this tail expression, then
    // its Drop removes the temp dir as the function returns.
    if result.is_skipped() {
        // Binary missing or timed out: fail-open.
        result
    } else {
        let report_path = out_dir.join("jscpd-report.json");
        match fs::read_to_string(&report_path) {
            Ok(json) => match parse_jscpd_report(&json) {
                Some(report) => jscpd_outcome(&report, threshold, block),
                None => {
                    eprintln!("gates: jscpd report parse failed");
                    ToolResult::skipped("jscpd")
                }
            },
            Err(e) => {
                eprintln!("gates: jscpd report read failed: {e}");
                ToolResult::skipped("jscpd")
            }
        }
    }
}

/// Distinct banner prepended to a downgraded type gate's output so the human reads
/// "this is an environment problem, not a code defect" as the first advisory line
/// (issue #89 asks for a distinct message). Prepended rather than carried in `hint`
/// because the advisory render path (`reporter::append_advisories`) previews
/// `output()`, not `hint`.
const ENV_NOT_READY_BANNER: &str = "Environment not bootstrapped: dependencies or codegen outputs are missing \
     (unresolved modules below). Run the project's install/codegen (e.g. `npm install`), \
     then re-run. Advisory only — not blocking.";

/// True when a type checker's failure carries an unresolved-module error (TS2307),
/// the signal that the project isn't bootstrapped — dependencies uninstalled or
/// codegen not run — rather than holding a logic defect. `tsc` and `tsgo` both emit
/// `error TS2307` for an absent package AND for an absent relative import of a
/// not-yet-generated file, so one diagnostic code covers both unbootstrapped shapes
/// (issue #89; the issue quotes the exact strings).
///
/// Deliberately dumb: it matches exactly one code, not a taxonomy. A present TS2307
/// downgrades the *whole* run to advisory even when other diagnostics coexist (e.g.
/// the issue's prisma `error TS2339`). This is a deliberate tradeoff, not precision
/// loss: unresolved modules are a whole-run property. TypeScript checks the program
/// as a whole, so an unresolved import resolves to `any` and poisons inference
/// across every file that touches it — masking real errors and fabricating spurious
/// ones. No single diagnostic from a broken-resolution run is trustworthy, so the
/// coherent action is to advise on the entire run rather than block on any of it.
///
/// The cost: a genuine type error that coexists with a TS2307 in one run is not
/// blocked during that run. It is not buried — the full output is still shown to
/// the human (advisory) — and it re-blocks once resolution succeeds and the TS2307
/// clears. The strict alternative (downgrade only when TS2307 is the *sole* error)
/// was rejected: it cannot see past the 50-line output truncation, and it leaves
/// the issue's headline tsgo example (TS2307 + TS2339) blocking — the case #89
/// exists to fix.
///
/// Matching the code (not the message text) keeps detection locale-independent; if
/// TypeScript renumbers TS2307 the match stops and the gate reverts to today's
/// blocking behavior (fail-safe).
fn is_unbootstrapped_failure(output: &str) -> bool {
    output.contains("error TS2307:")
}

/// Reclassify a type gate's blocking `Failed` as an advisory `Warned` when its
/// failure is an unbootstrapped environment (issue #89), prefixing the distinct
/// banner and preserving the captured output so the human still sees both via
/// stderr. Non-failures and real type failures pass through unchanged.
fn downgrade_if_unbootstrapped(mut result: ToolResult) -> ToolResult {
    if let GateOutcome::Failed(text) = &result.outcome
        && is_unbootstrapped_failure(text)
    {
        // Build the banner-prefixed output by hand rather than via
        // `ToolResult::warned`, which re-runs `tail_lines` and would drop the
        // prepended banner once the (already-truncated) text fills the limit.
        let downgraded = format!("{ENV_NOT_READY_BANNER}\n{text}");
        result.hint = "";
        result.outcome = GateOutcome::Warned(downgraded);
    }
    result
}

pub fn run_gate(gate: &GateDefinition, project: &ProjectInfo) -> ToolResult {
    if !(gate.condition)(project) {
        return ToolResult::skipped(gate.name);
    }

    let bin = resolve::resolve_bin(gate.command, &project.root);
    let mut cmd = Command::new(&bin);
    cmd.args(gate.args).current_dir(&project.root);
    let mut result = run_command(gate.name, cmd, GATE_TIMEOUT);
    result.hint = gate.hint;
    // tsgo is intentionally the sole gate that surfaces an unbootstrapped env: only
    // it emits TS2307, so the predicate is false for every other gate and this is a
    // no-op there (no per-gate guard needed). The cause is project-level, so sibling
    // gates (depcruise/knip) can independently block on the same missing module; that
    // residual is accepted for #89's scope rather than downgrading every gate.
    downgrade_if_unbootstrapped(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::strip_ansi;
    use crate::reporter::format_summary;
    use crate::test_utils::{TempDir, link_fake_bin};
    use std::fs;
    use std::path::PathBuf;
    fn test_project(has_pkg: bool, has_ts: bool) -> ProjectInfo {
        ProjectInfo {
            root: PathBuf::from("/tmp/nonexistent"),
            has_package_json: has_pkg,
            has_tsconfig: has_ts,
        }
    }

    fn setup_package_json(scripts: &str) -> TempDir {
        let tmp = TempDir::new("script-gate");
        fs::write(
            tmp.join("package.json"),
            format!(r#"{{"scripts":{{{scripts}}}}}"#),
        )
        .unwrap();
        tmp
    }

    fn no_overrides() -> EnvOverrides {
        EnvOverrides::default()
    }

    #[test]
    fn from_env_reads_gates_prefixed_names() {
        let overrides = EnvOverrides::from_env_with(|key| match key {
            "GATES_LINT_CMD" => Some("gates-lint".into()),
            "GATES_TYPE_CMD" => Some("gates-type".into()),
            "GATES_UNIT_CMD" => Some("gates-unit".into()),
            "GATES_TEST_CMD" => Some("gates-test".into()),
            _ => None,
        });
        assert_eq!(overrides.lint_cmd.as_deref(), Some("gates-lint"));
        assert_eq!(overrides.type_cmd.as_deref(), Some("gates-type"));
        assert_eq!(overrides.unit_cmd.as_deref(), Some("gates-unit"));
        assert_eq!(overrides.test_cmd.as_deref(), Some("gates-test"));
    }

    #[test]
    fn from_env_ignores_bare_unprefixed_names() {
        // A bare `TEST_CMD=jest` from unrelated CI must not hijack the gate (#95).
        let overrides = EnvOverrides::from_env_with(|key| match key {
            "LINT_CMD" | "TYPE_CMD" | "UNIT_CMD" | "TEST_CMD" => Some("hijack".into()),
            _ => None,
        });
        assert_eq!(overrides.lint_cmd, None);
        assert_eq!(overrides.type_cmd, None);
        assert_eq!(overrides.unit_cmd, None);
        assert_eq!(overrides.test_cmd, None);
    }

    #[test]
    fn from_env_filters_empty_override_to_none() {
        let overrides =
            EnvOverrides::from_env_with(|key| (key == "GATES_LINT_CMD").then(String::new));
        assert_eq!(overrides.lint_cmd, None);
    }

    const NPM: Option<&str> = Some("npm run");

    #[test]
    fn detect_lint_gate_when_script_exists() {
        let tmp = setup_package_json(r#""lint":"eslint .""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].name, "lint");
        assert_eq!(gates[0].command, "npm run lint");
    }

    #[test]
    fn no_lint_gate_when_no_script() {
        let tmp = setup_package_json(r#""test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        assert!(gates.iter().all(|g| g.name != "lint"));
    }

    #[test]
    fn detect_type_check_gate_test_type() {
        let tmp = setup_package_json(r#""test:type":"tsc --noEmit""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        assert!(gates.iter().any(|g| g.name == "type-check"));
    }

    #[test]
    fn detect_type_check_gate_typecheck() {
        let tmp = setup_package_json(r#""typecheck":"tsc --noEmit""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        assert!(gates.iter().any(|g| g.name == "type-check"));
    }

    #[test]
    fn detect_test_gate_unit_with_type_check() {
        let tmp = setup_package_json(r#""test:type":"tsc","test:unit":"vitest","test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        let test_gate = gates.iter().find(|g| g.name == "test").unwrap();
        assert!(test_gate.command.contains("test:unit"));
    }

    #[test]
    fn detect_test_gate_fallback_to_test() {
        let tmp = setup_package_json(r#""test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, NPM);
        let test_gate = gates.iter().find(|g| g.name == "test").unwrap();
        assert!(test_gate.command.ends_with("test"));
    }

    #[test]
    fn env_override_lint_cmd() {
        let tmp = setup_package_json(r#""lint":"eslint .""#);
        let overrides = EnvOverrides {
            lint_cmd: Some("custom-lint".into()),
            ..Default::default()
        };
        let gates = detect_script_gates_inner(&overrides, &tmp, None);
        let lint_gate = gates.iter().find(|g| g.name == "lint").unwrap();
        assert_eq!(lint_gate.command, "custom-lint");
    }

    #[test]
    fn type_check_fail_cascades_to_skip_test() {
        let tmp = TempDir::new("cascade");
        fs::create_dir_all(tmp.join(".git")).unwrap();

        let type_gate = ScriptGate {
            name: "type-check",
            command: "sh -c 'echo type-error && exit 1'".into(),
            hint: "Fix type errors.",
        };
        let test_gate = ScriptGate {
            name: "test",
            command: "echo test-ok".into(),
            hint: "Fix test failures.",
        };
        let results = run_script_gates(&[type_gate, test_gate], &tmp);
        let type_result = results.iter().find(|r| r.name == "type-check").unwrap();
        let test_result = results.iter().find(|r| r.name == "test").unwrap();
        assert!(type_result.is_failure());
        assert!(
            test_result.is_skipped(),
            "test should be skipped when type-check fails"
        );
    }

    // T-89-1: a failure of only TS2307 missing-package errors reads as unbootstrapped.
    #[test]
    fn unbootstrapped_detects_missing_package() {
        let output = "src/a.ts(1,23): error TS2307: Cannot find module 'remark-cjk-friendly' or its corresponding type declarations.";
        assert!(is_unbootstrapped_failure(output));
    }

    // T-89-2: a TS2307 on a relative import (codegen output not yet generated) also
    // reads as unbootstrapped — the same diagnostic code covers missing files.
    #[test]
    fn unbootstrapped_detects_missing_codegen_file() {
        let output = "src/api.ts(3,10): error TS2307: Cannot find module './llmApi.generated' or its corresponding type declarations.";
        assert!(is_unbootstrapped_failure(output));
    }

    // T-89-3: a real type error (non-TS2307) is not an unbootstrapped env.
    #[test]
    fn unbootstrapped_false_for_real_type_error() {
        let output = "src/x.ts(5,1): error TS2345: Argument of type 'string' is not assignable to parameter of type 'number'.";
        assert!(!is_unbootstrapped_failure(output));
    }

    // T-89-4: a coexisting error (e.g. the issue's prisma TS2339) does not defeat
    // detection — any present TS2307 marks the run as unbootstrapped, so the issue's
    // headline tsgo example (TS2307 + TS2339) downgrades instead of blocking. The
    // genuine error re-surfaces as blocking once the env is bootstrapped (TS2307 gone).
    #[test]
    fn unbootstrapped_true_when_other_error_coexists() {
        let output = "src/a.ts(1,1): error TS2307: Cannot find module 'x'\n\
                      app/models/x.ts(59,30): error TS2339: Property 'updateTask' does not exist on type 'PrismaClient'.";
        assert!(is_unbootstrapped_failure(output));
    }

    // T-89-5: output with no TS diagnostics (empty / non-TS crash) is not unbootstrapped.
    #[test]
    fn unbootstrapped_false_without_ts_errors() {
        assert!(!is_unbootstrapped_failure(""));
        assert!(!is_unbootstrapped_failure("Segmentation fault"));
    }

    // T-89-6: a tsgo Failed carrying TS2307 is downgraded to an advisory Warned that
    // prefixes the distinct env-not-ready banner and preserves the captured output.
    #[test]
    fn downgrade_turns_unbootstrapped_failure_into_warning() {
        let failed = ToolResult::failed(
            "tsgo",
            "Fix type errors.",
            "src/a.ts(1,1): error TS2307: Cannot find module 'x'",
        );
        let result = downgrade_if_unbootstrapped(failed);
        assert!(result.is_warning(), "TS2307 failure should downgrade");
        assert!(!result.is_failure());
        assert!(
            result.output().contains("TS2307"),
            "preserves original output"
        );
        assert!(
            result.output().starts_with("Environment not bootstrapped"),
            "distinct banner must lead the advisory output: {}",
            result.output()
        );
    }

    // T-89-6b: a real type failure passes through downgrade unchanged (stays blocking).
    #[test]
    fn downgrade_keeps_real_failure_blocking() {
        let failed = ToolResult::failed(
            "tsgo",
            "Fix type errors.",
            "src/x.ts(5,1): error TS2345: not assignable",
        );
        let result = downgrade_if_unbootstrapped(failed);
        assert!(result.is_failure(), "real type error must keep blocking");
    }

    // T-89-7: a type-check that fails with only TS2307 downgrades to Warned AND skips
    // test (test shares the unbootstrapped env and would re-report the same noise).
    #[test]
    fn type_check_unbootstrapped_downgrades_and_skips_test() {
        let tmp = TempDir::new("cascade-env");
        fs::create_dir_all(tmp.join(".git")).unwrap();

        let type_gate = ScriptGate {
            name: "type-check",
            command: "echo \"src/a.ts(1,1): error TS2307: Cannot find module 'x'\"; exit 1".into(),
            hint: "Fix type errors.",
        };
        let test_gate = ScriptGate {
            name: "test",
            command: "echo test-ok".into(),
            hint: "Fix test failures.",
        };
        let results = run_script_gates(&[type_gate, test_gate], &tmp);
        let type_result = results.iter().find(|r| r.name == "type-check").unwrap();
        let test_result = results.iter().find(|r| r.name == "test").unwrap();
        assert!(
            type_result.is_warning(),
            "TS2307-only type-check failure should downgrade to advisory Warned"
        );
        assert!(
            test_result.is_skipped(),
            "test must be skipped when the env is unbootstrapped"
        );
    }

    // T-89-9: end-to-end render. A downgraded tsgo result, run through the real
    // reporter, surfaces the env-not-ready banner as the first advisory preview
    // line (the banner is one logical line, so push_preview's non-blank filter
    // keeps it leading) and stays out of the BLOCKED section.
    #[test]
    fn downgraded_result_renders_banner_first_in_advisory() {
        let failed = ToolResult::failed(
            "tsgo",
            "Fix type errors.",
            "src/a.ts(1,1): error TS2307: Cannot find module 'remark-cjk-friendly'",
        );
        let downgraded = downgrade_if_unbootstrapped(failed);
        let rendered = strip_ansi(&format_summary(&[downgraded]));
        assert!(
            !rendered.contains("BLOCKED"),
            "downgraded env failure must not block: {rendered}"
        );
        assert!(
            rendered.contains("advisory warning"),
            "must render under the advisory section: {rendered}"
        );
        let banner_pos = rendered
            .find("Environment not bootstrapped")
            .expect("banner must render");
        let detail_pos = rendered
            .find("TS2307")
            .expect("original output must render");
        assert!(
            banner_pos < detail_pos,
            "banner must precede the captured TS2307 detail: {rendered}"
        );
    }

    #[test]
    fn no_lock_file_skips_script_gates_without_override() {
        let tmp = setup_package_json(r#""lint":"eslint .","test":"vitest""#);

        let gates = detect_script_gates_inner(&no_overrides(), &tmp, None);
        assert!(
            gates.is_empty(),
            "no gates should be generated without lock file and no overrides"
        );

        let overrides = EnvOverrides {
            lint_cmd: Some("custom-lint".into()),
            ..Default::default()
        };
        let gates = detect_script_gates_inner(&overrides, &tmp, None);
        assert!(gates.iter().any(|g| g.name == "lint"));
        assert_eq!(
            gates.iter().find(|g| g.name == "lint").unwrap().command,
            "custom-lint"
        );
    }

    #[test]
    fn detect_run_prefix_from_lock_files() {
        let tmp = TempDir::new("lock-detect");

        assert!(detect_run_prefix(&tmp).is_none());

        fs::write(tmp.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_run_prefix(&tmp).as_deref(), Some("pnpm run"));
        fs::remove_file(tmp.join("pnpm-lock.yaml")).unwrap();

        fs::write(tmp.join("package-lock.json"), "").unwrap();
        assert_eq!(detect_run_prefix(&tmp).as_deref(), Some("npm run"));
        fs::remove_file(tmp.join("package-lock.json")).unwrap();

        fs::write(tmp.join("bun.lock"), "").unwrap();
        assert_eq!(detect_run_prefix(&tmp).as_deref(), Some("bun run"));
        fs::remove_file(tmp.join("bun.lock")).unwrap();

        fs::write(tmp.join("yarn.lock"), "").unwrap();
        assert_eq!(detect_run_prefix(&tmp).as_deref(), Some("yarn run"));
    }

    #[test]
    fn pnpm_lock_generates_pnpm_commands() {
        let tmp = setup_package_json(r#""lint":"eslint .","test":"vitest""#);
        fs::write(tmp.join("pnpm-lock.yaml"), "").unwrap();
        let gates = detect_script_gates_with_overrides(&no_overrides(), &tmp);
        let lint = gates.iter().find(|g| g.name == "lint").unwrap();
        assert_eq!(lint.command, "pnpm run lint");
    }

    #[test]
    fn gates_skip_when_condition_not_met() {
        for (name, project) in [
            ("knip", test_project(false, false)),
            ("tsgo", test_project(true, false)),
        ] {
            let result = run_gate(gate_by_name(name), &project);
            assert!(result.is_skipped(), "{name} should skip");
        }
    }

    #[test]
    fn missing_command_returns_skipped() {
        let gate = GateDefinition {
            name: "missing",
            command: "nonexistent-command-99999",
            args: &[],
            hint: "",
            condition: |_| true,
        };
        let project = test_project(true, true);
        let result = run_gate(&gate, &project);
        assert!(result.is_skipped());
    }

    #[test]
    fn gate_conditions_are_correct() {
        let pkg_only = test_project(true, false);
        let ts_only = test_project(false, true);

        assert!((gate_by_name("knip").condition)(&pkg_only));
        assert!(!(gate_by_name("knip").condition)(&ts_only));

        assert!(!(gate_by_name("tsgo").condition)(&pkg_only));
        assert!((gate_by_name("tsgo").condition)(&ts_only));
    }

    fn depcruise_project_with(config: Option<&str>) -> (TempDir, ProjectInfo) {
        let tmp = TempDir::new("depcruise");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        if let Some(name) = config {
            fs::write(tmp.join(name), "module.exports = {};").unwrap();
        }
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: false,
            has_tsconfig: false,
        };
        (tmp, project)
    }

    #[test]
    fn depcruise_condition_true_with_js_config() {
        let (_tmp, project) = depcruise_project_with(Some(".dependency-cruiser.js"));
        assert!((gate_by_name("depcruise").condition)(&project));
    }

    #[test]
    fn depcruise_condition_true_with_cjs_config() {
        let (_tmp, project) = depcruise_project_with(Some(".dependency-cruiser.cjs"));
        assert!((gate_by_name("depcruise").condition)(&project));
    }

    #[test]
    fn depcruise_condition_true_with_mjs_config() {
        let (_tmp, project) = depcruise_project_with(Some(".dependency-cruiser.mjs"));
        assert!((gate_by_name("depcruise").condition)(&project));
    }

    #[test]
    fn depcruise_condition_true_with_json_config() {
        let (_tmp, project) = depcruise_project_with(Some(".dependency-cruiser.json"));
        assert!((gate_by_name("depcruise").condition)(&project));
    }

    #[test]
    fn depcruise_condition_false_without_config() {
        let (_tmp, project) = depcruise_project_with(None);
        assert!(!(gate_by_name("depcruise").condition)(&project));
    }

    #[test]
    fn depcruise_skips_without_config() {
        let (_tmp, project) = depcruise_project_with(None);
        let result = run_gate(gate_by_name("depcruise"), &project);
        assert!(result.is_skipped(), "depcruise should skip without config");
    }

    #[test]
    fn depcruise_definition_uses_auto_detect() {
        let gate = gate_by_name("depcruise");
        assert_eq!(gate.command, "dependency-cruiser");
        assert_eq!(gate.args, &["src/"]);
    }

    /// Builds a tsconfig project, optionally planting an executable
    /// `node_modules/.bin/tsgolint` — the signal the oxlint gate uses to decide
    /// whether its `--type-aware` backend can run.
    fn oxlint_project(with_tsgolint: bool) -> (TempDir, ProjectInfo) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("oxlint");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        if with_tsgolint {
            let bin_dir = tmp.join("node_modules/.bin");
            fs::create_dir_all(&bin_dir).unwrap();
            let bin = bin_dir.join("tsgolint");
            fs::write(&bin, "").unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: false,
            has_tsconfig: true,
        };
        (tmp, project)
    }

    #[test]
    fn oxlint_condition_true_with_tsconfig_and_tsgolint() {
        let (_tmp, project) = oxlint_project(true);
        assert!((gate_by_name("oxlint").condition)(&project));
    }

    #[test]
    fn oxlint_condition_false_when_tsgolint_absent() {
        let (_tmp, project) = oxlint_project(false);
        assert!(
            !(gate_by_name("oxlint").condition)(&project),
            "missing tsgolint must keep the gate fail-open (skipped), not false-block"
        );
    }

    #[test]
    fn oxlint_condition_false_without_tsconfig() {
        let (_tmp, mut project) = oxlint_project(true);
        project.has_tsconfig = false;
        assert!(
            !(gate_by_name("oxlint").condition)(&project),
            "tsconfig is required even when tsgolint is present"
        );
    }

    #[test]
    fn oxlint_skips_when_tsgolint_absent() {
        let (_tmp, project) = oxlint_project(false);
        let result = run_gate(gate_by_name("oxlint"), &project);
        assert!(
            result.is_skipped(),
            "tsconfig project without tsgolint must skip, not block"
        );
    }

    #[test]
    fn oxlint_definition_uses_type_aware_without_type_check() {
        // Locks the exit-code contract: --max-warnings 0 makes warnings block, and
        // --type-check is omitted so type errors are not double-reported with tsgo.
        let gate = gate_by_name("oxlint");
        assert_eq!(gate.command, "oxlint");
        assert_eq!(gate.args, &["--type-aware", "--max-warnings", "0"]);
    }

    fn run_circular(project: &ProjectInfo) -> ToolResult {
        run_graph_gates(project, true, false, None)
            .into_iter()
            .next()
            .expect("circular gate enabled yields one result")
    }

    #[test]
    fn circular_skips_without_package_json() {
        let project = test_project(false, false);
        let result = run_circular(&project);
        assert!(result.is_skipped());
    }

    #[test]
    fn circular_skips_without_src_dir() {
        let tmp = TempDir::new("circular-nosrc");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_circular(&project);
        assert!(result.is_skipped());
    }

    #[test]
    fn circular_passes_clean_project() {
        let tmp = TempDir::new("circular-pass");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nexport const a = b + 1;\n",
        )
        .unwrap();
        fs::write(src.join("b.ts"), "export const b = 42;\n").unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_circular(&project);
        assert!(!result.is_failure(), "clean project should pass");
    }

    #[test]
    fn circular_detects_cycle() {
        let tmp = TempDir::new("circular-fail");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            src.join("b.ts"),
            "import { a } from './a';\nexport const b = 2;\n",
        )
        .unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_circular(&project);
        assert!(result.is_failure(), "circular deps should fail");
        let output = result.output();
        assert!(
            output.contains("1 circular dependency"),
            "should show count: {output}"
        );
        assert!(output.contains(" → "), "should show arrow chain: {output}");
    }

    fn run_coupling(project: &ProjectInfo, ca_threshold: Option<usize>) -> ToolResult {
        run_graph_gates(project, false, true, ca_threshold)
            .into_iter()
            .next()
            .expect("coupling gate enabled yields one result")
    }

    fn coupling_project(files: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new("coupling-gate");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        for (name, body) in files {
            fs::write(src.join(name), body).unwrap();
        }
        tmp
    }

    // T-401: coupling skips when package.json is absent (fail-open).
    #[test]
    fn coupling_skips_without_package_json() {
        let project = test_project(false, false);
        let result = run_coupling(&project, Some(2));
        assert!(result.is_skipped());
    }

    // T-402: coupling skips when src/ is absent (fail-open).
    #[test]
    fn coupling_skips_without_src_dir() {
        let tmp = TempDir::new("coupling-nosrc");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_coupling(&project, Some(2));
        assert!(result.is_skipped());
    }

    // T-403: coupling skips when caThreshold is unset.
    #[test]
    fn coupling_skips_without_threshold() {
        let tmp = coupling_project(&[("a.ts", "export const a = 1;\n")]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_coupling(&project, None);
        assert!(result.is_skipped());
    }

    // T-404: a configured threshold with no God module passes.
    #[test]
    fn coupling_passes_under_threshold() {
        let tmp = coupling_project(&[
            ("a.ts", "export const a = 1;\n"),
            ("b.ts", "import { a } from './a';\nexport const b = a;\n"),
        ]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_coupling(&project, Some(2));
        assert!(!result.is_failure(), "Ca=1 under threshold 2 should pass");
        assert!(!result.is_skipped());
    }

    // T-405: a God module above threshold fails with path/Ca/Ce/I in the output.
    #[test]
    fn coupling_detects_god_module() {
        let tmp = coupling_project(&[
            ("a.ts", "export const a = 1;\n"),
            ("b.ts", "import { a } from './a';\nexport const b = a;\n"),
            ("c.ts", "import { a } from './a';\nexport const c = a;\n"),
            ("d.ts", "import { a } from './a';\nexport const d = a;\n"),
        ]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_coupling(&project, Some(2));
        assert!(result.is_failure(), "Ca=3 over threshold 2 should fail");
        let output = result.output();
        assert!(output.contains("god module"), "should label: {output}");
        assert!(output.contains("a.ts"), "should list path: {output}");
        assert!(output.contains("Ca=3"), "should show Ca: {output}");
        assert!(
            output.contains("Ce=") && output.contains("I="),
            "should show Ce and I: {output}"
        );
    }

    #[test]
    fn litmus_skips_without_package_json() {
        let project = test_project(false, false);
        let result = run_litmus(&project);
        assert!(result.iter().any(ToolResult::is_skipped));
    }

    #[test]
    fn litmus_skips_when_no_test_files() {
        let tmp = TempDir::new("litmus-empty");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_litmus(&project);
        assert!(result.iter().any(ToolResult::is_skipped));
    }

    #[test]
    fn litmus_passes_with_good_tests() {
        let tmp = TempDir::new("litmus-good");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(
            tmp.join("src/example.test.ts"),
            r"
import { describe, test, expect } from 'vitest';
describe('math', () => {
    test('adds two numbers correctly', () => {
        const result = add(1, 2);
        expect(result).toBe(3);
    });
});
",
        )
        .unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_litmus(&project);
        assert!(
            !result.iter().any(ToolResult::is_failure),
            "good test should pass: {result:?}"
        );
    }

    #[test]
    fn litmus_detects_tautological_test() {
        let tmp = TempDir::new("litmus-bad");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(
            tmp.join("bad.test.ts"),
            r"
import { test, expect } from 'vitest';
test('works', () => {
    expect(true).toBe(true);
});
",
        )
        .unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_litmus(&project);
        assert!(
            result.iter().any(ToolResult::is_failure),
            "tautological test should fail"
        );
        assert!(
            !result.iter().any(ToolResult::is_warning),
            "no warning tier"
        );
        let blocking_output: String = result
            .iter()
            .filter(|r| r.is_failure())
            .map(ToolResult::output)
            .collect();
        assert!(blocking_output.contains("tautological"));
    }

    #[test]
    fn litmus_warns_without_blocking_on_warning_tier() {
        // dummy-data is a warning-tier rule (advisory, exit 1 in litmus' CLI):
        // the test exercises a real act yet feeds placeholder "foo"/"FOO" data.
        let tmp = TempDir::new("litmus-warn");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(
            tmp.join("warn.test.ts"),
            r#"
import { test, expect } from 'vitest';
test("uppercases the provided first name", () => {
    const result = normalize("foo");
    expect(result).toBe("FOO");
});
"#,
        )
        .unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_litmus(&project);
        assert!(
            result.iter().any(ToolResult::is_warning),
            "warning-tier issue should warn: {result:?}"
        );
        assert!(
            !result.iter().any(ToolResult::is_failure),
            "warning-tier issue must not block: {result:?}"
        );
    }

    #[test]
    fn litmus_reports_both_tiers_when_mixed() {
        // A blocking rule (tautological) and a warning rule (dummy-data) in the
        // same run must both surface; the blocking result must not swallow the
        // warning.
        let tmp = TempDir::new("litmus-mixed");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(
            tmp.join("block.test.ts"),
            r"
import { test, expect } from 'vitest';
test('works', () => {
    expect(true).toBe(true);
});
",
        )
        .unwrap();
        fs::write(
            tmp.join("warn.test.ts"),
            r#"
import { test, expect } from 'vitest';
test("uppercases the provided first name", () => {
    const result = normalize("foo");
    expect(result).toBe("FOO");
});
"#,
        )
        .unwrap();
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_litmus(&project);
        assert!(
            result.iter().any(ToolResult::is_failure),
            "mixed run should keep the blocking result: {result:?}"
        );
        assert!(
            result.iter().any(ToolResult::is_warning),
            "mixed run should keep the warning result: {result:?}"
        );
    }

    fn clone_project(files: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new("clone-gate");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        for (name, body) in files {
            fs::write(src.join(name), body).unwrap();
        }
        tmp
    }

    // A duplicated function spanning 6 lines / >20 AST nodes, copied verbatim.
    const CLONE_BODY: &str = "export function compute(a: number, b: number) {\n  const x = a + b;\n  const y = x * 2;\n  const z = y - 1;\n  const w = z / 3;\n  return w + x;\n}\n";

    // T-621: clone group count reaching block_threshold fails the gate.
    #[test]
    fn clone_blocks_at_threshold() {
        let tmp = clone_project(&[("a.ts", CLONE_BODY), ("b.ts", CLONE_BODY)]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_clone(
            &project,
            clone::DEFAULT_MIN_NODES,
            clone::DEFAULT_MIN_LINES,
            1,
        );
        assert!(
            result.is_failure(),
            "1 clone group at threshold 1 should fail"
        );
        assert!(
            result.output().contains("structural clone group"),
            "report should name clone groups: {}",
            result.output()
        );
    }

    // T-622: a clone count below block_threshold passes the gate.
    #[test]
    fn clone_passes_below_threshold() {
        let tmp = clone_project(&[("a.ts", CLONE_BODY), ("b.ts", CLONE_BODY)]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_clone(
            &project,
            clone::DEFAULT_MIN_NODES,
            clone::DEFAULT_MIN_LINES,
            10,
        );
        assert!(
            !result.is_failure(),
            "clone count under threshold 10 should pass"
        );
        assert!(!result.is_skipped());
    }

    // T-623: declaration files (`*.d.ts`) are excluded from clone analysis, so a
    // duplicate confined to them does not block. With only `.d.ts` inputs the
    // gate has nothing to analyze and skips.
    #[test]
    fn clone_ignores_declaration_files() {
        let tmp = clone_project(&[("a.d.ts", CLONE_BODY), ("b.d.ts", CLONE_BODY)]);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        let result = run_clone(
            &project,
            clone::DEFAULT_MIN_NODES,
            clone::DEFAULT_MIN_LINES,
            1,
        );
        assert!(
            result.is_skipped(),
            "only .d.ts inputs leave nothing to analyze"
        );
    }

    fn jscpd_json(percentage: f64, duplicates: &str) -> String {
        format!(
            r#"{{"statistics":{{"total":{{"percentage":{percentage}}}}},"duplicates":[{duplicates}]}}"#
        )
    }

    // T-631: duplication above threshold without block set → Warned (advisory).
    #[test]
    fn jscpd_warns_above_threshold_without_block() {
        let report = parse_jscpd_report(&jscpd_json(12.0, "")).unwrap();
        let result = jscpd_outcome(&report, 10.0, false);
        assert!(result.is_warning());
        assert!(!result.is_failure());
    }

    // T-632: duplication above threshold with block set → Failed (blocks).
    #[test]
    fn jscpd_fails_above_threshold_with_block() {
        let report = parse_jscpd_report(&jscpd_json(12.0, "")).unwrap();
        let result = jscpd_outcome(&report, 10.0, true);
        assert!(result.is_failure());
        assert!(!result.is_warning());
    }

    // T-633: duplication below threshold → Passed.
    #[test]
    fn jscpd_passes_below_threshold() {
        let report = parse_jscpd_report(&jscpd_json(8.0, "")).unwrap();
        let result = jscpd_outcome(&report, 10.0, false);
        assert!(!result.is_failure());
        assert!(!result.is_warning());
        assert!(!result.is_skipped());
    }

    // T-634: duplication exactly at threshold → Passed (comparison is `>`, not `>=`).
    #[test]
    fn jscpd_passes_at_threshold_boundary() {
        let report = parse_jscpd_report(&jscpd_json(10.0, "")).unwrap();
        let result = jscpd_outcome(&report, 10.0, false);
        assert!(!result.is_warning());
        assert!(!result.is_failure());
    }

    // T-635: malformed JSON yields None rather than panicking.
    #[test]
    fn jscpd_parse_rejects_invalid_json() {
        assert!(parse_jscpd_report("not json{{{").is_none());
    }

    // T-636: the report lists each duplicate file pair with its line span.
    #[test]
    fn jscpd_report_lists_file_pairs() {
        let dup = r#"{"lines":7,"firstFile":{"name":"src/a.ts"},"secondFile":{"name":"src/b.ts"}}"#;
        let report = parse_jscpd_report(&jscpd_json(12.0, dup)).unwrap();
        let result = jscpd_outcome(&report, 10.0, false);
        let output = result.output();
        assert!(output.contains("12%"), "header percentage: {output}");
        assert!(output.contains("src/a.ts"), "first file: {output}");
        assert!(output.contains("src/b.ts"), "second file: {output}");
        assert!(output.contains("7 lines"), "line span: {output}");
    }

    // T-637: jscpd skips a project without package.json (fail-open).
    #[test]
    fn jscpd_skips_without_package_json() {
        let project = test_project(false, false);
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);
        assert!(result.is_skipped());
    }

    /// Install a fake `jscpd` via a committed, exec-only fixture (never written
    /// during the test, so it can't hit the write-then-exec ETXTBSY race — #59).
    /// When `report_body` is non-empty the `jscpd-emit-report` fixture copies it
    /// from a plain data file (`jscpd-report-src.json`, read via cwd = project
    /// root) into the `--output` dir, mirroring how real jscpd emits its JSON.
    /// Empty body uses `jscpd-noop`, which exits 0 without writing a report.
    fn jscpd_project_with_fake_bin(report_body: &str) -> (TempDir, ProjectInfo) {
        let tmp = TempDir::new("jscpd-gate");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let fixture = if report_body.is_empty() {
            "jscpd-noop"
        } else {
            fs::write(tmp.join("jscpd-report-src.json"), report_body).unwrap();
            "jscpd-emit-report"
        };
        link_fake_bin(&tmp, "jscpd", fixture);
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        (tmp, project)
    }

    // T-638: end-to-end — jscpd writes a report above threshold, gate warns.
    #[test]
    fn jscpd_warns_from_written_report() {
        let body = r#"{"statistics":{"total":{"percentage":25.0}},"duplicates":[{"lines":7,"firstFile":{"name":"a.ts"},"secondFile":{"name":"b.ts"}}]}"#;
        let (_tmp, project) = jscpd_project_with_fake_bin(body);
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);
        assert!(result.is_warning(), "25% over threshold 10 should warn");
        assert!(result.output().contains("a.ts"));
    }

    // T-639: a binary that writes no report file skips (fail-open).
    #[test]
    fn jscpd_skips_when_report_missing() {
        let (_tmp, project) = jscpd_project_with_fake_bin("");
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);
        assert!(result.is_skipped(), "missing report should skip, not block");
    }

    // The default ignore list keeps jscpd out of the `.git` directory, whose
    // sample hooks are near-identical and would otherwise surface as clone
    // groups. Verify the glob reaches jscpd's `--ignore` argument end to end.
    #[test]
    fn jscpd_default_ignore_excludes_git_dir() {
        let tmp = TempDir::new("jscpd-args");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        link_fake_bin(&tmp, "jscpd", "jscpd-record-args");
        let project = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };

        let ignore: Vec<String> = DEFAULT_JSCPD_IGNORE
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let _ = run_jscpd(&project, 5, 50, 10.0, false, &ignore);

        let args = fs::read_to_string(tmp.join("jscpd-args.txt")).unwrap();
        let ignore_line = args
            .lines()
            .find(|l| l.contains("**/.git/**") || l.contains(','))
            .unwrap_or("");
        assert!(
            ignore_line.contains("**/.git/**"),
            "jscpd --ignore must exclude the .git dir, got args:\n{args}"
        );
    }
}
