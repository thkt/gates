mod audit;
mod circular;
mod clone;
mod color;
mod config;
mod coupling;
mod depgraph;
mod hook_exit;
mod project;
mod reporter;
mod resolve;
mod sanitize;
mod snapshot;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use std::thread;

use hook_exit::HookExitCode;

/// Usage / not-a-directory errors exit with `EX_USAGE` (64) per ADR-0066
/// Group 3 (#18), on both the direct-CLI default path and the `show`
/// subcommand. The hook invocation (`gates`, dir = cwd) never trips these.
fn ex_usage() -> i32 {
    i32::from(HookExitCode::InputError.code())
}

/// sysexits `EX_IOERR`, used when `gates show` fails to write stdout. This is an
/// ADR-0060 I/O code (#14) orthogonal to the Group 3 hook exit semantics.
const EX_IOERR: i32 = 74;

const CONFIG_HINT: &str = "Gates: using defaults. Customize via .claude/tools.json \u{2014} see https://github.com/thkt/gates#configuration";

const BANNED_FOOTER: &str = "\
---\n\
Banned in completion claims: \"should\", \"probably\", \"seems to\", \"I think\", \"looks like\".\n\
Replace with evidence from command output.";

fn build_fix_prompt(failures: &str) -> String {
    format!("{failures}\n\n{BANNED_FOOTER}")
}

fn should_show_hint(project_dir: &Path, config: &config::GatesConfig) -> bool {
    if config.source != config::ConfigSource::Default {
        return false;
    }
    project_dir.join(".claude").is_dir()
}

fn format_failures(failures: &[&tools::ToolResult]) -> String {
    let mut lines = vec![String::new(), reporter::blocked_header()];

    for (i, f) in failures.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.push(color::red(&format!("  \u{2717} {}", f.name)));
        let hint = if f.hint.is_empty() {
            "Fix the issues:"
        } else {
            f.hint
        };
        lines.push(color::red(&format!("    {hint}")));

        let output = f.output();
        if !output.is_empty() {
            lines.push(String::new());
            for line in output.lines() {
                if line.trim().is_empty() {
                    lines.push(String::new());
                } else {
                    lines.push(color::red(&format!("    {line}")));
                }
            }
        }
    }

    lines.push(reporter::blocked_footer(failures.len()));

    lines.join("\n")
}

fn run(project_dir: &Path) -> Option<String> {
    run_with_overrides(project_dir, &tools::EnvOverrides::from_env())
}

/// Write the current fileset digest to the snapshot store. No-op when no
/// snapshot dir is configured (XDG/HOME unset). A write failure reports to
/// stderr and returns without blocking (fail-open); stdout is never touched.
fn record_snapshot(project_root: &Path, overrides: &tools::EnvOverrides) {
    let Some(dir) = overrides.snapshot_dir.as_deref() else {
        return;
    };
    let digest = snapshot::compute_digest(project_root);
    if let Err(e) = snapshot::write(dir, project_root, &digest) {
        eprintln!("gates: snapshot write failed: {e}");
    }
}

/// A spawned gate: its skip-fallback names paired with the join handle. Every
/// gate thread yields `Vec<ToolResult>` (single-result gates a 1-element vec) so
/// one join loop drains them all. The fallback names are the gates emitted as
/// `skipped` if the thread panics.
type GateTask = (
    Vec<&'static str>,
    thread::JoinHandle<Vec<tools::ToolResult>>,
);

/// Join one gate thread into `results`. Owns the panic→`skipped` mapping in one
/// place: a panicked gate degrades to a `skipped` result per fallback name
/// rather than aborting the hook (OUTCOME fail-open constraint).
fn join_into(results: &mut Vec<tools::ToolResult>, (fallback, handle): GateTask) {
    match handle.join() {
        Ok(gate_results) => results.extend(gate_results),
        Err(e) => {
            eprintln!("gates: {} thread panicked: {e:?}", fallback.join("+"));
            results.extend(fallback.into_iter().map(tools::ToolResult::skipped));
        }
    }
}

