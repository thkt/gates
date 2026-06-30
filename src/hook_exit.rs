//! Process exit codes per ADR-0066 Group 3 (Hook tool) convention, shared with
//! the role-pair `guardrails` (`PreToolUse`). gates is the `PostToolUse` half, so
//! its decision surface differs: a blocking decision is emitted as stdout
//! `{"decision":"block"}` + exit 0 (the OUTCOME constraint and issue #18), not
//! as a real exit 2. This enum encodes the Group 3 semantics at the type level
//! so the mapping is explicit and switchable if the hook spec later requires
//! distinct exit codes.
//!
//! | Exit | variant      | Meaning                              | Live on hook path? |
//! |------|--------------|--------------------------------------|--------------------|
//! | 0    | `Pass`       | all gates pass                       | yes                |
//! | 1    | `Advisory`   | advisory failure (severity=warn)     | no (reserved)      |
//! | 2    | `Blocking`   | blocking failure; child gate failed  | no (→ stdout JSON + exit 0) |
//! | 64   | `InputError` | usage error / malformed hook input   | yes (CLI usage)    |
//! | 70   | `Internal`   | orchestration panic (caught)         | yes (`main` `catch_unwind`) |
//!
//! Two codes reach a real process exit on the live paths. `InputError` (64) on
//! direct-CLI usage errors (`gates a b c`, a non-directory argument) — the hook
//! invocation (`gates`, dir = cwd) never trips them. `Internal` (70) when an
//! orchestration-layer panic (config load, the block `json!`, reporter
//! formatting) unwinds to `main`'s `catch_unwind`; gate-thread panics never get
//! here, as `join_or_skip` already maps them to `skipped`. Per Group 3 a non-2
//! exit stays non-blocking, so 70 surfaces the fault without breaking fail-open.
//! On the hook path `Blocking` is converted to stdout JSON + exit 0 by the
//! caller. `Advisory` (1) is reserved: every gate today is blocking. `EX_IOERR`
//! (74) on the `gates show` path is an ADR-0060 I/O code orthogonal to this enum
//! and lives as a separate constant.

// `Pass`, `Advisory`, and `Blocking` are reserved at the type level per issue
// #18: the hook wrapper keeps exit 0 for the decision surface, so `Pass` (0) and
// `Blocking` (2) never reach a live exit and `Advisory` (1) has no producer yet.
// `InputError` and `Internal` are live (see the module doc). The full Group 3
// mapping is kept so a future hook-spec change can switch the surface without
// redefining the type.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookExitCode {
    Pass,
    Advisory,
    Blocking,
    InputError,
    Internal,
}

impl HookExitCode {
    pub const fn code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Advisory => 1,
            Self::Blocking => 2,
            Self::InputError => 64,
            Self::Internal => 70,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HookExitCode;

    // T-101: Pass maps to sysexits EX_OK (0).
    #[test]
    fn pass_is_zero() {
        assert_eq!(HookExitCode::Pass.code(), 0);
    }

    // T-102: Advisory maps to 1 (severity=warn convention).
    #[test]
    fn advisory_is_one() {
        assert_eq!(HookExitCode::Advisory.code(), 1);
    }

    // T-103: Blocking maps to 2 (severity=error / child gate failed).
    #[test]
    fn blocking_is_two() {
        assert_eq!(HookExitCode::Blocking.code(), 2);
    }

    // T-104: InputError maps to sysexits EX_USAGE (64).
    #[test]
    fn input_error_is_sysexits_usage() {
        assert_eq!(HookExitCode::InputError.code(), 64);
    }

    // T-105: Internal maps to sysexits EX_SOFTWARE (70).
    #[test]
    fn internal_is_sysexits_software() {
        assert_eq!(HookExitCode::Internal.code(), 70);
    }
}
