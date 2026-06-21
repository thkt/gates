use crate::depgraph::{DependencyGraph, display_path};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct CircularResult {
    pub cycles: Vec<Vec<String>>,
}

/// Detect cycles from a prebuilt graph, letting callers share a single parse
/// with the coupling gate.
pub fn detect_in(graph: &DependencyGraph, src_dir: &Path) -> CircularResult {
    let raw_cycles = find_cycles(&graph.edges);

    let cycles = raw_cycles
        .into_iter()
        .map(|cycle| {
            cycle
                .into_iter()
                .map(|p| display_path(&p, src_dir))
                .collect()
        })
        .collect();

    CircularResult { cycles }
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
    use crate::depgraph;
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

        let result = detect_in(&depgraph::build(&src), &src);
        assert!(result.cycles.is_empty());
    }

    #[test]
    fn detects_simple_cycle() {
        let tmp = TempDir::new("circular-cycle");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
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

        let result = detect_in(&depgraph::build(&src), &src);
        assert_eq!(result.cycles.len(), 1);
        assert_eq!(result.cycles[0].len(), 2);
    }

    #[test]
    fn detects_three_node_cycle() {
        let tmp = TempDir::new("circular-3node");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "import { b } from './b';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            src.join("b.ts"),
            "import { c } from './c';\nexport const b = 2;\n",
        )
        .unwrap();
        fs::write(
            src.join("c.ts"),
            "import { a } from './a';\nexport const c = 3;\n",
        )
        .unwrap();

        let result = detect_in(&depgraph::build(&src), &src);
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

        let result = detect_in(&depgraph::build(&src), &src);
        assert!(result.cycles.is_empty());
    }

    #[test]
    fn handles_reexports() {
        let tmp = TempDir::new("circular-reexport");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("a.ts"),
            "export { b } from './b';\nexport const a = 1;\n",
        )
        .unwrap();
        fs::write(
            src.join("b.ts"),
            "export * from './a';\nexport const b = 2;\n",
        )
        .unwrap();

        let result = detect_in(&depgraph::build(&src), &src);
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

        let result = detect_in(&depgraph::build(&src), &src);
        assert_eq!(result.cycles.len(), 1);
    }

    #[test]
    fn empty_directory() {
        let tmp = TempDir::new("circular-empty");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();

        let result = detect_in(&depgraph::build(&src), &src);
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

        let result = detect_in(&depgraph::build(&src), &src);
        assert!(result.cycles.is_empty());
    }
}
