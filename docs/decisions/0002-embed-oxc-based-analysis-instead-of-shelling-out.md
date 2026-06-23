---
status: "accepted"
date: 2026-06-24
decision-makers: [thkt]
---

# Embed oxc-based analysis instead of shelling out to external tools

## Context and Problem Statement

The circular-dependency, coupling, and clone-detection gates analyze the project's TypeScript dependency graph. External tools already do this (madge, dependency-cruiser, dpdm). gates instead parses TS/TSX in-process with oxc (oxc_parser/ast/span/allocator) and builds one shared dependency graph reused by circular and coupling (depgraph.rs:3-13). The rationale lived only in an ephemeral research note (`.claude/workspace/research/2026-06-21-oxc-coupling-metrics.md`), which will be pruned. How is this architecture preserved for future readers?

## Decision Drivers

- The hook fires on every edit, so startup cost and per-run latency matter (OUTCOME 60s budget)
- Shelling out adds N node process spawns + N redundant parses per run
- gates already depends on oxc; adding madge/dependency-cruiser pulls a node toolchain

## Considered Options

- Embed analysis on oxc, parse once, reuse the graph across circular + coupling (chosen)
- Shell out to madge --circular / dependency-cruiser / dpdm per gate
- Mixed: embed coupling/clone, shell out circular

## Decision Outcome

Chosen option: embed on oxc with a single shared parse, because it removes per-gate process spawns and redundant parses, keeps the dependency surface to oxc (already present), and gives full control over detection semantics. The cost is owning the analysis algorithms ourselves.

### Consequences

- Good, because one parse per hook invocation feeds both circular and coupling (depgraph.rs:11-13), cutting latency
- Good, because detection precision (an OUTCOME Indicator) is ours to tune rather than bounded by an external tool's output
- Bad, because we own algorithm correctness: e.g. circular uses three-color DFS back-edge detection (circular.rs:27-99), which reports back-edge cycles rather than minimal SCCs, and module resolution honors only `.`-relative + .ts/.tsx/index, not tsconfig path aliases (depgraph.rs:131-157)

### Confirmation

Cargo.toml lists oxc\_\* as the only parser dependency; no node-based analysis tool appears in CI or runtime. Tests T-1xx (depgraph), T-3xx (coupling), and the circular/clone suites pin the embedded behavior.

## More Information

### Trade-offs

Three-color DFS over Tarjan/Johnson SCC: simpler to implement and sufficient for "is there a cycle" reporting, accepting that overlapping cycles may merge or be reported as back-edges rather than minimal SCC sets.

### Reassessment Triggers

- An external tool gains a feature (e.g. full tsconfig path resolution) that the embedded analyzer cannot match at acceptable cost
- Detection-precision complaints trace to the back-edge-vs-SCC choice and an SCC algorithm becomes worth the complexity
