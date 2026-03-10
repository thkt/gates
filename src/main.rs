mod config;
mod project;
mod resolve;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod tools;
mod traverse;

use std::path::Path;

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

    let first_failure = results.iter().find(|r| r.is_failure())?;

    let output = first_failure.output();
    let reason = if output.is_empty() {
        format!("{} failed.", first_failure.name)
    } else {
        format!(
            "{} failed. Fix the issues:\n{}",
            first_failure.name, output
        )
    };

    let block = serde_json::json!({
        "decision": "block",
        "reason": reason
    });

    Some(block.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: gates <project_dir>");
        std::process::exit(1);
    }

    let project_dir = Path::new(&args[1]);
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
