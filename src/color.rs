fn use_color() -> bool {
    static COLOR: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("NO_COLOR").is_none());
    *COLOR
}

fn wrap(ansi_code: &str, text: &str) -> String {
    if use_color() {
        format!("\x1b[{ansi_code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn red(text: &str) -> String {
    wrap("31", text)
}

pub fn bold_red(text: &str) -> String {
    wrap("1;31", text)
}

pub fn bold_green(text: &str) -> String {
    wrap("1;32", text)
}

pub fn dim(text: &str) -> String {
    wrap("2", text)
}

#[cfg(test)]
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for inner in chars.by_ref() {
                if inner == 'm' {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_codes() {
        let colored = "\x1b[32mok\x1b[0m";
        assert_eq!(strip_ansi(colored), "ok");
    }

    #[test]
    fn strip_ansi_preserves_plain() {
        assert_eq!(strip_ansi("hello"), "hello");
    }
}
