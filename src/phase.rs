use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum PreviousBlock {
    NotFound,
    GateFailed,
    AllPassed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Fix,
    Review,
    Allow,
}

/// Marker string embedded in review block reason.
pub const ALL_PASSED_MARKER: &str = "All gates passed.";

const BANNED_FOOTER: &str = "\
---\n\
Banned in completion claims: \"should\", \"probably\", \"seems to\", \"I think\", \"looks like\".\n\
Replace with evidence from command output.";

pub fn detect_previous_block(transcript_path: &Path) -> PreviousBlock {
    let Ok(content) = std::fs::read_to_string(transcript_path) else {
        return PreviousBlock::NotFound;
    };

    // Most recent match wins; gates JSON may be escaped inside a tool_result content string.
    for line in content.lines().rev() {
        let Some(reason) = extract_block_reason(line) else {
            continue;
        };
        if reason.contains(ALL_PASSED_MARKER) {
            return PreviousBlock::AllPassed;
        }
        if reason.contains("failed.") || reason.contains("gates failed") {
            return PreviousBlock::GateFailed;
        }
    }

    PreviousBlock::NotFound
}

fn extract_block_reason(line: &str) -> Option<String> {
    if !line.contains("block") {
        return None;
    }

    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;

    if parsed.get("decision").and_then(|v| v.as_str()) == Some("block") {
        return parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .map(String::from);
    }

    let content = parsed.get("content").and_then(|v| v.as_str())?;
    let inner: serde_json::Value = serde_json::from_str(content).ok()?;
    if inner.get("decision").and_then(|v| v.as_str()) == Some("block") {
        return inner
            .get("reason")
            .and_then(|v| v.as_str())
            .map(String::from);
    }

    None
}

pub fn determine_phase(all_passed: bool, previous: &PreviousBlock, review_enabled: bool) -> Phase {
    if !all_passed {
        return Phase::Fix;
    }
    if !review_enabled {
        return Phase::Allow;
    }
    match previous {
        PreviousBlock::AllPassed => Phase::Allow,
        _ => Phase::Review,
    }
}

pub fn build_fix_prompt(failures: &str) -> String {
    format!("{failures}\n\n{BANNED_FOOTER}")
}

pub fn build_review_prompt() -> String {
    format!(
        "{ALL_PASSED_MARKER} Before completing:\n\n\
         1. Run Review Gate:\n\
         \x20  - Spawn code-quality-reviewer as background agent\n\
         \x20  - Review changed files for structural and readability issues\n\
         \x20  - Fix any high-severity findings\n\n\
         2. Verify regression tests (if any bug fixes):\n\
         \x20  - Revert the fix, confirm test FAILS (proves test catches the bug)\n\
         \x20  - Restore the fix, confirm test passes\n\n\
         3. Final verification (IDENTIFY → RUN → READ → VERIFY → CLAIM):\n\
         \x20  - IDENTIFY: What commands prove all gates pass?\n\
         \x20  - RUN: Execute each command FRESH (not from memory)\n\
         \x20  - READ: Check exit codes and output\n\
         \x20  - VERIFY: Does output confirm all pass?\n\
         \x20  - CLAIM: Only then claim completion\n\n\
         {BANNED_FOOTER}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    // T-013: transcript に gates block あり (gate fail)
    #[test]
    fn detect_previous_gate_failure() {
        let tmp = TempDir::new("phase");
        let transcript = tmp.join("session.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"user","message":"fix this"}
{"type":"assistant","message":"I'll fix it"}
{"type":"tool_result","content":"{\"decision\":\"block\",\"reason\":\"knip failed. Remove unused exports.\"}"}
"#,
        )
        .unwrap();
        assert_eq!(
            detect_previous_block(&transcript),
            PreviousBlock::GateFailed
        );
    }

    // T-014: transcript に "All gates passed" block あり
    #[test]
    fn detect_previous_all_passed() {
        let tmp = TempDir::new("phase");
        let transcript = tmp.join("session.jsonl");
        fs::write(
            &transcript,
            &format!(
                r#"{{"type":"tool_result","content":"{{\"decision\":\"block\",\"reason\":\"{ALL_PASSED_MARKER} Before completing:\"}}"}}
"#
            ),
        )
        .unwrap();
        assert_eq!(detect_previous_block(&transcript), PreviousBlock::AllPassed);
    }

    // T-015: transcript に gates block なし
    #[test]
    fn detect_no_previous_block() {
        let tmp = TempDir::new("phase");
        let transcript = tmp.join("session.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"user","message":"hello"}
{"type":"assistant","message":"hi"}
"#,
        )
        .unwrap();
        assert_eq!(detect_previous_block(&transcript), PreviousBlock::NotFound);
    }

    // T-016: transcript 読み失敗 → NotFound (fail-open)
    #[test]
    fn detect_transcript_read_failure() {
        let result = detect_previous_block(Path::new("/nonexistent/path.jsonl"));
        assert_eq!(result, PreviousBlock::NotFound);
    }

    // T-017: gate fail → Phase::Fix
    #[test]
    fn phase_fix_on_gate_failure() {
        assert_eq!(
            determine_phase(false, &PreviousBlock::NotFound, true),
            Phase::Fix
        );
    }

    // T-018: all pass + NotFound → Phase::Review
    #[test]
    fn phase_review_on_first_pass() {
        assert_eq!(
            determine_phase(true, &PreviousBlock::NotFound, true),
            Phase::Review
        );
    }

    // T-019: all pass + GateFailed → Phase::Review
    #[test]
    fn phase_review_after_previous_failure() {
        assert_eq!(
            determine_phase(true, &PreviousBlock::GateFailed, true),
            Phase::Review
        );
    }

    // T-020: all pass + AllPassed → Phase::Allow
    #[test]
    fn phase_allow_after_review() {
        assert_eq!(
            determine_phase(true, &PreviousBlock::AllPassed, true),
            Phase::Allow
        );
    }

    // T-024: review: false → Phase::Allow
    #[test]
    fn phase_allow_when_review_disabled() {
        assert_eq!(
            determine_phase(true, &PreviousBlock::NotFound, false),
            Phase::Allow
        );
    }

    // T-021: fix prompt includes gate output
    #[test]
    fn fix_prompt_contains_failure_details() {
        let prompt = build_fix_prompt("knip failed. Remove unused exports.\n\nUnused export: foo");
        assert!(prompt.contains("knip failed"));
        assert!(prompt.contains("Unused export: foo"));
        assert!(prompt.contains("Banned"));
    }

    // T-022: review prompt includes 5-step gate + revert cycle
    #[test]
    fn review_prompt_contains_verification_steps() {
        let prompt = build_review_prompt();
        assert!(prompt.contains(ALL_PASSED_MARKER));
        assert!(prompt.contains("IDENTIFY"));
        assert!(prompt.contains("RUN"));
        assert!(prompt.contains("READ"));
        assert!(prompt.contains("VERIFY"));
        assert!(prompt.contains("CLAIM"));
        assert!(prompt.contains("Revert the fix"));
    }

    // T-023: all prompts include banned words footer
    #[test]
    fn prompts_include_banned_footer() {
        let fix = build_fix_prompt("error");
        let review = build_review_prompt();
        for prompt in [&fix, &review] {
            assert!(prompt.contains("should"), "missing 'should' in banned list");
            assert!(prompt.contains("probably"), "missing 'probably'");
            assert!(prompt.contains("seems to"), "missing 'seems to'");
        }
    }
}
