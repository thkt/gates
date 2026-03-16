use crate::project::ProjectInfo;
use crate::resolve;
use crate::sanitize;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const GATE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT_LINES: usize = 50;

#[derive(Debug)]
pub enum GateOutcome {
    Passed,
    Failed(String),
    Skipped,
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

    pub fn is_failure(&self) -> bool {
        matches!(self.outcome, GateOutcome::Failed(_))
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self.outcome, GateOutcome::Skipped)
    }

    pub fn output(&self) -> &str {
        match &self.outcome {
            GateOutcome::Failed(s) => s,
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
    GateDefinition {
        name: "madge",
        command: "madge",
        args: &["--circular", "--extensions", "ts,tsx", "src/"],
        hint: "Break circular import dependencies.",
        condition: |p| p.has_package_json && p.root.join("src").is_dir(),
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
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        Self {
            lint_cmd: std::env::var("LINT_CMD").ok().filter(|s| !s.is_empty()),
            type_cmd: std::env::var("TYPE_CMD").ok().filter(|s| !s.is_empty()),
            unit_cmd: std::env::var("UNIT_CMD").ok().filter(|s| !s.is_empty()),
            test_cmd: std::env::var("TEST_CMD").ok().filter(|s| !s.is_empty()),
        }
    }
}

fn has_nr_in_path(path_override: &str) -> bool {
    if path_override.is_empty() {
        std::process::Command::new("nr")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    } else {
        std::path::Path::new(path_override).join("nr").is_file()
    }
}

pub fn detect_script_gates_with_overrides(
    overrides: &EnvOverrides,
    project_dir: &std::path::Path,
) -> Vec<ScriptGate> {
    let nr_available = has_nr_in_path("");
    detect_script_gates_inner(overrides, project_dir, nr_available)
}

fn detect_script_gates_inner(
    overrides: &EnvOverrides,
    project_dir: &std::path::Path,
    nr_available: bool,
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
    } else if nr_available && scripts.contains("lint") {
        gates.push(ScriptGate {
            name: "lint",
            command: "nr lint".into(),
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
    } else if nr_available && scripts.contains("test:type") {
        gates.push(ScriptGate {
            name: "type-check",
            command: "nr test:type".into(),
            hint: "Fix type errors.",
        });
        true
    } else if nr_available && scripts.contains("typecheck") {
        gates.push(ScriptGate {
            name: "type-check",
            command: "nr typecheck".into(),
            hint: "Fix type errors.",
        });
        true
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
    } else if !nr_available {
    } else if scripts.contains("test:unit") {
        gates.push(ScriptGate {
            name: "test",
            command: "nr test:unit".into(),
            hint: "Fix test failures.",
        });
    } else if !has_type_check && scripts.contains("test") {
        gates.push(ScriptGate {
            name: "test",
            command: "nr test".into(),
            hint: "Fix test failures.",
        });
    }

    gates
}

/// Run script gates with type-check → test cascade logic.
/// lint runs independently; if type-check fails, test is skipped.
pub fn run_script_gates(gates: &[ScriptGate], project_dir: &std::path::Path) -> Vec<ToolResult> {
    let mut results = Vec::new();

    let lint = gates.iter().find(|g| g.name == "lint");
    let type_check = gates.iter().find(|g| g.name == "type-check");
    let test = gates.iter().find(|g| g.name == "test");

    let lint_handle = lint.map(|g| {
        let cmd_str = g.command.clone();
        let hint = g.hint;
        let dir = project_dir.to_path_buf();
        std::thread::spawn(move || run_shell_command("lint", &cmd_str, hint, &dir))
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
                eprintln!("gates: lint thread panicked: {:?}", e);
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
    project_dir: &std::path::Path,
) -> ToolResult {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", cmd_str]).current_dir(project_dir);
    let label = cmd_str.to_string();
    let mut result = run_command_with_label(name, cmd, GATE_TIMEOUT, Some(&label));
    result.hint = hint;
    result
}

fn read_package_scripts(project_dir: &std::path::Path) -> std::collections::HashSet<String> {
    let path = project_dir.join("package.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return std::collections::HashSet::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return std::collections::HashSet::new();
    };
    let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) else {
        return std::collections::HashSet::new();
    };
    scripts.keys().cloned().collect()
}

#[cfg(test)]
pub fn gate_by_name(name: &str) -> &'static GateDefinition {
    GATES
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("gate '{}' not found", name))
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn kill_process_group(pid: u32) {
    if pid == 0 {
        eprintln!("gates: pid 0, refusing to kill own process group");
        return;
    }
    let Ok(pid_i32) = i32::try_from(pid) else {
        eprintln!(
            "gates: pid {} exceeds i32::MAX, cannot kill process group",
            pid
        );
        return;
    };
    // SAFETY: pid_i32 > 0 validated above; -pid targets the process group.
    let ret = unsafe { kill(-pid_i32, 9) };
    if ret != 0 {
        eprintln!(
            "gates: failed to kill process group {}: {}",
            pid,
            std::io::Error::last_os_error()
        );
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
                std::io::ErrorKind::NotFound => {}
                std::io::ErrorKind::PermissionDenied => {
                    eprintln!("gates: {} binary found but not executable: {}", name, e);
                }
                _ => {
                    eprintln!("gates: {} spawn error: {}", name, e);
                }
            }
            return ToolResult::skipped(name);
        }
    };

    let pid = child.id();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
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
                format!("{}\n{}", stdout, stderr)
            };
            let sanitized = sanitize::sanitize(&combined);
            let truncated = sanitize::tail_lines(&sanitized, MAX_OUTPUT_LINES);
            let text = truncated.trim().to_string();

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
            eprintln!("gates: {} output read error: {}", name, e);
            ToolResult::skipped(name)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Some(l) = label {
                eprintln!("gates: {} timed out after {}s (cmd: {})", name, timeout.as_secs(), l);
            } else {
                eprintln!("gates: {} timed out after {}s", name, timeout.as_secs());
            }
            kill_process_group(pid);
            let _ = rx.recv_timeout(Duration::from_secs(2));
            ToolResult::skipped(name)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("gates: {} wait thread disconnected", name);
            ToolResult::skipped(name)
        }
    }
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
    use crate::test_utils::TempDir;
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

    // T-001: lint script あり → lint gate 生成
    #[test]
    fn detect_lint_gate_when_script_exists() {
        let tmp = setup_package_json(r#""lint":"eslint .""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].name, "lint");
    }

    // T-002: lint script なし → lint gate なし
    #[test]
    fn no_lint_gate_when_no_script() {
        let tmp = setup_package_json(r#""test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        assert!(gates.iter().all(|g| g.name != "lint"));
    }

    // T-003: test:type script → type-check gate
    #[test]
    fn detect_type_check_gate_test_type() {
        let tmp = setup_package_json(r#""test:type":"tsc --noEmit""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        assert!(gates.iter().any(|g| g.name == "type-check"));
    }

    // T-004: typecheck script → type-check gate
    #[test]
    fn detect_type_check_gate_typecheck() {
        let tmp = setup_package_json(r#""typecheck":"tsc --noEmit""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        assert!(gates.iter().any(|g| g.name == "type-check"));
    }

    // T-005: test:unit + type-check → test = "nr test:unit"
    #[test]
    fn detect_test_gate_unit_with_type_check() {
        let tmp = setup_package_json(r#""test:type":"tsc","test:unit":"vitest","test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        let test_gate = gates.iter().find(|g| g.name == "test").unwrap();
        assert!(test_gate.command.contains("test:unit"));
    }

    // T-006: test script + no type-check → test = "nr test"
    #[test]
    fn detect_test_gate_fallback_to_test() {
        let tmp = setup_package_json(r#""test":"vitest""#);
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, true);
        let test_gate = gates.iter().find(|g| g.name == "test").unwrap();
        assert!(test_gate.command.contains("\"test\"") || test_gate.command.ends_with("test"));
    }

    // T-008: $LINT_CMD override (works even without nr)
    #[test]
    fn env_override_lint_cmd() {
        let tmp = setup_package_json(r#""lint":"eslint .""#);
        let overrides = EnvOverrides {
            lint_cmd: Some("custom-lint".into()),
            ..Default::default()
        };
        let gates = detect_script_gates_inner(&overrides, &tmp, false);
        let lint_gate = gates.iter().find(|g| g.name == "lint").unwrap();
        assert_eq!(lint_gate.command, "custom-lint");
    }

    // T-007: type-check fail → test skip
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
        assert!(test_result.is_skipped(), "test should be skipped when type-check fails");
    }

    // T-026: nr 未インストール → nr-based gates not generated, env override still works
    #[test]
    fn no_nr_skips_script_gates_without_override() {
        let tmp = setup_package_json(r#""lint":"eslint .","test":"vitest""#);

        // nr absent → no script gates without override
        let gates = detect_script_gates_inner(&no_overrides(), &tmp, false);
        assert!(gates.is_empty(), "no gates should be generated without nr and no overrides");

        // env override bypasses nr check
        let overrides = EnvOverrides {
            lint_cmd: Some("custom-lint".into()),
            ..Default::default()
        };
        let gates = detect_script_gates_inner(&overrides, &tmp, false);
        assert!(gates.iter().any(|g| g.name == "lint"));
        assert_eq!(
            gates.iter().find(|g| g.name == "lint").unwrap().command,
            "custom-lint"
        );
    }

    #[test]
    fn skipped_result() {
        let r = ToolResult::skipped("test");
        assert!(r.is_skipped());
        assert!(!r.is_failure());
        assert!(r.output().is_empty());
    }

    #[test]
    fn gates_skip_when_condition_not_met() {
        for (name, project) in [
            ("knip", test_project(false, false)),
            ("tsgo", test_project(true, false)),
            ("madge", test_project(false, true)),
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

        assert!(!(gate_by_name("madge").condition)(&pkg_only));
        assert!(!(gate_by_name("madge").condition)(&ts_only));

        let tmp = crate::test_utils::TempDir::new("madge-cond");
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        let with_src = ProjectInfo {
            root: tmp.to_path_buf(),
            has_package_json: true,
            has_tsconfig: false,
        };
        assert!((gate_by_name("madge").condition)(&with_src));
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
}