fn run_with_overrides(project_dir: &Path, overrides: &tools::EnvOverrides) -> Option<String> {
    let config = config::GatesConfig::load(project_dir);

    if should_show_hint(project_dir, &config) {
        eprintln!("{CONFIG_HINT}");
    }

    let project = project::ProjectInfo::detect(project_dir);

    let enabled: Vec<_> = tools::GATES
        .iter()
        .enumerate()
        .filter(|(_, g)| config.is_enabled(g.name))
        .collect();

    // Legacy mode: $TEST_CMD set → single gate, skip script detection
    let script_gates: Vec<_> = if let Some(ref test_cmd) = overrides.test_cmd {
        vec![tools::ScriptGate {
            name: "test",
            command: test_cmd.clone(),
            hint: "Fix test failures.",
        }]
    } else {
        tools::detect_script_gates_with_overrides(overrides, project_dir)
            .into_iter()
            .filter(|g| config.is_enabled(g.name))
            .collect()
    };

    let circular_enabled = config.is_enabled("circular");
    let coupling_enabled = config.is_enabled("coupling");

    let mut tasks: Vec<GateTask> = Vec::new();

    for (idx, gate) in enabled {
        let p = project.clone();
        let name = gate.name;
        tasks.push((
            vec![name],
            thread::spawn(move || vec![tools::run_gate(&tools::GATES[idx], &p)]),
        ));
    }

    if config.is_enabled("litmus") {
        let p = project.clone();
        tasks.push((
            vec!["litmus"],
            thread::spawn(move || vec![tools::run_litmus(&p)]),
        ));
    }

    if circular_enabled || coupling_enabled {
        let p = project.clone();
        let ca_threshold = config.coupling.ca_threshold;
        let mut fallback = Vec::new();
        if circular_enabled {
            fallback.push("circular");
        }
        if coupling_enabled {
            fallback.push("coupling");
        }
        tasks.push((
            fallback,
            thread::spawn(move || {
                tools::run_graph_gates(&p, circular_enabled, coupling_enabled, ca_threshold)
            }),
        ));
    }

    if config.is_enabled("clone") {
        let p = project.clone();
        let min_nodes = config.clone.min_nodes.unwrap_or(clone::DEFAULT_MIN_NODES);
        let min_lines = config.clone.min_lines.unwrap_or(clone::DEFAULT_MIN_LINES);
        let block_threshold = config
            .clone
            .block_threshold
            .unwrap_or(clone::DEFAULT_BLOCK_THRESHOLD);
        tasks.push((
            vec!["clone"],
            thread::spawn(move || {
                vec![tools::run_clone(&p, min_nodes, min_lines, block_threshold)]
            }),
        ));
    }

    if config.is_enabled("jscpd") {
        let p = project.clone();
        let min_lines = config
            .jscpd
            .min_lines
            .unwrap_or(tools::DEFAULT_JSCPD_MIN_LINES);
        let min_tokens = config
            .jscpd
            .min_tokens
            .unwrap_or(tools::DEFAULT_JSCPD_MIN_TOKENS);
        let threshold = config
            .jscpd
            .threshold
            .unwrap_or(tools::DEFAULT_JSCPD_THRESHOLD);
        let block = config.jscpd.block.unwrap_or(false);
        let ignore = config.jscpd.ignore.clone().unwrap_or_else(|| {
            tools::DEFAULT_JSCPD_IGNORE
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        });
        tasks.push((
            vec!["jscpd"],
            thread::spawn(move || {
                vec![tools::run_jscpd(
                    &p, min_lines, min_tokens, threshold, block, &ignore,
                )]
            }),
        ));
    }

    if !script_gates.is_empty() {
        let fallback: Vec<&'static str> = script_gates.iter().map(|g| g.name).collect();
        let dir = project_dir.to_path_buf();
        tasks.push((
            fallback,
            thread::spawn(move || tools::run_script_gates(&script_gates, &dir)),
        ));
    }

    if tasks.is_empty() {
        return None;
    }

    let mut results: Vec<tools::ToolResult> = Vec::new();
    for task in tasks {
        join_into(&mut results, task);
    }

    warn_missing_tools(&results, &project);

    let summary = reporter::format_summary(&results);
    if !summary.is_empty() {
        eprintln!("{summary}");
    }

    // Record the post-gate fileset digest so a following Bash-triggered
    // `gates changed` does not re-detect this same edit (FR-006 double-detection
    // guard). Recording here, after the gates have run, captures the actual
    // on-disk state: a gate that writes a target file (e.g. a project type-check
    // script emitting `.js`) is reflected in the stored digest, so the next
    // unchanged `gates changed` still fast-exits instead of re-triggering forever.
    // Write failures degrade to stderr only (fail-open, never block).
    record_snapshot(&project.root, overrides);

    let failures: Vec<_> = results.iter().filter(|r| r.is_failure()).collect();
    let ran_count = results.iter().filter(|r| !r.is_skipped()).count();

    if ran_count == 0 {
        return None;
    }

    record_audit(project_dir, &failures, overrides);

    if !failures.is_empty() {
        let reason = build_fix_prompt(&format_failures(&failures));
        let block = serde_json::json!({
            "decision": "block",
            "reason": reason
        });
        return Some(block.to_string());
    }

    None
}

