use crate::audit;
use crate::circular;
use crate::clone;
use crate::coupling;
use crate::depgraph;
use crate::project::ProjectInfo;
use crate::resolve;
use crate::sanitize;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::iter;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const GATE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT_LINES: usize = 50;

#[derive(Debug)]
pub enum GateOutcome {
    Passed,
    Failed(String),
    Skipped,
    /// Advisory failure: reported to the human via stderr but not promoted to a
    /// block decision, so the AI is not forced to act. Used by warn-level gates.
    Warned(String),
}

#[derive(Debug)]
pub struct ToolResult {
    pub name: &'static str,
    pub hint: &'static str,
    pub outcome: GateOutcome,
}

impl ToolResult {
    pub fn skipped(name: &'static str) -> Self {
        Self {
            name,
            hint: "",
            outcome: GateOutcome::Skipped,
        }
    }

    pub fn passed(name: &'static str) -> Self {
        Self {
            name,
            hint: "",
            outcome: GateOutcome::Passed,
        }
    }

    /// Build a Failed result whose output is truncated to `MAX_OUTPUT_LINES`,
    /// the shared truncation policy for embedded gates.
    ///
    /// `run_command_with_label` does not use this: external command output also
    /// needs `sanitize` + `trim` and may resolve to `Passed`, so it assembles
    /// its outcome inline.
    pub fn failed(name: &'static str, hint: &'static str, text: &str) -> Self {
        Self {
            name,
            hint,
            outcome: GateOutcome::Failed(sanitize::tail_lines(text, MAX_OUTPUT_LINES)),
        }
    }

    /// Build a Warned result: the advisory counterpart of `failed`, sharing the
    /// same truncation policy. Reported to the human but never blocks the AI.
    pub fn warned(name: &'static str, hint: &'static str, text: &str) -> Self {
        Self {
            name,
            hint,
            outcome: GateOutcome::Warned(sanitize::tail_lines(text, MAX_OUTPUT_LINES)),
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self.outcome, GateOutcome::Failed(_))
    }

    pub fn is_warning(&self) -> bool {
        matches!(self.outcome, GateOutcome::Warned(_))
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self.outcome, GateOutcome::Skipped)
    }

    pub fn output(&self) -> &str {
        match &self.outcome {
            GateOutcome::Failed(s) | GateOutcome::Warned(s) => s,
            GateOutcome::Passed | GateOutcome::Skipped => "",
        }
    }
}

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
        Self {
            lint_cmd: env::var("LINT_CMD").ok().filter(|s| !s.is_empty()),
            type_cmd: env::var("TYPE_CMD").ok().filter(|s| !s.is_empty()),
            unit_cmd: env::var("UNIT_CMD").ok().filter(|s| !s.is_empty()),
            test_cmd: env::var("TEST_CMD").ok().filter(|s| !s.is_empty()),
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
        thread::spawn(move || run_shell_command("lint", &cmd_str, hint, &dir))
    });

    if let Some(tc) = type_check {
        let tc_result = run_shell_command("type-check", &tc.command, tc.hint, project_dir);
        let type_failed = tc_result.is_failure();
        results.push(tc_result);

        if let Some(t) = test {
            if type_failed {
                results.push(ToolResult::skipped("test"));
            } else {
                results.push(run_shell_command("test", &t.command, t.hint, project_dir));
            }
        }
    } else if let Some(t) = test {
        results.push(run_shell_command("test", &t.command, t.hint, project_dir));
    }

    if let Some(handle) = lint_handle {
        match handle.join() {
            Ok(r) => results.push(r),
            Err(e) => {
                eprintln!("gates: lint thread panicked: {e:?}");
                results.push(ToolResult::skipped("lint"));
            }
        }
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

fn kill_process_group(pid: u32) {
    if pid == 0 {
        eprintln!("gates: pid 0, refusing to kill own process group");
        return;
    }
    let Ok(pid_i32) = i32::try_from(pid) else {
        eprintln!("gates: pid {pid} exceeds i32::MAX, cannot kill process group");
        return;
    };
    let target = format!("-{pid_i32}");
    let status = Command::new("kill")
        .args(["-9", &target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if !s.success() => {
            eprintln!("gates: kill exited {s} for process group {pid}");
        }
        Err(e) => {
            eprintln!("gates: failed to kill process group {pid}: {e}");
        }
        _ => {}
    }
}

fn run_command(name: &'static str, cmd: Command, timeout: Duration) -> ToolResult {
    run_command_with_label(name, cmd, timeout, None)
}

fn run_command_with_label(
    name: &'static str,
    mut cmd: Command,
    timeout: Duration,
    label: Option<&str>,
) -> ToolResult {
    cmd.process_group(0);

    let child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            match e.kind() {
                io::ErrorKind::NotFound => {}
                io::ErrorKind::PermissionDenied => {
                    eprintln!("gates: {name} binary found but not executable: {e}");
                }
                _ => {
                    eprintln!("gates: {name} spawn error: {e}");
                }
            }
            return ToolResult::skipped(name);
        }
    };

    let pid = child.id();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stderr.is_empty() {
                stdout.into_owned()
            } else if stdout.is_empty() {
                stderr.into_owned()
            } else {
                format!("{stdout}\n{stderr}")
            };
            let sanitized = sanitize::sanitize(&combined);
            let truncated = sanitize::tail_lines(&sanitized, MAX_OUTPUT_LINES);
            let text = truncated.trim().to_owned();

            let outcome = if output.status.success() {
                GateOutcome::Passed
            } else {
                GateOutcome::Failed(text)
            };
            ToolResult {
                name,
                hint: "",
                outcome,
            }
        }
        Ok(Err(e)) => {
            eprintln!("gates: {name} output read error: {e}");
            ToolResult::skipped(name)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Some(l) = label {
                eprintln!(
                    "gates: {} timed out after {}s (cmd: {})",
                    name,
                    timeout.as_secs(),
                    l
                );
            } else {
                eprintln!("gates: {} timed out after {}s", name, timeout.as_secs());
            }
            kill_process_group(pid);
            let _ = rx.recv_timeout(Duration::from_secs(2));
            ToolResult::skipped(name)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("gates: {name} wait thread disconnected");
            ToolResult::skipped(name)
        }
    }
}

