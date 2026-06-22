mod audit;
mod circular;
mod clone;
mod color;
mod config;
mod coupling;
mod depgraph;
mod project;
mod reporter;
mod resolve;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

use std::env;
use std::path::Path;
use std::process;
use std::thread;

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
    let mut lines = vec![String::new()];
    lines.push(color::bold_red(&format!(
        "Gates {}",
        reporter::HEADER_SEPARATOR
    )));

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

    lines.push(color::bold_red(reporter::FOOTER_SEPARATOR));
    lines.push(color::bold_red(&format!(
        "BLOCKED: {} gate{} failed. Fix the source code and retry. Do not circumvent this check.",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    )));

    lines.join("\n")
}

fn run(project_dir: &Path) -> Option<String> {
    run_with_overrides(project_dir, &tools::EnvOverrides::from_env())
}

fn run_with_overrides(project_dir: &Path, overrides: &tools::EnvOverrides) -> Option<String> {
    let config = config::GatesConfig::load(project_dir);

    if should_show_hint(project_dir, &config) {
        eprintln!("{}", CONFIG_HINT);
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

    let litmus_enabled = config.is_enabled("litmus");
    let circular_enabled = config.is_enabled("circular");
    let coupling_enabled = config.is_enabled("coupling");
    let clone_enabled = config.is_enabled("clone");
    let jscpd_enabled = config.is_enabled("jscpd");

    let total_enabled = enabled.len()
        + script_gates.len()
        + usize::from(litmus_enabled)
        + usize::from(circular_enabled)
        + usize::from(coupling_enabled)
        + usize::from(clone_enabled)
        + usize::from(jscpd_enabled);
    if total_enabled == 0 {
        return None;
    }

    let handles: Vec<_> = enabled
        .into_iter()
        .map(|(idx, gate)| {
            let p = project.clone();
            let name = gate.name;
            (
                name,
                thread::spawn(move || tools::run_gate(&tools::GATES[idx], &p)),
            )
        })
        .collect();

    let litmus_handle = if litmus_enabled {
        let p = project.clone();
        Some(thread::spawn(move || tools::run_litmus(&p)))
    } else {
        None
    };

    let ca_threshold = config.coupling_ca_threshold;
    let graph_handle = if circular_enabled || coupling_enabled {
        let p = project.clone();
        Some(thread::spawn(move || {
            tools::run_graph_gates(&p, circular_enabled, coupling_enabled, ca_threshold)
        }))
    } else {
        None
    };

    let clone_handle = if clone_enabled {
        let p = project.clone();
        let min_nodes = config.clone_min_nodes.unwrap_or(clone::DEFAULT_MIN_NODES);
        let min_lines = config.clone_min_lines.unwrap_or(clone::DEFAULT_MIN_LINES);
        let block_threshold = config
            .clone_block_threshold
            .unwrap_or(clone::DEFAULT_BLOCK_THRESHOLD);
        Some(thread::spawn(move || {
            tools::run_clone(&p, min_nodes, min_lines, block_threshold)
        }))
    } else {
        None
    };

    let jscpd_handle = if jscpd_enabled {
        let p = project.clone();
        let min_lines = config
            .jscpd_min_lines
            .unwrap_or(tools::DEFAULT_JSCPD_MIN_LINES);
        let min_tokens = config
            .jscpd_min_tokens
            .unwrap_or(tools::DEFAULT_JSCPD_MIN_TOKENS);
        let threshold = config
            .jscpd_threshold
            .unwrap_or(tools::DEFAULT_JSCPD_THRESHOLD);
        let block = config.jscpd_block.unwrap_or(false);
        let ignore = config.jscpd_ignore.clone().unwrap_or_else(|| {
            tools::DEFAULT_JSCPD_IGNORE
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        });
        Some(thread::spawn(move || {
            tools::run_jscpd(&p, min_lines, min_tokens, threshold, block, &ignore)
        }))
    } else {
        None
    };

    let script_gate_names: Vec<&'static str> = script_gates.iter().map(|g| g.name).collect();
    let script_handle = if !script_gates.is_empty() {
        let dir = project_dir.to_path_buf();
        Some(thread::spawn(move || {
            tools::run_script_gates(&script_gates, &dir)
        }))
    } else {
        None
    };

    let mut results: Vec<_> = handles
        .into_iter()
        .map(|(name, handle)| match handle.join() {
            Ok(result) => result,
            Err(e) => {
                eprintln!("gates: {} thread panicked: {:?}", name, e);
                tools::ToolResult::skipped(name)
            }
        })
        .collect();

    if let Some(h) = litmus_handle {
        match h.join() {
            Ok(result) => results.push(result),
            Err(e) => {
                eprintln!("gates: litmus thread panicked: {e:?}");
                results.push(tools::ToolResult::skipped("litmus"));
            }
        }
    }

    if let Some(h) = graph_handle {
        match h.join() {
            Ok(graph_results) => results.extend(graph_results),
            Err(e) => {
                eprintln!("gates: graph gates thread panicked: {e:?}");
                if circular_enabled {
                    results.push(tools::ToolResult::skipped("circular"));
                }
                if coupling_enabled {
                    results.push(tools::ToolResult::skipped("coupling"));
                }
            }
        }
    }

    if let Some(h) = clone_handle {
        match h.join() {
            Ok(result) => results.push(result),
            Err(e) => {
                eprintln!("gates: clone thread panicked: {e:?}");
                results.push(tools::ToolResult::skipped("clone"));
            }
        }
    }

    if let Some(h) = jscpd_handle {
        match h.join() {
            Ok(result) => results.push(result),
            Err(e) => {
                eprintln!("gates: jscpd thread panicked: {e:?}");
                results.push(tools::ToolResult::skipped("jscpd"));
            }
        }
    }

    if let Some(handle) = script_handle {
        match handle.join() {
            Ok(script_results) => results.extend(script_results),
            Err(e) => {
                eprintln!("gates: script gates thread panicked: {:?}", e);
                for name in &script_gate_names {
                    results.push(tools::ToolResult::skipped(name));
                }
            }
        }
    }

    warn_missing_tools(&results, &project);

    let summary = reporter::format_summary(&results);
    if !summary.is_empty() {
        eprintln!("{summary}");
    }

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

const SHOW_USAGE: &str = "usage: gates show [--last N] [--decision pass|fail]";

/// Parsed `show` options. `None` decision means no filter.
struct ShowArgs {
    last: usize,
    decision: Option<String>,
}

fn parse_show_args(args: &[String]) -> Result<ShowArgs, String> {
    let mut last = 20;
    let mut decision = None;
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
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(ShowArgs { last, decision })
}

fn run_show(args: &[String]) -> i32 {
    let parsed = match parse_show_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gates: {e}\n{SHOW_USAGE}");
            return 1;
        }
    };
    let Some(dir) = audit::default_dir() else {
        return 0;
    };
    let entries = audit::query(&dir, parsed.last, parsed.decision.as_deref());
    if !entries.is_empty() {
        println!("{}", audit::render(&entries));
    }
    0
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.get(1).map(String::as_str) == Some("show") {
        process::exit(run_show(&args[2..]));
    }

    if args.len() > 2 {
        eprintln!("usage: gates [project_dir]");
        process::exit(1);
    }

    let dir = args.get(1).map(String::as_str).unwrap_or(".");
    let project_dir = Path::new(dir);
    if !project_dir.is_dir() {
        eprintln!("gates: not a directory: {}", project_dir.display());
        process::exit(1);
    }

    if let Some(json) = run(project_dir) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_utils::TempDir;

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
        let header = format!("Gates {}", reporter::HEADER_SEPARATOR);
        let blocked = format!(
            "BLOCKED: {} gate{} failed. Fix the source code and retry. Do not circumvent this check.",
            count,
            if count == 1 { "" } else { "s" }
        );
        let mut lines = vec!["", header.as_str()];
        lines.extend_from_slice(gate_lines);
        lines.push(reporter::FOOTER_SEPARATOR);
        lines.push(&blocked);
        lines.join("\n")
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

        let bin_dir = tmp.join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_knip = bin_dir.join("knip");
        fs::write(&fake_knip, "#!/bin/sh\necho 'Unused export' >&2\nexit 1\n").unwrap();

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_knip, fs::Permissions::from_mode(0o755)).unwrap();

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

        let bin_dir = tmp.join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_knip = bin_dir.join("knip");
        fs::write(&fake_knip, "#!/bin/sh\necho 'Unused export' >&2\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_knip, fs::Permissions::from_mode(0o755)).unwrap();

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
}
