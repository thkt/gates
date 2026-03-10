use regex::Regex;
use std::sync::LazyLock;

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());
static MULTI_BLANK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

pub fn sanitize(input: &str) -> String {
    let s = ANSI_RE.replace_all(input, "");

    let s: String = s
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    let s = MULTI_BLANK.replace_all(&s, "\n\n");

    s.into_owned()
}

pub fn tail_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.to_string();
    }
    lines[lines.len() - max_lines..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_ansi_escape_codes() {
        let input = "\x1b[31mError:\x1b[0m something failed";
        assert_eq!(sanitize(input), "Error: something failed");
    }

    #[test]
    fn removes_complex_ansi_codes() {
        let input = "\x1b[1;32m✓\x1b[0m test passed \x1b[38;5;240m(0.5s)\x1b[0m";
        assert_eq!(sanitize(input), "✓ test passed (0.5s)");
    }

    #[test]
    fn compresses_consecutive_blank_lines() {
        let input = "line1\n\n\n\nline2";
        assert_eq!(sanitize(input), "line1\n\nline2");
    }

    #[test]
    fn preserves_single_blank_line() {
        let input = "line1\n\nline2";
        assert_eq!(sanitize(input), "line1\n\nline2");
    }

    #[test]
    fn removes_trailing_whitespace() {
        let input = "hello   \nworld\t";
        assert_eq!(sanitize(input), "hello\nworld");
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn tail_keeps_last_n_lines() {
        let input = "1\n2\n3\n4\n5";
        assert_eq!(tail_lines(input, 3), "3\n4\n5");
    }

    #[test]
    fn tail_no_op_when_within_limit() {
        let input = "1\n2\n3";
        assert_eq!(tail_lines(input, 5), "1\n2\n3");
    }

    #[test]
    fn tail_single_line() {
        assert_eq!(tail_lines("hello", 50), "hello");
    }
}
