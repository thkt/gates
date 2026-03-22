mod circular;
mod color;
mod config;
mod project;
mod reporter;
mod resolve;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

use std::path::Path;

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

fn hint_or_default(hint: &str) -> &str {
    if hint.is_empty() {
        "Fix the issues:"
    } else {
        hint
    }
}

fn format_failures(failures: &[&tools::ToolResult]) -> String {
    if failures.len() == 1 {
        let f = failures[0];
        let output = f.output();
        let action = hint_or_default(f.hint);
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
            let hint = hint_or_default(f.hint);
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

fn run(project_dir: &Path) -> Option<String> {
    run_with_overrides(project_dir, tools::EnvOverrides::from_env())
}

fn run_with_overrides(project_dir: &Path, overrides: tools::EnvOverrides) -> Option<String> {
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
        tools::detect_script_gates_with_overrides(&overrides, project_dir)
            .into_iter()
            .filter(|g| config.is_enabled(g.name))
            .collect()
    };

    let litmus_enabled = config.is_enabled("litmus");
    let circular_enabled = config.is_enabled("circular");

    let total_enabled = enabled.len()
        + script_gates.len()
        + usize::from(litmus_enabled)
        + usize::from(circular_enabled);
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

    let litmus_handle = if litmus_enabled {
        let p = project.clone();
        Some(std::thread::spawn(move || tools::run_litmus(&p)))
    } else {
        None
    };

    let circular_handle = if circular_enabled {
        let p = project.clone();
        Some(std::thread::spawn(move || tools::run_circular(&p)))
    } else {
        None
    };

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

    for (name, handle) in [("litmus", litmus_handle), ("circular", circular_handle)] {
        if let Some(h) = handle {
            match h.join() {
                Ok(result) => results.push(result),
                Err(e) => {
                    eprintln!("gates: {name} thread panicked: {e:?}");
                    results.push(tools::ToolResult::skipped(name));
                }
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

    if let Some(json) = run(project_dir) {
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

        let result = run(&tmp);
        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
        assert!(json["reason"].as_str().unwrap().contains("knip failed"));
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
            tools::EnvOverrides {
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
            tools::EnvOverrides {
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
        assert!(
            reason.contains("Banned"),
            "fix prompt should include footer"
        );
    }
}
