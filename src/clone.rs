use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::depgraph::display_path;

/// Default minimum AST node count for a subtree to be considered a clone unit.
pub const DEFAULT_MIN_NODES: usize = 20;
/// Default minimum source line span for a clone unit.
pub const DEFAULT_MIN_LINES: usize = 5;
/// Default number of clone groups that triggers a block (DRY rule of three).
pub const DEFAULT_BLOCK_THRESHOLD: usize = 3;

/// ESTree node types whose identifier/literal *content* is normalized away when
/// computing the Type 2 (structural) key, so renamed identifiers and differing
/// literals hash equal. Gated by node type — not key name — so structural fields
/// that merely share a name (e.g. `Property.value`) are never blanked.
const CONTENT_TYPES: &[&str] = &[
    "Identifier",
    "Literal",
    "TemplateElement",
    "PrivateIdentifier",
    "JSXIdentifier",
    "JSXText",
];
/// Content-bearing field names on the node types above.
const CONTENT_KEYS: &[&str] = &["name", "value", "raw", "cooked", "regex", "bigint"];

/// One occurrence of a structural clone.
pub struct CloneInstance {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// A set of structurally identical subtrees occurring in 2+ locations.
pub struct CloneGroup {
    /// Occurrences, sorted by (path, start_line). Length >= 2.
    pub instances: Vec<CloneInstance>,
    /// AST node count of the duplicated subtree.
    pub node_count: usize,
    /// True when every occurrence is byte-for-byte structurally identical
    /// including identifiers/literals (Type 1); false when only the structure
    /// matches (Type 2, renamed identifiers/literals).
    pub exact: bool,
}

pub struct CloneResult {
    /// Clone groups, sorted by node_count descending then first path ascending.
    pub groups: Vec<CloneGroup>,
}

/// A qualifying subtree recorded during the walk.
struct Candidate {
    file_idx: usize,
    canon_blank: String,
    canon_named: String,
    node_count: usize,
    start: usize,
    end: usize,
    start_line: usize,
    end_line: usize,
}

/// Canonical string pair plus node count produced for a serde_json value.
struct Rendered {
    named: String,
    blank: String,
    count: usize,
}

struct WalkCtx<'a> {
    file_idx: usize,
    min_nodes: usize,
    line_starts: &'a [usize],
    candidates: &'a mut Vec<Candidate>,
}

/// Detect Type 1/2 structural clones across `files` by bucketing every subtree
/// of `>= min_nodes` nodes on its identifier-normalized structural key. A group
/// qualifies only when its largest instance spans `>= min_lines` source lines;
/// the line span is checked per group (not per instance) so a reformatted or
/// compacted copy of a clone is not dropped before bucketing. Nested clones
/// fully contained in a larger clone are pruned so only maximal duplications are
/// reported.
pub fn analyze(
    files: &[PathBuf],
    src_dir: &Path,
    min_nodes: usize,
    min_lines: usize,
) -> CloneResult {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut paths: Vec<String> = Vec::new();

    for file in files {
        let Ok(source) = fs::read_to_string(file) else {
            continue;
        };
        let file_idx = paths.len();
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(file).unwrap_or_default();
        let ret = Parser::new(&allocator, &source, source_type).parse();
        let json = ret.program.to_estree_json(true, false);
        let Ok(value) = serde_json::from_str::<Value>(&json) else {
            continue;
        };
        let line_starts = line_starts(&source);
        let mut ctx = WalkCtx {
            file_idx,
            min_nodes,
            line_starts: &line_starts,
            candidates: &mut candidates,
        };
        render(&value, &mut ctx);
        paths.push(display_path(file, src_dir));
    }

    let groups_idx = bucket_groups(&candidates);
    let dominated = nested_dominated(&candidates, &groups_idx);
    build_result(&candidates, &paths, &groups_idx, &dominated, min_lines)
}

/// Bucket candidate indices by their structural (blank) key; keep buckets with
/// 2+ members.
fn bucket_groups(candidates: &[Candidate]) -> Vec<Vec<usize>> {
    let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        buckets.entry(&c.canon_blank).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = buckets.into_values().filter(|v| v.len() >= 2).collect();
    // Deterministic group order independent of HashMap iteration.
    groups.sort_by(|a, b| a[0].cmp(&b[0]));
    groups
}

