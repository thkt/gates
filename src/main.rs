mod config;
mod project;
mod resolve;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

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

fn run(project_dir: &Path) -> Option<String> {
    let config = config::GatesConfig::load(project_dir);
    let project = project::ProjectInfo::detect(project_dir);

    let enabled: Vec<_> = tools::GATES
        .iter()
        .enumerate()
        .filter(|(_, g)| config.is_enabled(g.name))
        .collect();

    if enabled.is_empty() {
        return None;
    }

    let enabled_count = enabled.len();

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

    let results: Vec<_> = handles
        .into_iter()
        .map(|(name, handle)| match handle.join() {
            Ok(result) => result,
            Err(e) => {
                eprintln!("gates: {} thread panicked: {:?}", name, e);
                tools::ToolResult::skipped(name)
            }
        })
        .collect();

    if results.iter().all(|r| r.is_skipped()) {
        eprintln!(
            "gates: warning: all {} enabled gates were skipped (binaries not found?)",
            enabled_count
        );
    }

    let failures: Vec<_> = results.iter().filter(|r| r.is_failure()).collect();
    if failures.is_empty() {
        return None;
    }

    let reason = format_failures(&failures);
    let block = serde_json::json!({
        "decision": "block",
        "reason": reason
    });

    Some(block.to_string())
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
}