/// Append the pass/fail decision to the audit log. Fail-open: any error is
/// reported to stderr but never propagated into the hook control flow.
fn record_audit(
    project_dir: &Path,
    failures: &[&tools::ToolResult],
    overrides: &tools::EnvOverrides,
) {
    let Some(dir) = overrides.audit_dir.as_deref() else {
        return;
    };
    let project = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let entry = audit::AuditEntry {
        ts: audit::now_rfc3339(),
        project: project.to_string_lossy().into_owned(),
        decision: if failures.is_empty() { "pass" } else { "fail" }.to_owned(),
        failed: failures.iter().map(|f| f.name.to_owned()).collect(),
    };
    if let Err(e) = audit::append(dir, &entry) {
        eprintln!("gates: audit log write failed: {e}");
    }
}

fn warn_missing_tools(results: &[tools::ToolResult], project: &project::ProjectInfo) {
    for gate in tools::GATES {
        if !(gate.condition)(project) {
            continue;
        }
        if !results
            .iter()
            .any(|r| r.name == gate.name && r.is_skipped())
        {
            continue;
        }
        if let Some(info) = tools::INSTALL_COMMANDS.iter().find(|i| i.name == gate.name) {
            eprintln!(
                "Gates: {} not installed. Install: {}",
                gate.name, info.install
            );
        } else {
            eprintln!("Gates: {} not installed. Install manually.", gate.name);
        }
    }
}

const SHOW_USAGE: &str = "usage: gates show [--last N] [--decision pass|fail] [--json]";

/// Parsed `show` options. `None` decision means no filter.
struct ShowArgs {
    last: usize,
    decision: Option<String>,
    json: bool,
}

fn parse_show_args(args: &[String]) -> Result<ShowArgs, String> {
    let mut last = 20;
    let mut decision = None;
    let mut json = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--last" => {
                let v = it.next().ok_or("--last requires a value")?;
                last = v
                    .parse()
                    .map_err(|_| format!("invalid --last value: {v}"))?;
            }
            "--decision" => {
                let v = it.next().ok_or("--decision requires a value")?;
                if v != "pass" && v != "fail" {
                    return Err(format!("--decision must be pass or fail, got: {v}"));
                }
                decision = Some(v.clone());
            }
            "--json" => json = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(ShowArgs {
        last,
        decision,
        json,
    })
}

/// Build the `show` stdout payload. `--json` always emits a JSON array (`[]`
/// when empty) so an agent parses one machine-readable shape unconditionally;
/// human mode suppresses output when there is nothing to show.
fn format_show_output(entries: &[audit::AuditEntry], json: bool) -> Option<String> {
    if json {
        Some(serde_json::to_string(entries).expect("AuditEntry serialization is infallible"))
    } else if entries.is_empty() {
        None
    } else {
        Some(audit::render(entries))
    }
}

/// Write one line to stdout, returning an exit code. A closed downstream pipe
/// (`gates show | head`) exits 0 instead of panicking the way `println!` does;
/// any other write error maps to `EX_IOERR`.
fn write_stdout(s: &str) -> i32 {
    match writeln!(io::stdout(), "{s}") {
        Ok(()) => 0,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("gates: stdout write failed: {e}");
            EX_IOERR
        }
    }
}

fn run_show(args: &[String]) -> i32 {
    let parsed = match parse_show_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gates: {e}\n{SHOW_USAGE}");
            return ex_usage();
        }
    };
    // A None dir (neither XDG_DATA_HOME nor HOME set) yields no entries, the
    // same shape as an empty log. Route it through format_show_output so --json
    // still emits `[]` instead of leaking empty stdout past the array contract.
    let entries = match audit::default_dir() {
        Some(dir) => audit::query(&dir, parsed.last, parsed.decision.as_deref()),
        None => Vec::new(),
    };
    match format_show_output(&entries, parsed.json) {
        Some(out) => write_stdout(&out),
        None => 0,
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    process::exit(dispatch(&args));
}

/// Route argv to a subcommand and return the process exit code. Extracted from
/// `main` so every routing arm is unit-testable without `process::exit`; `main`
/// is the only caller that turns this code into an actual exit. The two execution
/// modes are intentionally asymmetric (see `run_changed` and `dispatch_default`).
fn dispatch(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("show") => run_show(&args[2..]),
        Some("changed") => dispatch_changed(args.get(2).map_or(".", String::as_str)),
        _ => dispatch_default(args),
    }
}

/// `gates changed [dir]`: PostToolUse Bash trigger (issue #17). Delta-gated: runs
/// gates only when the fileset changed since the last recorded snapshot, else
/// fast-exits. Asymmetric with the default mode by design (the Bash matcher fires
/// on every shell command, so most invocations have no change to gate).
fn dispatch_changed(dir: &str) -> i32 {
    let project_dir = Path::new(dir);
    if !project_dir.is_dir() {
        eprintln!("gates: not a directory: {}", project_dir.display());
        return ex_usage();
    }
    if let Some(json) = run_changed(project_dir, &tools::EnvOverrides::from_env()) {
        println!("{json}");
    }
    0
}