/// Indices of grouped candidates that are positionally contained within a larger
/// grouped candidate (same file) whose group is at least as large — i.e. clones
/// explained by an enclosing clone.
fn nested_dominated(candidates: &[Candidate], groups_idx: &[Vec<usize>]) -> HashSet<usize> {
    let mut group_size: HashMap<usize, usize> = HashMap::new();
    for g in groups_idx {
        for &ci in g {
            group_size.insert(ci, g.len());
        }
    }
    let grouped: Vec<usize> = {
        let mut v: Vec<usize> = group_size.keys().copied().collect();
        v.sort_unstable();
        v
    };
    grouped
        .iter()
        .copied()
        .filter(|&i| {
            let ci = &candidates[i];
            let gi = group_size[&i];
            grouped.iter().any(|&j| {
                if j == i {
                    return false;
                }
                let cj = &candidates[j];
                cj.file_idx == ci.file_idx
                    && cj.start <= ci.start
                    && ci.end <= cj.end
                    && cj.node_count > ci.node_count
                    && group_size[&j] >= gi
            })
        })
        .collect()
}

fn build_result(
    candidates: &[Candidate],
    paths: &[String],
    groups_idx: &[Vec<usize>],
    dominated: &HashSet<usize>,
    min_lines: usize,
) -> CloneResult {
    let mut groups: Vec<CloneGroup> = Vec::new();
    for g in groups_idx {
        let survivors: Vec<usize> = g
            .iter()
            .copied()
            .filter(|i| !dominated.contains(i))
            .collect();
        if survivors.len() < 2 {
            continue;
        }
        // Gate min_lines at the group level on the largest instance: a clone is
        // reported as long as one copy spans >= min_lines, so a reformatted or
        // compacted sibling does not suppress it.
        let max_span = survivors
            .iter()
            .map(|&i| candidates[i].end_line - candidates[i].start_line + 1)
            .max()
            .unwrap_or(0);
        if max_span < min_lines {
            continue;
        }
        let first_named = &candidates[survivors[0]].canon_named;
        let exact = survivors
            .iter()
            .all(|&i| &candidates[i].canon_named == first_named);
        let node_count = candidates[survivors[0]].node_count;
        let mut instances: Vec<CloneInstance> = survivors
            .iter()
            .map(|&i| {
                let c = &candidates[i];
                CloneInstance {
                    path: paths[c.file_idx].clone(),
                    start_line: c.start_line,
                    end_line: c.end_line,
                }
            })
            .collect();
        instances.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.start_line.cmp(&b.start_line))
        });
        groups.push(CloneGroup {
            instances,
            node_count,
            exact,
        });
    }
    groups.sort_by(|a, b| {
        b.node_count
            .cmp(&a.node_count)
            .then_with(|| a.instances[0].path.cmp(&b.instances[0].path))
            .then_with(|| a.instances[0].start_line.cmp(&b.instances[0].start_line))
    });
    CloneResult { groups }
}

fn render(value: &Value, ctx: &mut WalkCtx) -> Rendered {
    match value {
        Value::Object(map) => match map.get("type") {
            Some(Value::String(ty)) => render_node(map, ty, ctx),
            _ => render_plain_object(map, ctx),
        },
        Value::Array(arr) => {
            let mut named = String::from("[");
            let mut blank = String::from("[");
            let mut count = 0;
            for el in arr {
                let r = render(el, ctx);
                named.push_str(&r.named);
                named.push(',');
                blank.push_str(&r.blank);
                blank.push(',');
                count += r.count;
            }
            named.push(']');
            blank.push(']');
            Rendered {
                named,
                blank,
                count,
            }
        }
        other => {
            let s = other.to_string();
            Rendered {
                named: s.clone(),
                blank: s,
                count: 0,
            }
        }
    }
}

