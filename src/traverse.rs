use std::env;
use std::path::{Path, PathBuf};

const MAX_TRAVERSAL_DEPTH: usize = 20;

/// Stops at the `.git` boundary (after visiting that directory), at `$HOME`
/// (which is inspected but whose ancestors are not), or at the depth limit.
/// The `$HOME` fence is load-bearing, not cosmetic: the walk must never escape
/// `$HOME` into shared parents (`/Users`, `/home`, `/`), so removing the fence is
/// a containment regression. It also mirrors formatter's `bounded_ancestors` so the two
/// resolvers' ancestor walks stay aligned (independent copies, no drift). The
/// third sibling, guardrails, deliberately keeps a different model
/// (`canonicalize` + `project_root` boundary, no fences) to preserve its
/// `OutsideProjectRoot` forensic signal — so it is not unified with this walk.
pub fn walk_ancestors<T>(start: &Path, mut visitor: impl FnMut(&Path) -> Option<T>) -> Option<T> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut current = start;
    for _ in 0..MAX_TRAVERSAL_DEPTH {
        if let Some(result) = visitor(current) {
            return Some(result);
        }
        if current.join(".git").exists() {
            break;
        }
        // Inspect `$HOME` itself, then fence out everything above it.
        if home.as_deref() == Some(current) {
            break;
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    #[test]
    fn finds_target_in_start_dir() {
        let tmp = TempDir::new("traverse-start");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("target.txt"), "").unwrap();

        let result = walk_ancestors(&tmp, |dir| {
            let c = dir.join("target.txt");
            c.exists().then_some(c)
        });
        assert!(result.is_some());
    }

    #[test]
    fn finds_target_in_parent() {
        let tmp = TempDir::new("traverse-parent");
        fs::write(tmp.join("target.txt"), "").unwrap();
        let subdir = tmp.join("sub");
        fs::create_dir_all(&subdir).unwrap();

        let result = walk_ancestors(&subdir, |dir| {
            let c = dir.join("target.txt");
            c.exists().then_some(c)
        });
        assert!(result.is_some());
    }

    #[test]
    fn stops_at_git_boundary() {
        let tmp = TempDir::new("traverse-git");
        let project = tmp.join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(tmp.join("target.txt"), "").unwrap();
        let subdir = project.join("src");
        fs::create_dir_all(&subdir).unwrap();

        let result = walk_ancestors(&subdir, |dir| {
            let c = dir.join("target.txt");
            c.exists().then_some(c)
        });
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_when_not_found() {
        let tmp = TempDir::new("traverse-none");
        fs::create_dir_all(tmp.join(".git")).unwrap();

        let result: Option<bool> = walk_ancestors(&tmp, |_| None);
        assert!(result.is_none());
    }
}
