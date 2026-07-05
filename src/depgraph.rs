use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const EXCLUDED_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target"];

/// Intra-project dependency graph built from a single parse of all `.ts`/`.tsx`
/// files. Shared by the `circular` and `coupling` gates so the project is parsed
/// once per hook invocation.
pub struct DependencyGraph {
    /// All collected `.ts`/`.tsx` files (canonicalized).
    pub files: Vec<PathBuf>,
    /// Resolved intra-project import edges (afferent/efferent coupling source).
    pub edges: HashMap<PathBuf, Vec<PathBuf>>,
    /// Count of external (bare) import specifiers per file (efferent coupling to
    /// packages outside the project).
    pub external_counts: HashMap<PathBuf, usize>,
}

pub fn build(src_dir: &Path) -> DependencyGraph {
    let mut files = Vec::new();
    collect_source_files(src_dir, &mut files);

    let file_set: HashSet<&PathBuf> = files.iter().collect();
    let mut edges = HashMap::new();
    let mut external_counts = HashMap::new();

    for file in &files {
        let Ok(source) = fs::read_to_string(file) else {
            edges.insert(file.clone(), Vec::new());
            external_counts.insert(file.clone(), 0);
            continue;
        };
        let mut resolved = Vec::new();
        let mut external = 0;
        for specifier in extract_import_specifiers(&source, file) {
            if specifier.starts_with('.') {
                // Dedup so Ca/Ce count distinct imported files: a file that
                // splits value and type imports of the same module
                // (`import { a }` + `import type { T } from './a'`) must
                // contribute a single edge, matching the "number of importing
                // files" definition of afferent coupling.
                if let Some(path) = resolve_import(file, &specifier)
                    && file_set.contains(&path)
                    && !resolved.contains(&path)
                {
                    resolved.push(path);
                }
            } else {
                external += 1;
            }
        }
        edges.insert(file.clone(), resolved);
        external_counts.insert(file.clone(), external);
    }

    DependencyGraph {
        files,
        edges,
        external_counts,
    }
}

/// Render a file path relative to `base` for display.
pub fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Collect all `.ts`/`.tsx` files under `dir` (canonicalized, excluding
/// `node_modules`/`.git`/`dist`/`build`/`target`). Shared with the `clone` gate,
/// which re-parses files independently of the dependency graph.
pub fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_source_files(dir, &mut files);
    files
}

fn collect_source_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !EXCLUDED_DIRS.contains(&name) {
                collect_source_files(&path, files);
            }
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx")
        ) && let Ok(canonical) = path.canonicalize()
        {
            files.push(canonical);
        }
    }
}

fn extract_import_specifiers(source: &str, file: &Path) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file).unwrap_or_default();
    let ret = Parser::new(&allocator, source, source_type).parse();

    let mut specifiers = Vec::new();
    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                specifiers.push(decl.source.value.to_string());
            }
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(source) = &decl.source {
                    specifiers.push(source.value.to_string());
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                specifiers.push(decl.source.value.to_string());
            }
            _ => {}
        }
    }
    specifiers
}