pub fn run_litmus(project: &ProjectInfo) -> ToolResult {
    if !project.has_package_json {
        return ToolResult::skipped("litmus");
    }

    let files = litmus::find_test_files(&project.root);
    if files.is_empty() {
        return ToolResult::skipped("litmus");
    }

    let result = litmus::analyze_files(&files);

    for error in &result.errors {
        eprintln!("gates: {error}");
    }

    if result.issues.is_empty() {
        return ToolResult::passed("litmus");
    }

    let output: Vec<String> = result.issues.iter().map(ToString::to_string).collect();

    ToolResult::failed(
        "litmus",
        "Fix test quality issues (weak assertions, mock overuse, tautological tests).",
        &output.join("\n"),
    )
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
pub const DEFAULT_JSCPD_IGNORE: &[&str] = &[
    "**/node_modules/**",
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

    let out_dir = env::temp_dir().join(format!("gates-jscpd-{}", process::id()));
    // Clear any report left by a prior run whose cleanup failed before this PID
    // was reused, so a stale report is never read as the current run's result.
    let _ = fs::remove_dir_all(&out_dir);
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("gates: jscpd temp dir create failed: {e}");
        return ToolResult::skipped("jscpd");
    }

    let bin = resolve::resolve_bin("jscpd", &project.root);
    let mut cmd = Command::new(&bin);
    cmd.arg(&project.root)
        .args(["--reporters", "json"])
        .arg("--output")
        .arg(&out_dir)
        .arg("--silent")
        .args(["--min-lines", &min_lines.to_string()])
        .args(["--min-tokens", &min_tokens.to_string()]);
    if !ignore.is_empty() {
        cmd.args(["--ignore", &ignore.join(",")]);
    }
    cmd.current_dir(&project.root);

    let result = run_command("jscpd", cmd, GATE_TIMEOUT);
    let outcome = if result.is_skipped() {
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
    };

    if let Err(e) = fs::remove_dir_all(&out_dir) {
        eprintln!("gates: jscpd temp dir cleanup failed: {e}");
    }
    outcome
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
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{TempDir, link_fake_bin};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError};

    /// run_jscpd derives its temp output dir from the process PID, so the three
    /// end-to-end tests below share one dir. Serialize them so one test's report
    /// is never read (or wiped) by another running concurrently.
    static JSCPD_RUN_LOCK: Mutex<()> = Mutex::new(());

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
    fn skipped_result() {
        let r = ToolResult::skipped("test");
        assert!(r.is_skipped());
        assert!(!r.is_failure());
        assert!(r.output().is_empty());
    }

    // T-601: a Warned outcome is not a failure (stays out of the block decision).
    #[test]
    fn warned_is_not_failure() {
        let r = ToolResult::warned("jscpd", "hint", "dup");
        assert!(!r.is_failure());
    }

    // T-602: a Warned outcome counts as ran, not skipped.
    #[test]
    fn warned_is_not_skipped() {
        let r = ToolResult::warned("jscpd", "hint", "dup");
        assert!(!r.is_skipped());
        assert!(r.is_warning());
    }

    // T-603: a Warned outcome exposes its text via output().
    #[test]
    fn warned_exposes_output() {
        let r = ToolResult::warned("jscpd", "hint", "duplication detail");
        assert_eq!(r.output(), "duplication detail");
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
        assert!(result.is_skipped());
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
        assert!(result.is_skipped());
    }

    #[test]
    fn litmus_passes_with_good_tests() {
        let tmp = TempDir::new("litmus-good");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(
            tmp.join("src/example.test.ts"),
            r#"
import { describe, test, expect } from 'vitest';
describe('math', () => {
    test('adds two numbers correctly', () => {
        const result = add(1, 2);
        expect(result).toBe(3);
    });
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
            !result.is_failure(),
            "good test should pass: {:?}",
            result.outcome
        );
    }

    #[test]
    fn litmus_detects_tautological_test() {
        let tmp = TempDir::new("litmus-bad");
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(
            tmp.join("bad.test.ts"),
            r#"
import { test, expect } from 'vitest';
test('works', () => {
    expect(true).toBe(true);
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
        assert!(result.is_failure(), "tautological test should fail");
        assert!(result.output().contains("tautological"));
    }

    #[test]
    fn command_success() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let result = run_command("echo-test", cmd, Duration::from_secs(5));
        assert!(matches!(result.outcome, GateOutcome::Passed));
    }

    #[test]
    fn command_failure() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo fail >&2; exit 1"]);
        let result = run_command("fail-test", cmd, Duration::from_secs(5));
        assert!(result.is_failure());
        assert!(result.output().contains("fail"));
    }

    #[test]
    fn timeout_returns_skipped() {
        let mut cmd = Command::new("sleep");
        cmd.arg("120");
        let result = run_command("sleep-test", cmd, Duration::from_millis(200));
        assert!(result.is_skipped());
    }

    #[test]
    fn spawn_error_returns_skipped() {
        let cmd = Command::new("nonexistent-binary-99999");
        let result = run_command("missing", cmd, Duration::from_secs(5));
        assert!(result.is_skipped());
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
        let _guard = JSCPD_RUN_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let body = r#"{"statistics":{"total":{"percentage":25.0}},"duplicates":[{"lines":7,"firstFile":{"name":"a.ts"},"secondFile":{"name":"b.ts"}}]}"#;
        let (_tmp, project) = jscpd_project_with_fake_bin(body);
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);
        assert!(result.is_warning(), "25% over threshold 10 should warn");
        assert!(result.output().contains("a.ts"));
    }

    // T-639: a binary that writes no report file skips (fail-open).
    #[test]
    fn jscpd_skips_when_report_missing() {
        let _guard = JSCPD_RUN_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (_tmp, project) = jscpd_project_with_fake_bin("");
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);
        assert!(result.is_skipped(), "missing report should skip, not block");
    }

    // T-640: a stale report left in the temp dir (prior run's cleanup failed,
    // then PID reused) is not read as the current run's result. run_jscpd must
    // start from a clean output dir.
    #[test]
    fn jscpd_ignores_stale_report_from_prior_run() {
        let _guard = JSCPD_RUN_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let out_dir = env::temp_dir().join(format!("gates-jscpd-{}", process::id()));
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(
            out_dir.join("jscpd-report.json"),
            r#"{"statistics":{"total":{"percentage":99.0}},"duplicates":[]}"#,
        )
        .unwrap();

        // The current run's binary writes no report (finds nothing / fails to emit).
        let (_tmp, project) = jscpd_project_with_fake_bin("");
        let result = run_jscpd(&project, 5, 50, 10.0, false, &[]);

        assert!(
            result.is_skipped(),
            "stale report must not be read as the current run: {}",
            result.output()
        );
    }
}
