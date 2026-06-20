use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

const TOOLS_CONFIG_FILE: &str = ".claude/tools.json";

#[derive(Debug, PartialEq)]
pub enum ConfigSource {
    Default,
    Explicit,
}

#[derive(Debug, PartialEq)]
pub struct GatesConfig {
    pub gates: Option<HashMap<String, bool>>,
    pub coupling_ca_threshold: Option<usize>,
    pub source: ConfigSource,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            gates: None,
            coupling_ca_threshold: None,
            source: ConfigSource::Default,
        }
    }
}

#[derive(Deserialize)]
struct ToolsJson {
    gates: Option<HashMap<String, serde_json::Value>>,
    coupling: Option<CouplingSection>,
}

#[derive(Deserialize)]
struct CouplingSection {
    #[serde(rename = "caThreshold")]
    ca_threshold: Option<usize>,
}

impl GatesConfig {
    pub fn is_enabled(&self, name: &str) -> bool {
        match &self.gates {
            None => true,
            Some(map) => *map.get(name).unwrap_or(&false),
        }
    }

    pub fn load(project_dir: &Path) -> Self {
        let path = project_dir.join(TOOLS_CONFIG_FILE);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!("gates: failed to read {}: {}", path.display(), e);
                return Self::default();
            }
        };
        let parsed = match serde_json::from_str::<ToolsJson>(&content) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("gates: failed to parse {}: {}", path.display(), e);
                return Self::default();
            }
        };
        let coupling_ca_threshold = parsed.coupling.and_then(|c| c.ca_threshold);
        let Some(gates_map) = parsed.gates else {
            return Self {
                gates: None,
                coupling_ca_threshold,
                source: ConfigSource::Explicit,
            };
        };
        let mut gates: HashMap<String, bool> = HashMap::new();
        for (k, v) in gates_map {
            match v.as_bool() {
                Some(b) => {
                    gates.insert(k, b);
                }
                None => {
                    eprintln!(
                        "gates: ignoring non-boolean value for gate '{}' in {}",
                        k, TOOLS_CONFIG_FILE
                    );
                }
            }
        }
        Self {
            gates: Some(gates),
            coupling_ca_threshold,
            source: ConfigSource::Explicit,
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
        assert!(config.is_enabled("knip"));
        assert!(!config.is_enabled("tsgo"));
        assert!(config.is_enabled("madge"));
        assert_eq!(config.source, ConfigSource::Explicit);
    }

    #[test]
    fn missing_file_enables_all_gates() {
        let dir = setup_dir(None);
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("any-name"));
        assert_eq!(config.source, ConfigSource::Default);
    }

    #[test]
    fn missing_gates_section_enables_all_gates() {
        let dir = setup_dir(Some(r#"{"reviews":{"tools":{"knip":true}}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("any-name"));
        assert_eq!(config.source, ConfigSource::Explicit);
    }

    #[test]
    fn partial_gates_section() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("knip"));
        assert!(!config.is_enabled("unlisted"));
    }

    #[test]
    fn invalid_json_enables_all_gates() {
        let dir = setup_dir(Some("not json{{{"));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("knip"));
        assert_eq!(config.source, ConfigSource::Default);
    }

    #[test]
    fn unknown_gate_names_are_preserved() {
        let dir = setup_dir(Some(
            r#"{"gates":{"test-quality":true,"my-custom-gate":false}}"#,
        ));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("test-quality"));
        assert!(!config.is_enabled("my-custom-gate"));
    }

    // T-501: coupling.caThreshold is read into coupling_ca_threshold.
    #[test]
    fn reads_coupling_ca_threshold() {
        let dir = setup_dir(Some(r#"{"coupling":{"caThreshold":20}}"#));
        let config = GatesConfig::load(&dir);
        assert_eq!(config.coupling_ca_threshold, Some(20));
    }

    // T-502: absent coupling section yields no threshold.
    #[test]
    fn missing_coupling_section_yields_none() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true}}"#));
        let config = GatesConfig::load(&dir);
        assert_eq!(config.coupling_ca_threshold, None);
    }

    // T-503: disabling coupling via gates does not imply a threshold.
    #[test]
    fn coupling_disabled_has_no_threshold() {
        let dir = setup_dir(Some(r#"{"gates":{"coupling":false}}"#));
        let config = GatesConfig::load(&dir);
        assert!(!config.is_enabled("coupling"));
        assert_eq!(config.coupling_ca_threshold, None);
    }

    // T-504: gates booleans and coupling threshold coexist without interference.
    #[test]
    fn gates_and_coupling_coexist() {
        let dir = setup_dir(Some(
            r#"{"gates":{"knip":true,"coupling":true},"coupling":{"caThreshold":15}}"#,
        ));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("knip"));
        assert!(config.is_enabled("coupling"));
        assert_eq!(config.coupling_ca_threshold, Some(15));
    }

    #[test]
    fn non_bool_gate_values_are_ignored() {
        let dir = setup_dir(Some(r#"{"gates":{"knip":true,"bad":"string","num":42}}"#));
        let config = GatesConfig::load(&dir);
        assert!(config.is_enabled("knip"));
        assert!(!config.is_enabled("bad"));
        assert!(!config.is_enabled("num"));
    }
}
