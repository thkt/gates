use crate::traverse;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn resolve_bin(name: &str, start: &Path) -> PathBuf {
    if name.contains('/') || name.contains("..") {
        return PathBuf::from(name);
    }
    traverse::walk_ancestors(start, |dir| {
        let candidate = dir.join("node_modules/.bin").join(name);
        is_executable(&candidate).then_some(candidate)
    })
    .unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_bin_in_node_modules() {
        let tmp = TempDir::new("resolve-find");
        let bin_dir = tmp.join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join("knip");
        fs::write(&bin_path, "").unwrap();
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();

        let result = resolve_bin("knip", &tmp);
        assert_eq!(result, bin_path);
    }

    #[test]
    fn skips_non_executable_bin() {
        let tmp = TempDir::new("resolve-noexec");
        let bin_dir = tmp.join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join("knip");
        fs::write(&bin_path, "").unwrap();
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o644)).unwrap();

        let result = resolve_bin("knip", &tmp);
        assert_eq!(result, PathBuf::from("knip"));
    }

    #[test]
    fn falls_back_to_bare_name() {
        let tmp = TempDir::new("resolve-nomod");
        fs::create_dir_all(tmp.join(".git")).unwrap();

        let result = resolve_bin("knip", &tmp);
        assert_eq!(result, PathBuf::from("knip"));
    }
}
