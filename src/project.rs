use crate::traverse;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub has_package_json: bool,
    pub has_tsconfig: bool,
}

impl ProjectInfo {
    pub fn detect(dir: &Path) -> Self {
        let root = Self::find_root(dir);
        let has_package_json = root.join("package.json").exists();
        let has_tsconfig = root.join("tsconfig.json").exists();

        Self {
            root,
            has_package_json,
            has_tsconfig,
        }
    }

    fn find_root(start: &Path) -> PathBuf {
        traverse::walk_ancestors(start, |dir| {
            dir.join(".git").exists().then(|| dir.to_path_buf())
        })
        .unwrap_or_else(|| start.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    #[test]
    fn detects_both_files() {
        let tmp = TempDir::new("project-both");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(tmp.join("tsconfig.json"), "{}").unwrap();

        let info = ProjectInfo::detect(&tmp);
        assert!(info.has_package_json);
        assert!(info.has_tsconfig);
    }

    #[test]
    fn detects_package_json_only() {
        let tmp = TempDir::new("project-pkg");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();

        let info = ProjectInfo::detect(&tmp);
        assert!(info.has_package_json);
        assert!(!info.has_tsconfig);
    }

    #[test]
    fn detects_tsconfig_only() {
        let tmp = TempDir::new("project-ts");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("tsconfig.json"), "{}").unwrap();

        let info = ProjectInfo::detect(&tmp);
        assert!(!info.has_package_json);
        assert!(info.has_tsconfig);
    }

    #[test]
    fn no_project_files() {
        let tmp = TempDir::new("project-empty");
        fs::create_dir_all(tmp.join(".git")).unwrap();

        let info = ProjectInfo::detect(&tmp);
        assert!(!info.has_package_json);
        assert!(!info.has_tsconfig);
    }

    #[test]
    fn uses_git_root_not_subdir() {
        let tmp = TempDir::new("project-root");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let subdir = tmp.join("src/components");
        fs::create_dir_all(&subdir).unwrap();

        let info = ProjectInfo::detect(&subdir);
        assert!(info.has_package_json);
        assert_eq!(info.root, *tmp);
    }
}