fn resolve_import(from_file: &Path, specifier: &str) -> Option<PathBuf> {
    if !specifier.starts_with('.') {
        return None;
    }
    let dir = from_file.parent()?;
    let base = dir.join(specifier);

    if base.is_file() {
        return base.canonicalize().ok();
    }

    for ext in ["ts", "tsx"] {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }

    for ext in ["ts", "tsx"] {
        let candidate = base.join(format!("index.{ext}"));
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    fn canon(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    // Pin EXCLUDED_DIRS's exact contents/order so an accidental addition,
    // removal, or reordering is caught immediately by this single test.
    #[test]
    fn excluded_dirs_is_pinned() {
        assert_eq!(
            EXCLUDED_DIRS,
            &["node_modules", ".git", "dist", "build", "target"]
        );
    }

    // T-101: relative import a -> b records an intra-project edge, no external.
    #[test]
    fn relative_import_records_edge() {
        let tmp = TempDir::new("depgraph-edge");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "import { b } from './b';\n").unwrap();
        fs::write(src.join("b.ts"), "export const b = 1;\n").unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        let b = canon(&src.join("b.ts"));
        assert_eq!(graph.edges[&a], vec![b]);
        assert_eq!(graph.external_counts[&a], 0);
    }

    // T-102: a bare import counts as external, not an edge.
    #[test]
    fn bare_import_counts_external() {
        let tmp = TempDir::new("depgraph-bare");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "import React from 'react';\n").unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        assert!(graph.edges[&a].is_empty());
        assert_eq!(graph.external_counts[&a], 1);
    }

    // T-103: mixed relative and external specifiers are counted separately.
    #[test]
    fn mixed_internal_and_external() {
        let tmp = TempDir::new("depgraph-mixed");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nimport x from 'pkg-x';\nimport y from 'pkg-y';\n",
        )
        .unwrap();
        fs::write(src.join("b.ts"), "export const b = 1;\n").unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        assert_eq!(graph.edges[&a].len(), 1);
        assert_eq!(graph.external_counts[&a], 2);
    }

    // T-108: value + type imports of the same module record one deduped edge.
    #[test]
    fn duplicate_imports_record_single_edge() {
        let tmp = TempDir::new("depgraph-dedup");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nimport type { T } from './b';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            src.join("b.ts"),
            "export const b = 1;\nexport type T = number;\n",
        )
        .unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        let b = canon(&src.join("b.ts"));
        assert_eq!(
            graph.edges[&a],
            vec![b],
            "split value/type import of one module must dedup to a single edge"
        );
    }

    // T-104: `./sub` resolves to sub/index.ts.
    #[test]
    fn resolves_index_file() {
        let tmp = TempDir::new("depgraph-index");
        let src = tmp.join("src");
        let sub = src.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(src.join("a.ts"), "import { b } from './sub';\n").unwrap();
        fs::write(sub.join("index.ts"), "export const b = 1;\n").unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        let idx = canon(&sub.join("index.ts"));
        assert_eq!(graph.edges[&a], vec![idx]);
    }

    // T-105: re-export `export * from './b'` records an edge.
    #[test]
    fn reexport_records_edge() {
        let tmp = TempDir::new("depgraph-reexport");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "export * from './b';\n").unwrap();
        fs::write(src.join("b.ts"), "export const b = 1;\n").unwrap();

        let graph = build(&src);
        let a = canon(&src.join("a.ts"));
        assert_eq!(graph.edges[&a].len(), 1);
    }

    // T-106: files under node_modules are excluded from collection.
    #[test]
    fn excludes_node_modules() {
        let tmp = TempDir::new("depgraph-nm");
        let src = tmp.join("src");
        let nm = src.join("node_modules/pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(src.join("a.ts"), "export const a = 1;\n").unwrap();
        fs::write(nm.join("index.ts"), "export const x = 1;\n").unwrap();

        let graph = build(&src);
        assert_eq!(graph.files.len(), 1);
    }

    // T-109: the remaining 4 hardcoded EXCLUDED_DIRS entries (.git/dist/build/
    // target) are still excluded from collection, while a sibling control file
    // is still collected. Removing an entry from EXCLUDED_DIRS makes this fail.
    #[test]
    fn excludes_remaining_hardcoded_dirs_but_collects_control_file() {
        // Intentional duplicate of EXCLUDED_DIRS: kept local so that deleting an
        // entry from the production list breaks this test instead of silently
        // shrinking the loop.
        let excluded_dirs = [".git", "dist", "build", "target"];

        let tmp = TempDir::new("depgraph-excluded-dirs");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("kept.ts"), "export const kept = 1;\n").unwrap();

        for dir in excluded_dirs {
            let sub = src.join(dir);
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("excluded.ts"), "export const excluded = 1;\n").unwrap();
        }

        let graph = build(&src);
        let names: HashSet<&str> = graph
            .files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        let expected: HashSet<&str> = HashSet::from(["kept.ts"]);
        assert_eq!(
            names, expected,
            "excluded dirs must not contribute files, got {:?}",
            names
        );
        assert!(graph.files.contains(&canon(&src.join("kept.ts"))));
    }

    // T-107: an empty directory yields empty graph data.
    #[test]
    fn empty_directory() {
        let tmp = TempDir::new("depgraph-empty");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();

        let graph = build(&src);
        assert!(graph.files.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.external_counts.is_empty());
    }
}