fn render_node(map: &Map<String, Value>, ty: &str, ctx: &mut WalkCtx) -> Rendered {
    let content = CONTENT_TYPES.contains(&ty);
    let mut keys: Vec<&String> = map
        .keys()
        .filter(|k| !matches!(k.as_str(), "type" | "start" | "end" | "range"))
        .collect();
    keys.sort();

    let mut named = format!("{ty}(");
    let mut blank = format!("{ty}(");
    let mut count = 1usize;
    for k in keys {
        let v = &map[k];
        named.push_str(k);
        named.push(':');
        blank.push_str(k);
        blank.push(':');
        if content && CONTENT_KEYS.contains(&k.as_str()) {
            named.push_str(&v.to_string());
            blank.push('*');
        } else {
            let r = render(v, ctx);
            named.push_str(&r.named);
            blank.push_str(&r.blank);
            count += r.count;
        }
        named.push(',');
        blank.push(',');
    }
    named.push(')');
    blank.push(')');

    let start = map.get("start").and_then(Value::as_u64).map_or(0, to_usize);
    let end = map.get("end").and_then(Value::as_u64).map_or(0, to_usize);
    let start_line = offset_to_line(ctx.line_starts, start);
    let end_line = offset_to_line(ctx.line_starts, end);
    if count >= ctx.min_nodes {
        ctx.candidates.push(Candidate {
            file_idx: ctx.file_idx,
            canon_blank: blank.clone(),
            canon_named: named.clone(),
            node_count: count,
            start,
            end,
            start_line,
            end_line,
        });
    }
    Rendered {
        named,
        blank,
        count,
    }
}

