use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    pub transcript_path: Option<String>,
    #[allow(dead_code)]
    pub session_id: Option<String>,
    pub stop_hook_active: Option<bool>,
}

impl HookInput {
    pub fn from_stdin() -> Self {
        let stdin = std::io::stdin();
        match serde_json::from_reader(stdin.lock()) {
            Ok(input) => input,
            Err(e) => {
                eprintln!("gates: stdin parse failed (fail-open): {}", e);
                Self::default()
            }
        }
    }

    #[cfg(test)]
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T-010: 正常な stdin JSON パース
    #[test]
    fn parse_valid_hook_input() {
        let input = HookInput::parse(
            r#"{"transcript_path":"/tmp/session.jsonl","session_id":"abc123","stop_hook_active":false}"#,
        )
        .unwrap();
        assert_eq!(input.transcript_path.as_deref(), Some("/tmp/session.jsonl"));
        assert_eq!(input.session_id.as_deref(), Some("abc123"));
        assert_eq!(input.stop_hook_active, Some(false));
    }

    // T-011: stop_hook_active: true
    #[test]
    fn stop_hook_active_detected() {
        let input = HookInput::parse(r#"{"stop_hook_active":true}"#).unwrap();
        assert_eq!(input.stop_hook_active, Some(true));
    }

    // T-012: パース失敗 → None
    #[test]
    fn parse_failure_returns_none() {
        let input = HookInput::parse("not json{{{");
        assert!(input.is_none());
    }

    // Extra: missing fields default to None
    #[test]
    fn missing_fields_default_to_none() {
        let input = HookInput::parse(r#"{}"#).unwrap();
        assert!(input.transcript_path.is_none());
        assert!(input.session_id.is_none());
        assert!(input.stop_hook_active.is_none());
    }
}
