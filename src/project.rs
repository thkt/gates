use crate::traverse;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory names package discovery never descends into: dependencies, VCS,
/// build output, and coverage/next caches cannot own a first-party package
/// target. Extends the list `depgraph`/`snapshot` apply to their own downward
/// walks with the cache dirs that commonly hold copied `package.json` fixtures.
const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
];

/// How many directory levels below the git root package discovery descends. A
/// monorepo nests its members shallowly (`packages/foo`, `apps/bar/ui`), so a
/// small bound finds them while keeping a pathological tree from blowing the
/// hook's latency budget.
const MAX_PACKAGE_DEPTH: usize = 4;

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub has_package_json: bool,
    pub has_tsconfig: bool,
}

impl ProjectInfo {
    pub fn detect(dir: &Path) -> Self {
        let root = Self::find_root(dir);
        Self::for_dir(root)
    }

    /// Build a `ProjectInfo` rooted at `dir` without re-running the git-root
    /// walk. `dir` is taken as the project root verbatim, so every gate's
    /// `current_dir` and config-file probe target that directory. Used both for
    /// the git-root project (`detect`) and for each discovered package target.
    fn for_dir(dir: PathBuf) -> Self {
        let has_package_json = dir.join("package.json").exists();
        let has_tsconfig = dir.join("tsconfig.json").exists();
        Self {
            root: dir,
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

    /// The projects the package-scoped gates (tsgo, oxlint, circular, coupling)
    /// run against. When the git root directly owns analyzable code — its own
    /// `tsconfig.json` or `src/` — the root is the single target, which is the
    /// behavior every non-monorepo repo already gets. Otherwise the root is a
    /// monorepo container whose `.git` sits above the real packages (the issue
    /// #102 layout: outer `.git`, inner `packages/foo/tsconfig.json`), so the
    /// targets are the member package directories discovered beneath it. Each
    /// gate's own condition still decides per target whether it runs, so a
    /// package with no tsconfig skips the type gate exactly as before.
    pub fn package_targets(&self) -> Vec<ProjectInfo> {
        if self.has_tsconfig || self.root.join("src").is_dir() {
            return vec![self.clone()];
        }
        let mut dirs = Vec::new();
        discover_packages(&self.root, MAX_PACKAGE_DEPTH, &mut dirs);
        // `read_dir` yields entries in filesystem order, which varies across
        // platforms; sort so the gate output and audit log read the same run to
        // run.
        dirs.sort();
        dirs.into_iter().map(Self::for_dir).collect()
    }
}

/// Collect directories beneath `dir` that own a package boundary marker
/// (`package.json` or `tsconfig.json`), descending at most `depth` levels and
/// never into dependency/build directories. Descent stops once a directory
/// matches: a package's own fixtures or examples (which carry their own
/// `package.json`) are not promoted to separate targets, and workspace members
/// sit one level apart so this still finds every member. `src/` is deliberately
/// not a marker here — a bare `src/` with no manifest is not a distinct package,
/// and the self-contained-root case is already handled by `package_targets`.
fn discover_packages(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if EXCLUDED_DIRS.contains(&name) {
            continue;
        }
        if is_package_dir(&path) {
            out.push(path);
        } else {
            discover_packages(&path, depth - 1, out);
        }
    }
}

fn is_package_dir(dir: &Path) -> bool {
    dir.join("package.json").exists() || dir.join("tsconfig.json").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use crate::tools::run_graph_gates;
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

    // A root that directly owns its tsconfig is a self-contained project: the
    // package-scoped gates target the root alone, exactly as before #102. This
    // pins the "zero behavior change for non-monorepo repos" invariant.
    #[test]
    fn self_contained_tsconfig_root_targets_only_itself() {
        let tmp = TempDir::new("self-contained-ts");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("tsconfig.json"), "{}").unwrap();
        fs::create_dir_all(tmp.join("packages/app")).unwrap();
        fs::write(tmp.join("packages/app/tsconfig.json"), "{}").unwrap();

        let targets = ProjectInfo::detect(&tmp).package_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].root, *tmp);
    }

    // Same invariant via the `src/` anchor: a root that owns `src/` runs at the
    // root and does not fan out into nested packages.
    #[test]
    fn self_contained_src_root_targets_only_itself() {
        let tmp = TempDir::new("self-contained-src");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("packages/app")).unwrap();
        fs::write(tmp.join("packages/app/package.json"), "{}").unwrap();

        let targets = ProjectInfo::detect(&tmp).package_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].root, *tmp);
    }

    // The #102 layout: the git root is a container (package.json workspaces
    // manifest, no root tsconfig/src) and the real package owns the tsconfig.
    // Discovery descends to that package so its target carries the tsconfig the
    // root lacks.
    #[test]
    fn nested_monorepo_targets_the_tsconfig_owning_package() {
        let tmp = TempDir::new("nested-ts");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let pkg = tmp.join("packages/app");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), "{}").unwrap();
        fs::write(pkg.join("tsconfig.json"), "{}").unwrap();

        let root = ProjectInfo::detect(&tmp);
        assert!(!root.has_tsconfig, "the container root owns no tsconfig");

        let targets = root.package_targets();
        let app = targets
            .iter()
            .find(|t| t.root == pkg)
            .expect("packages/app must be discovered as a target");
        assert!(
            app.has_tsconfig,
            "the package target carries its own tsconfig"
        );
    }

    // Bug reproduction (#102): in a container monorepo the circular gate must
    // detect a cycle that lives inside a package's `src/`. Today the gate is
    // anchored at the git root, which owns no `src/`, so the cycle is skipped and
    // goes undetected — this test fails until discovery fans the gate out to the
    // package that owns the `src/`.
    #[test]
    fn circular_gate_detects_cycle_inside_nested_package() {
        let tmp = TempDir::new("nested-circular");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let pkg = tmp.join("packages/app");
        let src = pkg.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(pkg.join("package.json"), "{}").unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            src.join("b.ts"),
            "import { a } from './a';\nexport const b = 2;\n",
        )
        .unwrap();

        let root = ProjectInfo::detect(&tmp);
        // The git root owns no `src/`, so a root-anchored run skips the gate —
        // documenting the pre-fix behavior the bug report describes.
        let at_root = run_graph_gates(&root, true, false, None);
        assert!(
            at_root[0].is_skipped(),
            "the container root has no src/, so a root-anchored circular run skips"
        );

        // The fix: fanning out to the discovered package targets detects the cycle.
        let detected = root
            .package_targets()
            .iter()
            .flat_map(|t| run_graph_gates(t, true, false, None))
            .any(|r| r.is_failure());
        assert!(
            detected,
            "circular must detect the cycle inside packages/app/src"
        );
    }

    // Dependency directories never count as packages even when they contain a
    // package.json, so discovery does not fan a gate out into node_modules.
    #[test]
    fn discovery_skips_node_modules() {
        let tmp = TempDir::new("nested-nodemodules");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("package.json"), "{}").unwrap();
        let dep = tmp.join("node_modules/some-dep");
        fs::create_dir_all(&dep).unwrap();
        fs::write(dep.join("package.json"), "{}").unwrap();
        let pkg = tmp.join("packages/app");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), "{}").unwrap();

        let targets = ProjectInfo::detect(&tmp).package_targets();
        assert!(
            targets
                .iter()
                .all(|t| !t.root.starts_with(tmp.join("node_modules"))),
            "node_modules packages must not be discovered as targets"
        );
        assert!(
            targets.iter().any(|t| t.root == pkg),
            "real packages are still discovered"
        );
    }
}
