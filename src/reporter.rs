use crate::color;
use crate::tools::ToolResult;

// Match guardrails separator lengths (header + "Gates " = 50, footer = 50)
pub(crate) const HEADER_SEPARATOR: &str = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
pub(crate) const FOOTER_SEPARATOR: &str = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
const MAX_PREVIEW_LINES: usize = 3;

pub fn format_summary(results: &[ToolResult]) -> String {
    let ran: Vec<_> = results.iter().filter(|r| !r.is_skipped()).collect();
    if ran.is_empty() {
        return String::new();
    }

    let failures: Vec<_> = ran.iter().filter(|r| r.is_failure()).collect();
    let warnings: Vec<_> = ran.iter().filter(|r| r.is_warning()).collect();

    if failures.is_empty() {
        // Warnings are advisory, so they count as ran but not as passed.
        let passed = ran.len() - warnings.len();
        let mut lines = vec![
            String::new(),
            color::bold_green(&format!("Gates \u{2713} {}/{} passed", passed, ran.len())),
        ];
        append_advisories(&mut lines, &warnings);
        return lines.join("\n");
    }

    let mut lines = vec![
        String::new(),
        color::bold_red(&format!("Gates {HEADER_SEPARATOR}")),
    ];

    for f in &failures {
        lines.push(color::red(&format!("  \u{2717} {}", f.name)));
        push_preview(&mut lines, f.output(), color::red);
    }

    lines.push(color::bold_red(FOOTER_SEPARATOR));
    lines.push(color::bold_red(&format!(
        "BLOCKED: {} gate{} failed. Fix the source code and retry. Do not circumvent this check.",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    )));

    append_advisories(&mut lines, &warnings);

    lines.join("\n")
}

/// Append a yellow advisory block for warn-level gates. These report duplication
/// or similar findings without blocking, so they render below any failures.
fn append_advisories(lines: &mut Vec<String>, warnings: &[&&ToolResult]) {
    if warnings.is_empty() {
        return;
    }
    lines.push(color::bold_yellow(&format!(
        "  {} advisory warning{}:",
        warnings.len(),
        if warnings.len() == 1 { "" } else { "s" }
    )));
    for w in warnings {
        lines.push(color::yellow(&format!("  \u{26a0} {}", w.name)));
        push_preview(lines, w.output(), color::yellow);
    }
}

