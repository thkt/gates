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
    let mut seen_cycles: HashSet<Vec<PathBuf>> = HashSet::new();
    let mut cycles: Vec<Vec<PathBuf>> = Vec::new();

    for node in graph.keys() {
        if colors[node] == Color::White {
            dfs(node, graph, &mut colors, &mut seen_cycles, &mut cycles);
        }
    }
    cycles
}

/// Iterative three-color DFS. An explicit heap stack of `(node, next neighbor
/// index)` frames replaces recursion so depth is bounded by heap, not the thread
/// stack: a deep non-cyclic chain that would overflow recursive descent (an
/// uncatchable abort, defeating the fail-open join fallback) is traversed safely.
/// The stack doubles as the current DFS path (Gray nodes are exactly its
/// members), so frame order and Gray back-edge recording mirror the recursive
/// form and the set of detected cycles is unchanged.
fn dfs<'a>(
    start: &'a PathBuf,
    graph: &'a HashMap<PathBuf, Vec<PathBuf>>,
    colors: &mut HashMap<&'a PathBuf, Color>,
    seen: &mut HashSet<Vec<PathBuf>>,
    cycles: &mut Vec<Vec<PathBuf>>,
) {
    if let Some(c) = colors.get_mut(start) {
        *c = Color::Gray;
    }
    let mut stack: Vec<(&'a PathBuf, usize)> = vec![(start, 0)];

    while let Some(&(node, idx)) = stack.last() {
        let neighbors = graph.get(node);
        if let Some(next) = neighbors.and_then(|n| n.get(idx)) {
            stack.last_mut().unwrap().1 = idx + 1;
            match colors.get(next).copied() {
                Some(Color::White) => {
                    if let Some(c) = colors.get_mut(next) {
                        *c = Color::Gray;
                    }
                    stack.push((next, 0));
                }
                Some(Color::Gray) => record_cycle(next, &stack, seen, cycles),
                _ => {}
            }
            continue;
        }

        // Neighbors exhausted: leave `node` (post-visit) and pop its frame.
        if let Some(c) = colors.get_mut(node) {
            *c = Color::Black;
        }
        stack.pop();
    }
}

/// Record the cycle closed by a back edge to the Gray node `next`: the path of
/// nodes on `stack` from `next` onward, rotated to start at its lexicographically
/// smallest member so rotations of one cycle dedup to a single canonical entry.
fn record_cycle(
    next: &PathBuf,
    stack: &[(&PathBuf, usize)],
    seen: &mut HashSet<Vec<PathBuf>>,
    cycles: &mut Vec<Vec<PathBuf>>,
) {
    if let Some(start) = stack.iter().position(|&(p, _)| p == next) {
        let mut cycle: Vec<PathBuf> = stack[start..].iter().map(|&(p, _)| p.clone()).collect();
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
    fn deep_acyclic_chain_does_not_overflow() {
        // A single deep non-cyclic chain p0 -> p1 -> ... -> p(N-1). The recursive
        // dfs recursed once per chain link and overflowed the thread stack
        // (uncatchable SIGABRT) at this depth; the iterative dfs is heap-bounded
        // and reports no cycles. Calls find_cycles directly to skip filesystem +
        // parse and keep the synthetic graph in memory.
        let n = 50_000;
        let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for i in 0..n {
            let edges = if i + 1 < n {
                vec![PathBuf::from(format!("/p{}.ts", i + 1))]
            } else {
                Vec::new()
            };
            graph.insert(PathBuf::from(format!("/p{i}.ts")), edges);
        }

        let cycles = find_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn deep_chain_closed_into_single_cycle() {
        // The same deep chain with the tail linked back to the head, forming one
        // cycle of length N. Proves the iterative rewrite preserves cycle
        // detection at depth: exactly one cycle spanning all N nodes is reported.
        let n = 50_000;
        let mut graph: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for i in 0..n {
            let next = (i + 1) % n;
            graph.insert(
                PathBuf::from(format!("/p{i}.ts")),
                vec![PathBuf::from(format!("/p{next}.ts"))],
            );
        }

        let cycles = find_cycles(&graph);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), n);
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
