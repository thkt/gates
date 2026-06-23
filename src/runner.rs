use crate::sanitize;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub(crate) const GATE_TIMEOUT: Duration = Duration::from_secs(60);
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

pub(crate) fn run_command(name: &'static str, cmd: Command, timeout: Duration) -> ToolResult {
    run_command_with_label(name, cmd, timeout, None)
}

pub(crate) fn run_command_with_label(
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

#[cfg(test)]
mod tests {
    use super::*;

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