/// A `type`-less object (e.g. a regex `{pattern,flags}` payload). Rendered
/// structurally but never recorded as a clone candidate.
fn render_plain_object(map: &Map<String, Value>, ctx: &mut WalkCtx) -> Rendered {
    let mut keys: Vec<&String> = map
        .keys()
        .filter(|k| !matches!(k.as_str(), "start" | "end" | "range"))
        .collect();
    keys.sort();
    let mut named = String::from("{");
    let mut blank = String::from("{");
    let mut count = 0;
    for k in keys {
        let r = render(&map[k], ctx);
        named.push_str(k);
        named.push(':');
        named.push_str(&r.named);
        named.push(',');
        blank.push_str(k);
        blank.push(':');
        blank.push_str(&r.blank);
        blank.push(',');
        count += r.count;
    }
    named.push('}');
    blank.push('}');
    Rendered {
        named,
        blank,
        count,
    }
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Byte offsets at which each source line begins (line 1 starts at offset 0).
fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// 1-based line number for a byte offset.
fn offset_to_line(line_starts: &[usize], offset: usize) -> usize {
    line_starts.partition_point(|&s| s <= offset).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;

    /// Write `files` (name, content) under a temp `src` dir and analyze them.
    fn analyze_files(files: &[(&str, &str)], min_nodes: usize, min_lines: usize) -> CloneResult {
        let tmp = TempDir::new("clone");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(name, content)| {
                let p = src.join(name);
                fs::write(&p, content).unwrap();
                p.canonicalize().unwrap()
            })
            .collect();
        let src = src.canonicalize().unwrap();
        // Leak the temp dir guard for the duration of analysis by keeping it.
        let result = analyze(&paths, &src, min_nodes, min_lines);
        drop(tmp);
        result
    }

    const FN_A: &str = "export function compute(input: number) {\n  const a = input * 2;\n  const b = a + 1;\n  const c = b - 3;\n  return c;\n}\n";
    // Same structure as FN_A, renamed identifiers and changed literals (Type 2).
    const FN_A_RENAMED: &str = "export function evaluate(value: number) {\n  const x = value * 5;\n  const y = x + 9;\n  const z = y - 7;\n  return z;\n}\n";

    // T-601: identical functions in two files form one exact (Type 1) group.
    #[test]
    fn detects_type1_exact_clone() {
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A)], 5, 3);
        assert_eq!(result.groups.len(), 1);
        assert!(result.groups[0].exact, "identical copies are Type 1");
        assert_eq!(result.groups[0].instances.len(), 2);
    }

    // T-602: renamed identifiers/literals match structurally as a Type 2 group.
    #[test]
    fn detects_type2_structural_clone() {
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A_RENAMED)], 5, 3);
        assert_eq!(result.groups.len(), 1);
        assert!(
            !result.groups[0].exact,
            "renamed identifiers/literals are Type 2, not exact"
        );
        assert_eq!(result.groups[0].instances.len(), 2);
    }

    // T-603: a duplicate below the node-count threshold is not reported.
    #[test]
    fn ignores_clone_below_min_nodes() {
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A)], 1000, 1);
        assert!(result.groups.is_empty());
    }

    // T-604: a duplicate below the line-span threshold is not reported.
    #[test]
    fn ignores_clone_below_min_lines() {
        // FN_A spans 6 lines; require 100 lines -> excluded.
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A)], 5, 100);
        assert!(result.groups.is_empty());
    }

    // T-605: structurally different code yields no groups.
    #[test]
    fn no_clone_when_structures_differ() {
        let other = "export function totally(z: string) {\n  if (z.length > 0) {\n    return z.toUpperCase();\n  }\n  return z;\n}\n";
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", other)], 5, 3);
        assert!(result.groups.is_empty());
    }

    // T-606: the same structure across three files is one group of three.
    #[test]
    fn counts_multiplicity_across_files() {
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A), ("c.ts", FN_A)], 5, 3);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].instances.len(), 3);
    }

    // T-607: a duplicated block nested inside a duplicated function is pruned;
    // only the maximal (enclosing) clone is reported.
    #[test]
    fn prunes_nested_clone() {
        // The whole function is duplicated, so its inner block is also a
        // duplicate but fully contained -> a single reported group.
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A)], 3, 1);
        assert_eq!(result.groups.len(), 1, "nested block must not double-count");
    }

    // T-608: groups are ordered by node_count descending.
    #[test]
    fn groups_sorted_by_node_count_desc() {
        let small = "export const f = (p: number) => p + 1 - 2 + 3;\nexport const g = (q: number) => q + 1 - 2 + 3;\n";
        let big_dup = [("a.ts", FN_A), ("b.ts", FN_A)];
        let mut files = vec![("c.ts", small)];
        files.extend_from_slice(&big_dup);
        let result = analyze_files(&files, 5, 1);
        assert!(result.groups.len() >= 2);
        for w in result.groups.windows(2) {
            assert!(w[0].node_count >= w[1].node_count);
        }
    }

    // T-609: instances within a group are ordered by (path, start_line).
    #[test]
    fn instances_sorted_by_path() {
        let result = analyze_files(&[("z.ts", FN_A), ("a.ts", FN_A)], 5, 3);
        assert_eq!(result.groups.len(), 1);
        let paths: Vec<&str> = result.groups[0]
            .instances
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        assert_eq!(paths, vec!["a.ts", "z.ts"]);
    }

    // T-610: a syntactically broken file does not panic and does not block
    // analysis of valid files.
    #[test]
    fn tolerates_syntax_error_file() {
        let broken = "export function ( { { not valid typescript @@@";
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", FN_A), ("c.ts", broken)], 5, 3);
        assert_eq!(result.groups.len(), 1);
    }

    // T-611: reported line numbers match the source location of the clone.
    #[test]
    fn reports_source_line_numbers() {
        // Two leading blank lines push the function to start at line 3.
        let padded = format!("\n\n{FN_A}");
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", &padded)], 5, 3);
        assert_eq!(result.groups.len(), 1);
        let b = result.groups[0]
            .instances
            .iter()
            .find(|i| i.path == "b.ts")
            .unwrap();
        assert_eq!(
            b.start_line, 3,
            "function starts on line 3 after two blanks"
        );
    }

    // T-612: a structurally identical clone is still reported when one copy is
    // compacted below min_lines. min_lines gates the group (max instance span),
    // not each instance, so a reformatted sibling does not drop the bucket.
    #[test]
    fn detects_clone_when_one_copy_is_compacted_below_min_lines() {
        // FN_A spans 6 lines; the compacted copy spans 3 lines (< min_lines 5).
        let compacted = "export function compute(input: number) {\n  const a = input * 2; const b = a + 1; const c = b - 3; return c;\n}\n";
        let result = analyze_files(&[("a.ts", FN_A), ("b.ts", compacted)], 5, 5);
        assert_eq!(
            result.groups.len(),
            1,
            "the clone survives because one copy spans >= min_lines"
        );
        assert_eq!(result.groups[0].instances.len(), 2);
    }
}
