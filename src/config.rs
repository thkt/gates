use serde::Deserialize;
use std::path::Path;

const TOOLS_CONFIG_FILE: &str = ".claude/tools.json";

#[derive(Debug, PartialEq)]
pub struct GatesConfig {
    pub knip: bool,
    pub tsgo: bool,
    pub madge: bool,
    pub lint: bool,
    pub type_check: bool,
    pub test: bool,
    pub review: bool,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            knip: false,
            tsgo: false,
            madge: false,
            lint: false,
            type_check: false,
            test: false,
            review: true,
        }
    }
}

#[derive(Deserialize)]
struct ToolsJson {
    gates: Option<GatesSection>,
    review: Option<bool>,
}

#[derive(Deserialize)]
struct GatesSection {
    knip: Option<bool>,
    tsgo: Option<bool>,
    madge: Option<bool>,
    lint: Option<bool>,
    #[serde(rename = "type-check")]
    type_check: Option<bool>,
    test: Option<bool>,
}

impl GatesConfig {
    pub fn is_enabled(&self, name: &str) -> bool {
        match name {
            "knip" => self.knip,
            "tsgo" => self.tsgo,
            "madge" => self.madge,
            "lint" => self.lint,
            "type-check" => self.type_check,
            "test" => self.test,
            _ => false,
        }
    }

    pub fn load(project_dir: &Path) -> Self {
        let path = project_dir.join(TOOLS_CONFIG_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(parsed) = serde_json::from_str::<ToolsJson>(&content) else {
            eprintln!("gates: failed to parse {}", path.display());
            return Self::default();
        };
        let review = parsed.review.unwrap_or(true);
        let Some(gates) = parsed.gates else {
            return Self {
                review,
                ..Self::default()
            };
        };
        Self {
            knip: gates.knip.unwrap_or(false),
            tsgo: gates.tsgo.unwrap_or(false),
            madge: gates.madge.unwrap_or(false),
            lint: gates.lint.unwrap_or(false),
            type_check: gates.type_check.unwrap_or(false),
            test: gates.test.unwrap_or(false),
            review,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    fn setup_dir(json: Option<&str>) -> TempDir {
        let dir = TempDir::new("config");
        if let Some(content) = json {
            let claude_dir = dir.join(".claude");
            fs::create_dir_all(&claude_dir).unwrap();
            fs::write(claude_dir.join("tools.json"), content).unwrap();
        }
        dir
    }

    #[test]
    fn reads_gates_section() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true,"tsgo":false,"madge":true}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.knip);
        assert!(!config.tsgo);
        assert!(config.madge);
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = setup_dir(None);
        let config = GatesConfig::load(&dir);
        assert_eq!(config, GatesConfig::default());
    }

    #[test]
    fn missing_gates_section_returns_default() {
        let dir = setup_dir(Some(r#"{"reviews":{"tools":{"knip":true}}}"#));
        let config = GatesConfig::load(&dir);
        assert_eq!(config, GatesConfig::default());
    }

    #[test]
    fn partial_gates_section() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.knip);
        assert!(!config.tsgo);
        assert!(!config.madge);
        assert!(!config.lint);
    }

    #[test]
    fn invalid_json_returns_default() {
        let dir = setup_dir(Some("not json{{{"));
        let config = GatesConfig::load(&dir);
        assert_eq!(config, GatesConfig::default());
    }

    // T-001 config: new gate settings
    #[test]
    fn reads_new_gate_settings() {
        let dir = setup_dir(Some(
            r#"{"gates":{"knip":true,"lint":true,"type-check":true,"test":true}}"#,
        ));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("lint"));
        assert!(config.is_enabled("type-check"));
        assert!(config.is_enabled("test"));
    }

    // T-024: review: false
    #[test]
    fn review_disabled() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true},"review":false}"#));
        let config = GatesConfig::load(&dir);
        assert!(!config.review);
    }

    #[test]
    fn review_defaults_to_true() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.review);
    }
}