/// `gates [dir]`: default Write/Edit/MultiEdit trigger. Always runs the gates (no
/// delta gate): those matchers fire only when a tool wrote a file, so a change is
/// implied. The run still records a post-gate snapshot that seeds the `changed`
/// mode's double-detection guard (FR-006), which is why the asymmetry is
/// load-bearing rather than incidental.
fn dispatch_default(args: &[String]) -> i32 {
    if args.len() > 2 {
        eprintln!("usage: gates [project_dir]");
        return ex_usage();
    }
    let dir = args.get(1).map_or(".", String::as_str);
    let project_dir = Path::new(dir);
    if !project_dir.is_dir() {
        eprintln!("gates: not a directory: {}", project_dir.display());
        return ex_usage();
    }
    if let Some(json) = run(project_dir) {
        println!("{json}");
    }
    0
}

/// `PostToolUse` Bash entry (issue #17), invoked as `gates changed`: run gates
/// only when the gated fileset changed since the last recorded snapshot. A
/// matching stored digest means no gated file changed, so the gates are skipped
/// (fast-exit). A missing snapshot dir, a missing/corrupt stored digest, or a
/// mismatch all fall through to a full run (fail-open = run rather than skip). The
/// run records the fresh digest.
fn run_changed(project_dir: &Path, overrides: &tools::EnvOverrides) -> Option<String> {
    let project = project::ProjectInfo::detect(project_dir);
    if let Some(dir) = overrides.snapshot_dir.as_deref() {
        let current = snapshot::compute_digest(&project.root);
        if let Some(stored) = snapshot::read_stored(dir, &project.root)
            && stored == current
        {
            return None;
        }
    }
    run_with_overrides(project_dir, overrides)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_utils::TempDir;

    // A panicked gate thread degrades to one `skipped` result per fallback name
    // (OUTCOME fail-open constraint) instead of unwinding through `join_into`.
    // The multi-name case pins the graph gate's circular+coupling fallback, the
    // data-driven mapping this refactor introduced.
    #[test]
    fn join_into_maps_panic_to_skipped_per_fallback_name() {
        let mut results: Vec<tools::ToolResult> = Vec::new();
        let task: GateTask = (
            vec!["circular", "coupling"],
            thread::spawn(|| panic!("gate thread blew up")),
        );
        join_into(&mut results, task);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(tools::ToolResult::is_skipped));
        assert_eq!(results[0].name, "circular");
        assert_eq!(results[1].name, "coupling");
    }

    fn setup_project(gates_json: &str, files: &[&str]) -> TempDir {
        let tmp = TempDir::new("main");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(tmp.join(".claude/tools.json"), gates_json).unwrap();
        for file in files {
            fs::write(tmp.join(file), "{}").unwrap();
        }
        tmp
    }

    fn expected_block(gate_lines: &[&str], count: usize) -> String {
        // Reference the same skeleton the production path renders, stripped to
        // match the strip_ansi'd actual. Pins format_failures to reporter's
        // byte contract rather than re-declaring the wording here.
        let header = color::strip_ansi(&reporter::blocked_header());
        let footer = color::strip_ansi(&reporter::blocked_footer(count));
        let mut lines = vec!["", header.as_str()];
        lines.extend_from_slice(gate_lines);
        lines.push(&footer);
        lines.join("\n")
    }

    // --- issue #17 `gates changed` integration tests (M2) ---

    // Project with a single failing gate (knip exits 1). Used to make a run
    // observable: if the gate runs, run_changed returns Some(block JSON); if
    // the digest matches and the run is skipped, it returns None.
    fn setup_failing_knip() -> TempDir {
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &["package.json"]);
        test_utils::link_fake_bin(&tmp, "knip", "knip-unused-export");
        tmp
    }

    // T-014: a stored digest matching the current fileset skips the run entirely
    // (fast-exit), so the failing gate never fires and the result is None.
    #[test]
    fn changed_skips_when_unchanged_t014() {
        let tmp = setup_failing_knip();
        let snap_dir = tmp.join("snap");
        let root = project::ProjectInfo::detect(&tmp).root;
        snapshot::write(&snap_dir, &root, &snapshot::compute_digest(&root)).unwrap();

        let result = run_changed(
            &tmp,
            &tools::EnvOverrides {
                snapshot_dir: Some(snap_dir),
                audit_dir: Some(tmp.join("audit")),
                ..Default::default()
            },
        );
        assert!(result.is_none());
    }

    // T-015: a fileset change after the baseline digest was stored makes the run
    // fire (block JSON), and the snapshot is refreshed to the post-run digest.
    #[test]
    fn changed_runs_and_updates_digest_when_changed_t015() {
        let tmp = setup_failing_knip();
        fs::write(tmp.join("a.ts"), "const x = 1;").unwrap();
        let snap_dir = tmp.join("snap");
        let root = project::ProjectInfo::detect(&tmp).root;
        snapshot::write(&snap_dir, &root, &snapshot::compute_digest(&root)).unwrap();

        fs::write(tmp.join("a.ts"), "const x = 1; const y = 2;").unwrap();
        let result = run_changed(
            &tmp,
            &tools::EnvOverrides {
                snapshot_dir: Some(snap_dir.clone()),
                audit_dir: Some(tmp.join("audit")),
                ..Default::default()
            },
        );
        assert!(result.is_some());
        let stored = snapshot::read_stored(&snap_dir, &root).unwrap();
        assert_eq!(stored, snapshot::compute_digest(&root));
    }

    // T-016: with every gate disabled (empty gates map) the run is a no-op and
    // returns None (exit 0). FR-005 step 1 reduces to "no block".
    #[test]
    fn changed_no_op_when_no_enabled_gates_t016() {
        let tmp = setup_project(r#"{"gates":{}}"#, &["package.json"]);
        let result = run_changed(
            &tmp,
            &tools::EnvOverrides {
                snapshot_dir: Some(tmp.join("snap")),
                ..Default::default()
            },
        );
        assert!(result.is_none());
    }

    // T-017: a first run (no stored baseline) with a failing gate emits the
    // block JSON to stdout (returned by run_changed).
    #[test]
    fn changed_emits_block_json_on_failure_t017() {
        let tmp = setup_failing_knip();
        let result = run_changed(
            &tmp,
            &tools::EnvOverrides {
                snapshot_dir: Some(tmp.join("snap")),
                audit_dir: Some(tmp.join("audit")),
                ..Default::default()
            },
        );
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        assert!(json["reason"].as_str().unwrap().contains("\u{2717} knip"));
    }

    // T-019: the FR-006 snapshot side effect must not alter the block output.
    // The same failing-gate scenario produces byte-identical JSON with and
    // without snapshot_dir configured.
    #[test]
    fn gates_dir_output_is_byte_identical_t019() {
        let without = {
            let tmp = setup_failing_knip();
            run_with_overrides(
                &tmp,
                &tools::EnvOverrides {
                    audit_dir: Some(tmp.join("audit")),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let with = {
            let tmp = setup_failing_knip();
            run_with_overrides(
                &tmp,
                &tools::EnvOverrides {
                    snapshot_dir: Some(tmp.join("snap")),
                    audit_dir: Some(tmp.join("audit")),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_eq!(without, with);
    }

    // T-021: end-to-end detection loop. A baseline run records the digest, a
    // no-change run skips, and a `sed -i`-style source edit makes the next run
    // fire again.
    #[test]
    fn changed_fires_after_sed_edit_t021() {
        let tmp = setup_failing_knip();
        fs::write(tmp.join("a.ts"), "const x = 1;").unwrap();
        let overrides = tools::EnvOverrides {
            snapshot_dir: Some(tmp.join("snap")),
            audit_dir: Some(tmp.join("audit")),
            ..Default::default()
        };

        assert!(run_changed(&tmp, &overrides).is_some());
        assert!(run_changed(&tmp, &overrides).is_none());

        fs::write(tmp.join("a.ts"), "const x = 1; // edited").unwrap();
        assert!(run_changed(&tmp, &overrides).is_some());
    }

    // A gate that writes a target file (here a fake knip that emits `out.js`)
    // must not re-trigger forever: the digest is recorded after the gate runs,
    // so it captures the emitted file. The first run fires (no baseline), but a
    // following no-change run must skip. Under a pre-gate digest the emitted
    // file would never match the stored digest and every Bash call would re-run.
    fn setup_emitting_knip() -> TempDir {
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &["package.json"]);
        test_utils::link_fake_bin(&tmp, "knip", "knip-emit");
        tmp
    }

    #[test]
    fn changed_does_not_retrigger_after_gate_emits_target_file_t023() {
        let tmp = setup_emitting_knip();
        let overrides = tools::EnvOverrides {
            snapshot_dir: Some(tmp.join("snap")),
            audit_dir: Some(tmp.join("audit")),
            ..Default::default()
        };

        // First run: no baseline, gate runs and emits out.js, returns block.
        assert!(run_changed(&tmp, &overrides).is_some());
        // The emitted file is part of the recorded digest, so a no-change run skips.
        assert!(run_changed(&tmp, &overrides).is_none());
    }

    // --- #55 dispatch routing tests ---
    // dispatch() is the extracted, exit-free router. These assert the exit code
    // of every error arm without starting the gates, so they stay hermetic (each
    // arm returns before any gate runs).

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    // T-001: `gates show --bogus` routes to run_show and surfaces its usage error.
    #[test]
    fn dispatch_routes_show_usage_error_t001() {
        assert_eq!(dispatch(&argv(&["gates", "show", "--bogus"])), 64);
    }

    // T-002: `gates changed <missing-dir>` routes to the changed arm and rejects a
    // non-directory with EX_USAGE before running gates.
    #[test]
    fn dispatch_changed_rejects_missing_dir_t002() {
        assert_eq!(dispatch(&argv(&["gates", "changed", "/no/such/dir"])), 64);
    }

    // T-003: `gates <missing-dir>` routes to the default arm and rejects a
    // non-directory with EX_USAGE.
    #[test]
    fn dispatch_default_rejects_missing_dir_t003() {
        assert_eq!(dispatch(&argv(&["gates", "/no/such/dir"])), 64);
    }

    // T-004: the default arm rejects more than one positional argument.
    #[test]
    fn dispatch_default_rejects_too_many_args_t004() {
        assert_eq!(dispatch(&argv(&["gates", "a", "b", "c"])), 64);
    }

    // T-005: with no backward-compat alias, the old `gates post-bash` falls through
    // to the default arm and is treated as a directory path, exiting 64.
    #[test]
    fn dispatch_has_no_post_bash_alias_t005() {
        assert_eq!(dispatch(&argv(&["gates", "post-bash"])), 64);
    }

    #[test]
    fn format_single_failure_with_output() {
        let r = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed("Unused export: src/foo.ts".into()),
        };
        let result = color::strip_ansi(&format_failures(&[&r]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} knip",
                    "    Remove unused exports and dependencies.",
                    "",
                    "    Unused export: src/foo.ts",
                ],
                1
            )
        );
    }

    #[test]
    fn format_single_failure_without_output() {
        let r = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed(String::new()),
        };
        let result = color::strip_ansi(&format_failures(&[&r]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} knip",
                    "    Remove unused exports and dependencies.",
                ],
                1
            )
        );
    }

    #[test]
    fn format_single_failure_fallback_hint() {
        let r = tools::ToolResult {
            name: "custom",
            hint: "",
            outcome: tools::GateOutcome::Failed("error output".into()),
        };
        let result = color::strip_ansi(&format_failures(&[&r]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} custom",
                    "    Fix the issues:",
                    "",
                    "    error output",
                ],
                1
            )
        );
    }

    #[test]
    fn format_multiple_failures() {
        let r1 = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed("Unused export".into()),
        };
        let r2 = tools::ToolResult {
            name: "tsgo",
            hint: "Fix type errors.",
            outcome: tools::GateOutcome::Failed("TS2345: type error".into()),
        };
        let result = color::strip_ansi(&format_failures(&[&r1, &r2]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} knip",
                    "    Remove unused exports and dependencies.",
                    "",
                    "    Unused export",
                    "",
                    "  \u{2717} tsgo",
                    "    Fix type errors.",
                    "",
                    "    TS2345: type error",
                ],
                2
            )
        );
    }

    #[test]
    fn format_multiple_failures_without_output() {
        let r1 = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed(String::new()),
        };
        let r2 = tools::ToolResult {
            name: "tsgo",
            hint: "Fix type errors.",
            outcome: tools::GateOutcome::Failed(String::new()),
        };
        let result = color::strip_ansi(&format_failures(&[&r1, &r2]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} knip",
                    "    Remove unused exports and dependencies.",
                    "",
                    "  \u{2717} tsgo",
                    "    Fix type errors.",
                ],
                2
            )
        );
    }

    #[test]
    fn format_multiple_failures_mixed_hints() {
        let r1 = tools::ToolResult {
            name: "custom",
            hint: "",
            outcome: tools::GateOutcome::Failed("error".into()),
        };
        let r2 = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed("Unused export".into()),
        };
        let result = color::strip_ansi(&format_failures(&[&r1, &r2]));
        assert_eq!(
            result,
            expected_block(
                &[
                    "  \u{2717} custom",
                    "    Fix the issues:",
                    "",
                    "    error",
                    "",
                    "  \u{2717} knip",
                    "    Remove unused exports and dependencies.",
                    "",
                    "    Unused export",
                ],
                2
            )
        );
    }

    #[test]
    fn no_enabled_gates_returns_none() {
        let tmp = setup_project(r#"{"gates":{}}"#, &["package.json"]);
        assert!(run(&tmp).is_none());
    }

    #[test]
    fn no_config_all_gates_skipped_returns_none() {
        let tmp = TempDir::new("main-noconfig");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        // No package.json, no tsconfig → all gate conditions fail → all skipped
        assert!(run(&tmp).is_none());
    }

    #[test]
    fn hint_shown_when_default_config_with_claude_dir() {
        let tmp = TempDir::new("hint");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config = config::GatesConfig::default();
        assert!(should_show_hint(&tmp, &config));
    }

    #[test]
    fn hint_not_shown_when_explicit_config() {
        let tmp = TempDir::new("hint");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config = config::GatesConfig {
            source: config::ConfigSource::Explicit,
            ..Default::default()
        };
        assert!(!should_show_hint(&tmp, &config));
    }

    #[test]
    fn hint_not_shown_when_file_exists_without_gates_key() {
        let tmp = TempDir::new("hint");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(tmp.join(".claude/tools.json"), r#"{"review":false}"#).unwrap();
        let config = config::GatesConfig::load(&tmp);
        assert!(!should_show_hint(&tmp, &config));
    }

    #[test]
    fn hint_not_shown_when_no_claude_dir() {
        let tmp = TempDir::new("hint");
        let config = config::GatesConfig::default();
        assert!(!should_show_hint(&tmp, &config));
    }

    #[test]
    fn enabled_gate_missing_command_passes() {
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &["package.json"]);
        assert!(run(&tmp).is_none());
    }

    #[test]
    fn enabled_gate_condition_not_met_passes() {
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &[]);
        assert!(run(&tmp).is_none());
    }

    #[test]
    fn failing_gate_returns_block_json() {
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &["package.json"]);

        test_utils::link_fake_bin(&tmp, "knip", "knip-unused-export");

        let audit_dir = tmp.join("audit");
        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                audit_dir: Some(audit_dir.clone()),
                ..Default::default()
            },
        );
        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        assert!(json["reason"].as_str().unwrap().contains("\u{2717} knip"));

        // The failure is recorded once with decision=fail and the failed gate.
        let logged = audit::query(&audit_dir, 20, None);
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].decision, "fail");
        assert_eq!(logged[0].failed, vec!["knip".to_owned()]);
    }

    #[test]
    fn audit_write_failure_does_not_block_the_hook() {
        // The OUTCOME constraint: a logging failure never propagates into the
        // hook control flow. Point audit_dir under a regular file so
        // create_dir_all fails, then a failing gate must still return its block
        // JSON without panicking.
        let tmp = setup_project(r#"{"gates":{"knip":true}}"#, &["package.json"]);

        test_utils::link_fake_bin(&tmp, "knip", "knip-unused-export");

        // A file where the audit dir's parent should be → create_dir_all errors.
        let blocker = tmp.join("blocker");
        fs::write(&blocker, "").unwrap();
        let audit_dir = blocker.join("audit");

        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                audit_dir: Some(audit_dir),
                ..Default::default()
            },
        );

        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
    }

    #[test]
    fn audit_records_pass_decision() {
        let tmp = setup_project(r#"{"gates":{"lint":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();
        let audit_dir = tmp.join("audit");

        run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                lint_cmd: Some("true".into()),
                audit_dir: Some(audit_dir.clone()),
                ..Default::default()
            },
        );

        let logged = audit::query(&audit_dir, 20, None);
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].decision, "pass");
        assert!(logged[0].failed.is_empty());
    }

    #[test]
    fn show_args_default_to_last_20_no_filter() {
        let p = parse_show_args(&[]).unwrap();
        assert_eq!(p.last, 20);
        assert_eq!(p.decision, None);
        assert!(!p.json);
    }

    #[test]
    fn show_args_parse_json_flag() {
        let p = parse_show_args(&["--json".to_owned()]).unwrap();
        assert!(p.json);
        // --json composes with the existing filters.
        let args = ["--decision", "fail", "--json"].map(str::to_owned);
        let p = parse_show_args(&args).unwrap();
        assert!(p.json);
        assert_eq!(p.decision.as_deref(), Some("fail"));
    }

    fn audit_entry(ts: &str, decision: &str, failed: &[&str]) -> audit::AuditEntry {
        audit::AuditEntry {
            ts: ts.to_owned(),
            project: "/p".to_owned(),
            decision: decision.to_owned(),
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn show_json_emits_parseable_array_round_tripping_entries() {
        let entries = vec![
            audit_entry("2026-04-11T11:00:00Z", "fail", &["lint", "test"]),
            audit_entry("2026-04-11T11:05:30Z", "pass", &[]),
        ];
        let out = format_show_output(&entries, true).unwrap();
        let parsed: Vec<audit::AuditEntry> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, entries);
    }

    #[test]
    fn show_json_empty_emits_empty_array() {
        assert_eq!(format_show_output(&[], true).as_deref(), Some("[]"));
    }

    #[test]
    fn show_human_empty_suppresses_output() {
        assert!(format_show_output(&[], false).is_none());
    }

    #[test]
    fn show_human_non_empty_renders_table() {
        let entries = vec![audit_entry("2026-04-11T11:00:00Z", "fail", &["lint"])];
        let out = format_show_output(&entries, false).unwrap();
        assert!(out.starts_with("TIMESTAMP"));
        assert!(out.contains("lint"));
    }

    #[test]
    fn run_show_rejects_unknown_flag_with_ex_usage() {
        assert_eq!(run_show(&["--bogus".to_owned()]), ex_usage());
    }

    #[test]
    fn show_args_parse_last_and_decision() {
        let args = ["--last", "5", "--decision", "fail"].map(str::to_owned);
        let p = parse_show_args(&args).unwrap();
        assert_eq!(p.last, 5);
        assert_eq!(p.decision.as_deref(), Some("fail"));
    }

    #[test]
    fn show_args_reject_invalid_decision() {
        let args = ["--decision", "maybe"].map(str::to_owned);
        assert!(parse_show_args(&args).is_err());
    }

    #[test]
    fn show_args_reject_non_numeric_last() {
        let args = ["--last", "x"].map(str::to_owned);
        assert!(parse_show_args(&args).is_err());
    }

    #[test]
    fn show_args_reject_unknown_flag() {
        let args = ["--bogus".to_owned()];
        assert!(parse_show_args(&args).is_err());
    }

    #[test]
    fn audit_not_written_when_no_gate_runs() {
        let tmp = setup_project(r#"{"gates":{}}"#, &["package.json"]);
        let audit_dir = tmp.join("audit");

        run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                audit_dir: Some(audit_dir.clone()),
                ..Default::default()
            },
        );

        assert!(audit::query(&audit_dir, 20, None).is_empty());
    }

    #[test]
    fn all_pass_allows_completion() {
        let tmp = setup_project(r#"{"gates":{"lint":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                lint_cmd: Some("true".into()),
                ..Default::default()
            },
        );

        assert!(
            result.is_none(),
            "should allow completion when all gates pass"
        );
    }

    #[test]
    fn legacy_test_cmd_runs_single_gate() {
        let tmp = setup_project(r#"{"gates":{"lint":true,"test":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint .","test":"vitest"}}"#,
        )
        .unwrap();

        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                test_cmd: Some("sh -c 'echo legacy-fail && exit 1'".into()),
                ..Default::default()
            },
        );

        assert!(result.is_some(), "legacy test should block on failure");
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(
            reason.contains("legacy-fail"),
            "should contain legacy test output"
        );
        assert!(
            !reason.contains("lint"),
            "lint should not run in legacy mode"
        );
    }

    #[test]
    fn script_gate_lint_failure_blocks() {
        let tmp = setup_project(r#"{"gates":{"lint":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                lint_cmd: Some("sh -c 'echo lint-error && exit 1'".into()),
                ..Default::default()
            },
        );

        assert!(result.is_some(), "lint failure should block");
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(reason.contains("lint"), "reason should mention lint");
        assert!(
            reason.contains("Banned"),
            "fix prompt should include footer"
        );
    }

    // T-111: when two gates fail concurrently (knip on its own thread, lint on
    // the script-gate thread), the decision surface stays exit 0 + stdout JSON:
    // run returns Some(block JSON) listing both failed gates. Group 3 maps a
    // blocking decision to HookExitCode::Blocking (2) at the type level, but the
    // hook wrapper keeps exit 0 — so the testable surface is the returned JSON,
    // not a process exit code (#18).
    #[test]
    fn parallel_gate_failures_return_block_json_listing_both() {
        let tmp = setup_project(r#"{"gates":{"knip":true,"lint":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        test_utils::link_fake_bin(&tmp, "knip", "knip-unused-export");

        let result = run_with_overrides(
            &tmp,
            &tools::EnvOverrides {
                lint_cmd: Some("sh -c 'echo lint-error && exit 1'".into()),
                ..Default::default()
            },
        );

        assert!(
            result.is_some(),
            "parallel failures should block (exit 0 + JSON)"
        );
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(
            reason.contains("knip"),
            "reason should list the failed knip gate"
        );
        assert!(
            reason.contains("lint"),
            "reason should list the failed lint gate"
        );
    }

    // T-110 companion: usage errors exit with EX_USAGE (64) per Group 3, the
    // same code the show path returns. Guards the ex_usage() helper that the
    // three main() usage/not-a-dir paths share.
    #[test]
    fn usage_errors_use_ex_usage_64() {
        assert_eq!(ex_usage(), 64);
    }
}
