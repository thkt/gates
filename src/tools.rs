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
    Passed(String),
    Failed(String),
    Skipped,
}

#[derive(Debug)]
pub struct ToolResult {
    pub name: &'static str,
    pub outcome: GateOutcome,
}

impl ToolResult {
    pub fn skipped(name: &'static str) -> Self {
        Self {
            name,
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
            GateOutcome::Passed(s) | GateOutcome::Failed(s) => s,
            GateOutcome::Skipped => "",
        }
    }
}

pub struct GateDefinition {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub condition: fn(&ProjectInfo) -> bool,
}

pub const GATES: &[GateDefinition] = &[
    GateDefinition {
        name: "knip",
        command: "knip",
        args: &[],
        condition: |p| p.has_package_json,
    },
    GateDefinition {
        name: "tsgo",
        command: "tsgo",
        args: &[],
        condition: |p| p.has_tsconfig,
    },
    GateDefinition {
        name: "madge",
        command: "madge",
        args: &["--circular", "--extensions", "ts,tsx", "src/"],
        condition: |p| p.has_package_json && p.root.join("src").is_dir(),
    },
];

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
    // Safety: kill(-pid) sends SIGKILL to the process group led by `pid`.
    // pid_i32 is validated > 0 by the pid == 0 check and try_from(u32).
    let ret = unsafe { kill(-pid_i32, 9) };
    if ret != 0 {
        eprintln!(
            "gates: failed to kill process group {}: {}",
            pid,
            std::io::Error::last_os_error()
        );
    }
}

fn run_command(name: &'static str, mut cmd: Command, timeout: Duration) -> ToolResult {
    cmd.process_group(0);

    let child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gates: {} spawn error: {}", name, e);
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
                GateOutcome::Passed(text)
            } else {
                GateOutcome::Failed(text)
            };
            ToolResult { name, outcome }
        }
        Ok(Err(e)) => {
            eprintln!("gates: {} output read error: {}", name, e);
            ToolResult::skipped(name)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("gates: {} timed out after {}s", name, timeout.as_secs());
            kill_process_group(pid);
            // Brief wait for child cleanup after SIGKILL
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
    run_command(gate.name, cmd, GATE_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_project(has_pkg: bool, has_ts: bool) -> ProjectInfo {
        ProjectInfo {
            root: PathBuf::from("/tmp/nonexistent"),
            has_package_json: has_pkg,
            has_tsconfig: has_ts,
        }
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
        assert!(matches!(result.outcome, GateOutcome::Passed(_)));
        assert!(result.output().contains("hello"));
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