/// Push up to `MAX_PREVIEW_LINES` non-blank output lines, colorized, with a dim
/// truncation note. Shared by failure and advisory rendering.
fn push_preview(lines: &mut Vec<String>, output: &str, colorize: fn(&str) -> String) {
    if output.is_empty() {
        return;
    }
    let non_blank: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = non_blank.len();
    for line in &non_blank[..total.min(MAX_PREVIEW_LINES)] {
        lines.push(colorize(&format!("    {line}")));
    }
    if total > MAX_PREVIEW_LINES {
        lines.push(color::dim(&format!(
            "    ... +{} more lines",
            total - MAX_PREVIEW_LINES
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::strip_ansi;
    use crate::tools::GateOutcome;

    fn passed(name: &'static str) -> ToolResult {
        ToolResult {
            name,
            hint: "",
            outcome: GateOutcome::Passed,
        }
    }

    fn failed(name: &'static str) -> ToolResult {
        ToolResult {
            name,
            hint: "",
            outcome: GateOutcome::Failed("error".into()),
        }
    }

    fn failed_with(name: &'static str, output: &str) -> ToolResult {
        ToolResult {
            name,
            hint: "",
            outcome: GateOutcome::Failed(output.into()),
        }
    }

    fn skipped(name: &'static str) -> ToolResult {
        ToolResult::skipped(name)
    }

    fn warned_with(name: &'static str, output: &str) -> ToolResult {
        ToolResult {
            name,
            hint: "",
            outcome: GateOutcome::Warned(output.into()),
        }
    }

    #[test]
    fn empty_when_all_skipped() {
        let results = vec![skipped("knip")];
        assert_eq!(format_summary(&results), "");
    }

    #[test]
    fn all_passed_one_line() {
        let results = vec![passed("lint"), passed("test")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("Gates"), "missing Gates label");
        assert!(output.contains("2/2 passed"), "missing pass count");
        assert!(!output.contains(HEADER_SEPARATOR), "no border on pass");
    }

    #[test]
    fn failure_shows_only_failed_gates() {
        let results = vec![passed("lint"), failed("test"), passed("knip")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("\u{2717} test"), "missing failed gate");
        assert!(!output.contains("lint"), "passed gate should not appear");
        assert!(!output.contains("knip"), "passed gate should not appear");
        assert!(output.contains(
            "BLOCKED: 1 gate failed. Fix the source code and retry. Do not circumvent this check."
        ));
    }

    #[test]
    fn multiple_failures_plural() {
        let results = vec![failed("lint"), failed("test")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("\u{2717} lint"));
        assert!(output.contains("\u{2717} test"));
        assert!(output.contains(
            "BLOCKED: 2 gates failed. Fix the source code and retry. Do not circumvent this check."
        ));
    }

    #[test]
    fn skipped_gates_excluded_from_count() {
        let results = vec![passed("lint"), skipped("knip"), passed("test")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("2/2 passed"), "skipped excluded from count");
    }

    #[test]
    fn failure_output_preview() {
        let results = vec![failed_with(
            "test",
            "FAIL src/app.test.ts\nExpected 1, got 2\nsome detail",
        )];
        let output = strip_ansi(&format_summary(&results));
        assert!(
            output.contains("FAIL src/app.test.ts"),
            "missing first line"
        );
        assert!(output.contains("Expected 1, got 2"), "missing second line");
        assert!(output.contains("some detail"), "missing third line");
        assert!(
            !output.contains("more lines"),
            "should not truncate 3 lines"
        );
    }

    #[test]
    fn failure_output_truncated() {
        let long_output = "line1\nline2\nline3\nline4\nline5";
        let results = vec![failed_with("test", long_output)];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("line1"));
        assert!(output.contains("line3"));
        assert!(!output.contains("line4"), "line4 should be truncated");
        assert!(output.contains("+2 more lines"));
    }

    #[test]
    fn failure_empty_output_no_preview() {
        let results = vec![failed_with("knip", "")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("\u{2717} knip"));
    }

    #[test]
    fn empty_slice_returns_empty() {
        assert_eq!(format_summary(&[]), "");
    }

    #[test]
    fn blank_lines_excluded_from_preview() {
        let output_with_blanks = "\nline1\n\n\nline2\n   \nline3\nline4";
        let results = vec![failed_with("test", output_with_blanks)];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("line1"));
        assert!(output.contains("line3"));
        assert!(!output.contains("line4"), "line4 should be truncated");
        assert!(output.contains("+1 more lines"));
    }

    // T-611: a lone warning shows an advisory line, not a BLOCKED section.
    #[test]
    fn warning_only_is_advisory_not_blocked() {
        let results = vec![warned_with("jscpd", "Found duplication: 12%")];
        let output = strip_ansi(&format_summary(&results));
        assert!(!output.contains("BLOCKED"), "warn must not block: {output}");
        assert!(
            output.contains("\u{26a0} jscpd"),
            "missing advisory: {output}"
        );
        assert!(output.contains("Found duplication: 12%"), "missing preview");
    }

    // T-612: a warning counts as ran but not as passed.
    #[test]
    fn warning_excluded_from_pass_count() {
        let results = vec![passed("lint"), warned_with("jscpd", "dup")];
        let output = strip_ansi(&format_summary(&results));
        assert!(
            output.contains("1/2 passed"),
            "warn excluded from passed: {output}"
        );
        assert!(
            output.contains("advisory"),
            "missing advisory label: {output}"
        );
    }

    // T-613: a failure and a warning together show both BLOCKED and the advisory.
    #[test]
    fn failure_and_warning_show_both() {
        let results = vec![failed("test"), warned_with("jscpd", "dup")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("BLOCKED"), "failure must block: {output}");
        assert!(
            output.contains("\u{26a0} jscpd"),
            "missing advisory: {output}"
        );
    }

    // T-614: with no warnings the pass line is unchanged (no advisory text).
    #[test]
    fn no_advisory_when_no_warnings() {
        let results = vec![passed("lint"), passed("test")];
        let output = strip_ansi(&format_summary(&results));
        assert!(output.contains("2/2 passed"));
        assert!(!output.contains("advisory"), "no advisory without warnings");
    }

    #[test]
    fn header_footer_separators_match_guardrails() {
        assert_eq!(
            HEADER_SEPARATOR.chars().count(),
            44,
            "header: Gates + 44 = 50"
        );
        assert_eq!(FOOTER_SEPARATOR.chars().count(), 50, "footer: 50 chars");
    }
}
