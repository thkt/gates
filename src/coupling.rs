use crate::depgraph::{DependencyGraph, display_path};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Robert Martin coupling metrics for a single module.
pub struct ModuleMetrics {
    pub path: String,
    pub ca: usize,
    pub ce: usize,
    pub instability: f64,
}

pub struct CouplingResult {
    /// Modules whose afferent coupling exceeds the configured threshold,
    /// sorted by Ca descending.
    pub god_modules: Vec<ModuleMetrics>,
}

/// Compute Ca/Ce/instability per module and flag God modules (Ca > threshold).
///
/// - Ca (afferent) = number of intra-project files importing the module.
/// - Ce (efferent) = intra-project out-degree + external (bare) specifier count.
/// - I (instability) = Ce / (Ca + Ce); an isolated module (Ca + Ce == 0) is 0.0.
pub fn analyze(graph: &DependencyGraph, src_dir: &Path, ca_threshold: usize) -> CouplingResult {
    let mut afferent: HashMap<&PathBuf, usize> = graph.files.iter().map(|f| (f, 0)).collect();
    for targets in graph.edges.values() {
        for target in targets {
            if let Some(count) = afferent.get_mut(target) {
                *count += 1;
            }
        }
    }

    let mut god_modules: Vec<ModuleMetrics> = graph
        .files
        .iter()
        .filter_map(|file| {
            let ca = afferent.get(file).copied().unwrap_or(0);
            if ca <= ca_threshold {
                return None;
            }
            let internal_ce = graph.edges.get(file).map_or(0, Vec::len);
            let external_ce = graph.external_counts.get(file).copied().unwrap_or(0);
            let ce = internal_ce + external_ce;
            Some(ModuleMetrics {
                path: display_path(file, src_dir),
                ca,
                ce,
                instability: instability(ca, ce),
            })
        })
        .collect();

    // Tiebreak by path so equal-Ca modules print in a filesystem-independent
    // order (collect_source_files yields entries in read_dir order).
    god_modules.sort_by_key(|m| (Reverse(m.ca), m.path.clone()));

    CouplingResult { god_modules }
}

