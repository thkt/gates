mod config;
mod input;
mod phase;
mod project;
mod resolve;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

use std::io::IsTerminal;
use std::path::Path;

fn format_failures(failures: &[&tools::ToolResult]) -> String {
    if failures.len() == 1 {
        let f = failures[0];
        let output = f.output();
        let action = if f.hint.is_empty() {
            "Fix the issues:"
        } else {
            f.hint
        };
        return if output.is_empty() {
            format!("{} failed.", f.name)
        } else {
            format!("{} failed. {}\n\n{}", f.name, action, output)
        };
    }

    let sections: Vec<String> = failures
        .iter()
        .map(|f| {
            let output = f.output();
            let hint = if f.hint.is_empty() {
                "Fix the issues:"
            } else {
                f.hint
            };
            if output.is_empty() {
                format!("## {}\n{}", f.name, hint)
            } else {
                format!("## {}\n{}\n\n{}", f.name, hint, output)
            }
        })
        .collect();

    format!(
        "{} gates failed. Fix all issues:\n\n{}",
        failures.len(),
        sections.join("\n\n")
    )
}

#[cfg(test)]
fn run(project_dir: &Path) -> Option<String> {
    run_with_input(project_dir, None)
}

fn run_with_input(project_dir: &Path, hook_input: Option<input::HookInput>) -> Option<String> {
    run_with_input_overrides(project_dir, hook_input, tools::EnvOverrides::from_env())
}

