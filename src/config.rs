use serde::Deserialize;
use std::path::Path;

const TOOLS_CONFIG_FILE: &str = ".claude/tools.json";

#[derive(Debug, Default, PartialEq)]
pub struct GatesConfig {
    pub knip: bool,
    pub tsgo: bool,
    pub madge: bool,
}

#[derive(Deserialize)]
struct ToolsJson {
    gates: Option<GatesSection>,
}

#[derive(Deserialize)]
struct GatesSection {
    knip: Option<bool>,
    tsgo: Option<bool>,
    madge: Option<bool>,
}

impl GatesConfig {
    pub fn is_enabled(&self, name: &str) -> bool {
        match name {
            "knip" => self.knip,
            "tsgo" => self.tsgo,
            "madge" => self.madge,
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
        let Some(gates) = parsed.gates else {
            return Self::default();
        };
        Self {
            knip: gates.knip.unwrap_or(false),
            tsgo: gates.tsgo.unwrap_or(false),
            madge: gates.madge.unwrap_or(false),
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
        assert_eq!(
            config,
            GatesConfig {
                knip: true,
                tsgo: false,
                madge: true
            }
        );
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
        assert_eq!(
            config,
            GatesConfig {
                knip: true,
                tsgo: false,
                madge: false
            }
        );
    }

    #[test]
    fn invalid_json_returns_default() {
        let dir = setup_dir(Some("not json{{{"));
        let config = GatesConfig::load(&dir);
        assert_eq!(config, GatesConfig::default());
    }
}