fn instability(ca: usize, ce: usize) -> f64 {
    let total = ca + ce;
    if total == 0 {
        return 0.0;
    }
    ce as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::iter;

    fn graph_from(
        edges: &[(&str, &[&str])],
        external: &[(&str, usize)],
    ) -> (DependencyGraph, PathBuf) {
        let base = PathBuf::from("/proj/src");
        let p = |name: &str| base.join(name);
        let files: Vec<PathBuf> = {
            let mut set: Vec<PathBuf> = Vec::new();
            for (from, tos) in edges {
                for name in iter::once(from).chain(tos.iter()) {
                    let path = p(name);
                    if !set.contains(&path) {
                        set.push(path);
                    }
                }
            }
            for (name, _) in external {
                let path = p(name);
                if !set.contains(&path) {
                    set.push(path);
                }
            }
            set
        };
        let edge_map: HashMap<PathBuf, Vec<PathBuf>> = files
            .iter()
            .map(|f| {
                let targets = edges
                    .iter()
                    .find(|(from, _)| p(from) == *f)
                    .map(|(_, tos)| tos.iter().map(|t| p(t)).collect())
                    .unwrap_or_default();
                (f.clone(), targets)
            })
            .collect();
        let external_map: HashMap<PathBuf, usize> = files
            .iter()
            .map(|f| {
                let count = external
                    .iter()
                    .find(|(name, _)| p(name) == *f)
                    .map_or(0, |(_, c)| *c);
                (f.clone(), count)
            })
            .collect();
        (
            DependencyGraph {
                files,
                edges: edge_map,
                external_counts: external_map,
            },
            base,
        )
    }

    // T-301: Ca counts intra-project files importing the module.
    #[test]
    fn afferent_counts_importers() {
        let (graph, base) = graph_from(
            &[
                ("b.ts", &["a.ts"]),
                ("c.ts", &["a.ts"]),
                ("d.ts", &["a.ts"]),
            ],
            &[],
        );
        let result = analyze(&graph, &base, 0);
        let a = result
            .god_modules
            .iter()
            .find(|m| m.path == "a.ts")
            .unwrap();
        assert_eq!(a.ca, 3);
    }

    // T-302: Ce sums intra-project out-degree and external specifier count.
    #[test]
    fn efferent_sums_internal_and_external() {
        // a imports b, c (internal Ce=2) plus one external package (Ce+1);
        // z imports a so a surfaces as a god module at threshold 0.
        let (graph, base) = graph_from(
            &[("a.ts", &["b.ts", "c.ts"]), ("z.ts", &["a.ts"])],
            &[("a.ts", 1)],
        );
        let result = analyze(&graph, &base, 0);
        let a = result
            .god_modules
            .iter()
            .find(|m| m.path == "a.ts")
            .unwrap();
        assert_eq!(a.ce, 3);
    }

    // T-303: instability = Ce / (Ca + Ce).
    #[test]
    fn instability_ratio() {
        assert!((instability(1, 3) - 0.75).abs() < f64::EPSILON);
    }

    // T-304: an isolated module (Ca + Ce == 0) has instability 0.0, no panic.
    #[test]
    fn isolated_module_instability_zero() {
        assert_eq!(instability(0, 0), 0.0);
    }

    // T-305: a purely imported module (Ce == 0) has instability 0.0.
    #[test]
    fn pure_sink_instability_zero() {
        assert_eq!(instability(2, 0), 0.0);
    }

    // T-306: Ca == threshold + 1 is flagged as a God module.
    #[test]
    fn flags_module_above_threshold() {
        let (graph, base) = graph_from(
            &[
                ("b.ts", &["a.ts"]),
                ("c.ts", &["a.ts"]),
                ("d.ts", &["a.ts"]),
            ],
            &[],
        );
        let result = analyze(&graph, &base, 2);
        assert_eq!(result.god_modules.len(), 1);
        assert_eq!(result.god_modules[0].path, "a.ts");
        assert_eq!(result.god_modules[0].ca, 3);
    }

    // T-307: Ca == threshold is not flagged (strict greater-than).
    #[test]
    fn threshold_boundary_not_flagged() {
        let (graph, base) = graph_from(&[("b.ts", &["a.ts"]), ("c.ts", &["a.ts"])], &[]);
        let result = analyze(&graph, &base, 2);
        assert!(result.god_modules.is_empty());
    }

    // T-308: God modules are sorted by Ca descending.
    #[test]
    fn god_modules_sorted_by_ca_desc() {
        let (graph, base) = graph_from(
            &[
                ("i1.ts", &["low.ts"]),
                ("i2.ts", &["high.ts"]),
                ("i3.ts", &["high.ts"]),
                ("i4.ts", &["high.ts"]),
                ("i5.ts", &["low.ts"]),
            ],
            &[],
        );
        let result = analyze(&graph, &base, 1);
        assert_eq!(result.god_modules.len(), 2);
        assert_eq!(result.god_modules[0].path, "high.ts");
        assert_eq!(result.god_modules[1].path, "low.ts");
        assert!(result.god_modules[0].ca > result.god_modules[1].ca);
    }

    // T-309: equal-Ca god modules are ordered by path ascending (deterministic).
    #[test]
    fn equal_ca_tiebreaks_by_path() {
        // zebra and alpha are each imported by 2 files (Ca=2); declared
        // zebra-first so a stable sort would otherwise keep zebra ahead.
        let (graph, base) = graph_from(
            &[
                ("i1.ts", &["zebra.ts"]),
                ("i2.ts", &["zebra.ts"]),
                ("i3.ts", &["alpha.ts"]),
                ("i4.ts", &["alpha.ts"]),
            ],
            &[],
        );
        let result = analyze(&graph, &base, 1);
        assert_eq!(result.god_modules.len(), 2);
        assert_eq!(result.god_modules[0].path, "alpha.ts");
        assert_eq!(result.god_modules[1].path, "zebra.ts");
    }
}
