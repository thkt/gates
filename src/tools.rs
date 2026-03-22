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

    pub fn passed(name: &'static str) -> Self {
        Self {
            name,
            hint: "",
            outcome: GateOutcome::Passed,
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
];


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

fn detect_run_prefix(project_dir: &std::path::Path) -> Option<String> {
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
    project_dir: &std::path::Path,
) -> Vec<ScriptGate> {
    let run_prefix = detect_run_prefix(project_dir);
    detect_script_gates_inner(overrides, project_dir, run_prefix.as_deref())
}

fn detect_script_gates_inner(
    overrides: &EnvOverrides,
    project_dir: &std::path::Path,
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
            eprintln!("gates: {} wait thread disconnected", name);
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

    let output: Vec<String> = result.issues.iter().map(|i| i.to_string()).collect();
    let truncated = sanitize::tail_lines(&output.join("\n"), MAX_OUTPUT_LINES);

    ToolResult {
        name: "litmus",
        hint: "Fix test quality issues (weak assertions, mock overuse, tautological tests).",
        outcome: GateOutcome::Failed(truncated),
    }
}

pub fn run_circular(project: &ProjectInfo) -> ToolResult {
    let src_dir = project.root.join("src");
    if !project.has_package_json || !src_dir.is_dir() {
        return ToolResult::skipped("circular");
    }

    let result = crate::circular::detect(&src_dir);

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
                .chain(std::iter::once(cycle[0].as_str()))
                .collect::<Vec<_>>()
                .join(" → ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = sanitize::tail_lines(&format!("{header}{body}"), MAX_OUTPUT_LINES);

    ToolResult {
        name: "circular",
        hint: "Break circular import dependencies.",
        outcome: GateOutcome::Failed(truncated),
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
        assert!(output.contains("1 circular dependency"), "should show count: {output}");
        assert!(output.contains(" → "), "should show arrow chain: {output}");
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
}