fn run_with_input_overrides(
    project_dir: &Path,
    hook_input: Option<input::HookInput>,
    overrides: tools::EnvOverrides,
) -> Option<String> {
    let config = config::GatesConfig::load(project_dir);
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
        tools::detect_script_gates_with_overrides(&overrides, project_dir)
            .into_iter()
            .filter(|g| config.is_enabled(g.name))
            .collect()
    };

    let total_enabled = enabled.len() + script_gates.len();
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
                std::thread::spawn(move || tools::run_gate(&tools::GATES[idx], &p)),
            )
        })
        .collect();

    let script_gate_names: Vec<&'static str> = script_gates.iter().map(|g| g.name).collect();
    let script_handle = if !script_gates.is_empty() {
        let dir = project_dir.to_path_buf();
        Some(std::thread::spawn(move || {
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

    if results.iter().all(|r| r.is_skipped()) && total_enabled > 0 {
        eprintln!(
            "gates: warning: all {} enabled gates were skipped (binaries not found?)",
            total_enabled
        );
    }

    let failures: Vec<_> = results.iter().filter(|r| r.is_failure()).collect();
    let ran_count = results.iter().filter(|r| !r.is_skipped()).count();

    if ran_count == 0 {
        return None;
    }

    if !failures.is_empty() {
        let reason = phase::build_fix_prompt(&format_failures(&failures));
        let block = serde_json::json!({
            "decision": "block",
            "reason": reason
        });
        return Some(block.to_string());
    }

    // Phase detection only activates when hook input is provided (stop hook mode).
    // Without hook input (CLI mode), all-pass = allow.
    let input = hook_input.as_ref()?;

    let previous = input
        .transcript_path
        .as_deref()
        .map(|p| phase::detect_previous_block(Path::new(p)))
        .unwrap_or(phase::PreviousBlock::NotFound);

    let current_phase = phase::determine_phase(true, &previous, config.review);

    match current_phase {
        phase::Phase::Allow => None,
        phase::Phase::Review => {
            let reason = phase::build_review_prompt();
            let block = serde_json::json!({
                "decision": "block",
                "reason": reason
            });
            Some(block.to_string())
        }
        phase::Phase::Fix => {
            eprintln!("gates: unexpected Fix phase with all_passed=true");
            None
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 {
        eprintln!("usage: gates [project_dir]");
        std::process::exit(1);
    }

    let dir = args.get(1).map(String::as_str).unwrap_or(".");
    let project_dir = Path::new(dir);
    if !project_dir.is_dir() {
        eprintln!("gates: not a directory: {}", project_dir.display());
        std::process::exit(1);
    }

    let hook_input = if std::io::stdin().is_terminal() {
        None
    } else {
        let hi = input::HookInput::from_stdin();
        if hi.stop_hook_active == Some(true) {
            return; // exit 0 immediately
        }
        Some(hi)
    };

    if let Some(json) = run_with_input(project_dir, hook_input) {
        println!("{}", json);
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

    #[test]
    fn format_single_failure_with_output() {
        let r = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed("Unused export: src/foo.ts".into()),
        };
        let result = format_failures(&[&r]);
        assert_eq!(
            result,
            "knip failed. Remove unused exports and dependencies.\n\nUnused export: src/foo.ts"
        );
    }

    #[test]
    fn format_single_failure_without_output() {
        let r = tools::ToolResult {
            name: "knip",
            hint: "Remove unused exports and dependencies.",
            outcome: tools::GateOutcome::Failed(String::new()),
        };
        let result = format_failures(&[&r]);
        assert_eq!(result, "knip failed.");
    }

    #[test]
    fn format_single_failure_fallback_hint() {
        let r = tools::ToolResult {
            name: "custom",
            hint: "",
            outcome: tools::GateOutcome::Failed("error output".into()),
        };
        let result = format_failures(&[&r]);
        assert_eq!(result, "custom failed. Fix the issues:\n\nerror output");
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
        let result = format_failures(&[&r1, &r2]);
        assert_eq!(
            result,
            "2 gates failed. Fix all issues:\n\n\
             ## knip\n\
             Remove unused exports and dependencies.\n\n\
             Unused export\n\n\
             ## tsgo\n\
             Fix type errors.\n\n\
             TS2345: type error"
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
        let result = format_failures(&[&r1, &r2]);
        assert_eq!(
            result,
            "2 gates failed. Fix all issues:\n\n\
             ## knip\n\
             Remove unused exports and dependencies.\n\n\
             ## tsgo\n\
             Fix type errors."
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
        let result = format_failures(&[&r1, &r2]);
        assert_eq!(
            result,
            "2 gates failed. Fix all issues:\n\n\
             ## custom\n\
             Fix the issues:\n\n\
             error\n\n\
             ## knip\n\
             Remove unused exports and dependencies.\n\n\
             Unused export"
        );
    }

    #[test]
    fn no_enabled_gates_returns_none() {
        let tmp = setup_project(r#"{"gates":{}}"#, &["package.json"]);
        assert!(run(&tmp).is_none());
    }

    #[test]
    fn no_config_returns_none() {
        let tmp = TempDir::new("main-noconfig");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        assert!(run(&tmp).is_none());
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

        let result = run(&tmp);
        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        assert!(json["reason"].as_str().unwrap().contains("knip failed"));
    }

    // Phase integration: all pass + no transcript → review block
    #[test]
    fn all_pass_without_transcript_returns_review_block() {
        let tmp = setup_project(r#"{"gates":{"lint":true},"review":true}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        let hook_input = input::HookInput {
            transcript_path: None,
            session_id: None,
            stop_hook_active: None,
        };
        let result = run_with_input_overrides(
            &tmp,
            Some(hook_input),
            tools::EnvOverrides {
                lint_cmd: Some("true".into()),
                ..Default::default()
            },
        );

        assert!(result.is_some(), "should block for review");
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(
            reason.contains(phase::ALL_PASSED_MARKER),
            "should contain all-passed marker"
        );
    }

    // Phase integration: all pass + transcript with AllPassed → allow
    #[test]
    fn all_pass_after_review_allows_completion() {
        let tmp = setup_project(r#"{"gates":{"lint":true},"review":true}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        let transcript = tmp.join("transcript.jsonl");
        fs::write(
            &transcript,
            &format!(
                r#"{{"type":"tool_result","content":"{{\"decision\":\"block\",\"reason\":\"{}\"}}"}}
"#,
                phase::ALL_PASSED_MARKER
            ),
        )
        .unwrap();

        let hook_input = input::HookInput {
            transcript_path: Some(transcript.to_string_lossy().into()),
            session_id: None,
            stop_hook_active: None,
        };
        let result = run_with_input_overrides(
            &tmp,
            Some(hook_input),
            tools::EnvOverrides {
                lint_cmd: Some("true".into()),
                ..Default::default()
            },
        );

        assert!(result.is_none(), "should allow completion after review");
    }

    // CQ-1: $TEST_CMD legacy mode → single gate, script detection skipped
    #[test]
    fn legacy_test_cmd_runs_single_gate() {
        let tmp = setup_project(
            r#"{"gates":{"lint":true,"test":true}}"#,
            &["package.json"],
        );
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint .","test":"vitest"}}"#,
        )
        .unwrap();

        // $TEST_CMD set → single gate mode, lint and test from package.json are skipped
        let result = run_with_input_overrides(
            &tmp,
            None,
            tools::EnvOverrides {
                test_cmd: Some("sh -c 'echo legacy-fail && exit 1'".into()),
                ..Default::default()
            },
        );

        assert!(result.is_some(), "legacy test should block on failure");
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(reason.contains("legacy-fail"), "should contain legacy test output");
        // Should NOT contain lint output (script gates are skipped in legacy mode)
        assert!(
            !reason.contains("lint"),
            "lint should not run in legacy mode"
        );
    }

    // T-025: lint enabled + fails → block
    #[test]
    fn script_gate_lint_failure_blocks() {
        let tmp = setup_project(r#"{"gates":{"lint":true}}"#, &["package.json"]);
        fs::write(
            tmp.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();

        let result = run_with_input_overrides(
            &tmp,
            None,
            tools::EnvOverrides {
                lint_cmd: Some("sh -c 'echo lint-error && exit 1'".into()),
                ..Default::default()
            },
        );

        assert!(result.is_some(), "lint failure should block");
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        let reason = json["reason"].as_str().unwrap();
        assert!(reason.contains("lint"), "reason should mention lint");
        assert!(reason.contains("Banned"), "fix prompt should include footer");
    }
}
