use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const EXCLUDED_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target"];

pub struct CircularResult {
    pub cycles: Vec<Vec<String>>,
}

pub fn detect(src_dir: &Path) -> CircularResult {
    let mut files = Vec::new();
    collect_source_files(src_dir, &mut files);

    let graph = build_graph(&files);
    let raw_cycles = find_cycles(&graph);

    let cycles = raw_cycles
        .into_iter()
        .map(|cycle| {
            cycle
                .into_iter()
                .map(|p| path_display(&p, src_dir))
                .collect()
        })
        .collect();

    CircularResult { cycles }
}

fn path_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn collect_source_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

fn build_graph(files: &[PathBuf]) -> HashMap<PathBuf, Vec<PathBuf>> {
    let file_set: HashSet<&PathBuf> = files.iter().collect();
    let mut graph = HashMap::new();

    for file in files {
        let Ok(source) = std::fs::read_to_string(file) else {
            continue;
        };
        let specifiers = extract_import_specifiers(&source, file);
        let resolved: Vec<PathBuf> = specifiers
            .iter()
            .filter_map(|s| resolve_import(file, s))
            .filter(|p| file_set.contains(p))
            .collect();
        graph.insert(file.clone(), resolved);
    }

    graph
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

#[derive(Clone, Copy, PartialEq)]
enum Color {
    White,
    Gray,
    Black,
}

fn find_cycles(graph: &HashMap<PathBuf, Vec<PathBuf>>) -> Vec<Vec<PathBuf>> {
    let mut colors: HashMap<&PathBuf, Color> = graph.keys().map(|k| (k, Color::White)).collect();
    let mut path: Vec<PathBuf> = Vec::new();
    let mut seen_cycles: HashSet<Vec<PathBuf>> = HashSet::new();
    let mut cycles: Vec<Vec<PathBuf>> = Vec::new();

    for node in graph.keys() {
        if colors[node] == Color::White {
            dfs(
                node,
                graph,
                &mut colors,
                &mut path,
                &mut seen_cycles,
                &mut cycles,
            );
        }
    }
    cycles
}

fn dfs(
    node: &PathBuf,
    graph: &HashMap<PathBuf, Vec<PathBuf>>,
    colors: &mut HashMap<&PathBuf, Color>,
    path: &mut Vec<PathBuf>,
    seen: &mut HashSet<Vec<PathBuf>>,
    cycles: &mut Vec<Vec<PathBuf>>,
) {
    if let Some(c) = colors.get_mut(node) {
        *c = Color::Gray;
    }
    path.push(node.clone());

    if let Some(neighbors) = graph.get(node) {
        for next in neighbors {
            match colors.get(next).copied() {
                Some(Color::White) => {
                    dfs(next, graph, colors, path, seen, cycles);
                }
                Some(Color::Gray) => {
                    if let Some(start) = path.iter().position(|p| p == next) {
                        let mut cycle: Vec<PathBuf> = path[start..].to_vec();
                        if let Some(min_idx) = cycle
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, p)| (*p).clone())
                            .map(|(i, _)| i)
                        {
                            cycle.rotate_left(min_idx);
                        }
                        if seen.insert(cycle.clone()) {
                            cycles.push(cycle);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    path.pop();
    if let Some(c) = colors.get_mut(node) {
        *c = Color::Black;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;

    #[test]
    fn no_cycles_in_clean_project() {
        let tmp = TempDir::new("circular-clean");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nexport const a = b + 1;\n",
        )
        .unwrap();
        fs::write(src.join("b.ts"), "export const b = 42;\n").unwrap();

        let result = detect(&src);
        assert!(result.cycles.is_empty());
    }

    #[test]
    fn detects_simple_cycle() {
        let tmp = TempDir::new("circular-cycle");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "import { b } from './b';\nexport const a = 1;\n").unwrap();
        fs::write(src.join("b.ts"), "import { a } from './a';\nexport const b = 2;\n").unwrap();

        let result = detect(&src);
        assert_eq!(result.cycles.len(), 1);
        assert_eq!(result.cycles[0].len(), 2);
    }

    #[test]
    fn detects_three_node_cycle() {
        let tmp = TempDir::new("circular-3node");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "import { b } from './b';\nexport const a = 1;\n").unwrap();
        fs::write(src.join("b.ts"), "import { c } from './c';\nexport const b = 2;\n").unwrap();
        fs::write(src.join("c.ts"), "import { a } from './a';\nexport const c = 3;\n").unwrap();

        let result = detect(&src);
        assert_eq!(result.cycles.len(), 1);
        assert_eq!(result.cycles[0].len(), 3);
    }

    #[test]
    fn ignores_bare_imports() {
        let tmp = TempDir::new("circular-bare");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import React from 'react';\nexport const a = 1;\n",
        )
        .unwrap();

        let result = detect(&src);
        assert!(result.cycles.is_empty());
    }

    #[test]
    fn handles_reexports() {
        let tmp = TempDir::new("circular-reexport");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "export { b } from './b';\nexport const a = 1;\n").unwrap();
        fs::write(src.join("b.ts"), "export * from './a';\nexport const b = 2;\n").unwrap();

        let result = detect(&src);
        assert_eq!(result.cycles.len(), 1);
    }

    #[test]
    fn resolves_index_files() {
        let tmp = TempDir::new("circular-index");
        let src = tmp.join("src");
        let sub = src.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './sub';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            sub.join("index.ts"),
            "import { a } from '../a';\nexport const b = 2;\n",
        )
        .unwrap();

        let result = detect(&src);
        assert_eq!(result.cycles.len(), 1);
    }

    #[test]
    fn empty_directory() {
        let tmp = TempDir::new("circular-empty");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();

        let result = detect(&src);
        assert!(result.cycles.is_empty());
    }

    #[test]
    fn skips_node_modules() {
        let tmp = TempDir::new("circular-nm");
        let src = tmp.join("src");
        let nm = src.join("node_modules/pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(src.join("a.ts"), "export const a = 1;\n").unwrap();
        fs::write(nm.join("index.ts"), "import { a } from '../../a';\n").unwrap();

        let result = detect(&src);
        assert!(result.cycles.is_empty());
    }
}
