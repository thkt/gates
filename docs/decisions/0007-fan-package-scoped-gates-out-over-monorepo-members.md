---
status: "accepted"
date: 2026-06-30
decision-makers: [thkt]
---

# Fan package-scoped gates out over monorepo members

## Context and Problem Statement

`ProjectInfo::detect` resolves a single `root` to the nearest `.git` ancestor (project.rs:52-57), and every gate runs with `current_dir(&project.root)` while its `condition` probes config files only at that root (tools.rs:843, GATES conditions tools.rs:69-117). In a monorepo where `.git` sits at the outer container and the real `tsconfig.json`/`src` lives one level down (`packages/foo`), the root owns no tsconfig and no `src/`, so `has_tsconfig` is false and `run_graph_gates` early-returns `Skipped` (tools.rs:458-468). The gate silently skips the package that owns the code (#102): tsgo, oxlint, circular, and coupling never see the nested package. This is a false pass, not a fail-open skip — the machinery could have run, it just looked in the wrong directory.

## Decision Drivers

- A nested-package monorepo must get the same gate coverage as a flat repo, with no config change required from the user
- Non-monorepo repos (the common case) must keep byte-identical behavior — zero regression risk
- The hook runs on every edit, so discovery cannot blow the latency budget on a deep tree
- Gates that already resolve the whole workspace from the root (knip) or from their own config (depcruise) must not be double-run

## Considered Options

- Fan the package-scoped gates out over discovered member directories, root-or-members by an either/or discriminator (this ADR)
- Always run both the root and every discovered package (union)
- Make each gate walk down to find its own config at run time
- Require the user to point gates at each package via config

## Decision Outcome

Chosen option: `ProjectInfo::package_targets` (project.rs:68-79) returns the directories the package-scoped gates run against, by an either/or discriminator. When the git root directly owns analyzable code (`has_tsconfig` or a root `src/`), the root is the sole target — today's behavior, so every non-monorepo repo is unchanged. Otherwise the root is a container and discovery descends (bounded `MAX_PACKAGE_DEPTH = 4`, excluding dependency/build dirs) to the member directories that own a `package.json` or `tsconfig.json`, stopping descent once a package matches so a member's own fixtures are not promoted to separate targets (project.rs:90-116). The root and the members are never both run — union would double-report.

Exactly four gates fan out, selected by where their scope is anchored:

- tsgo, oxlint — anchored on the in-directory `tsconfig.json`, marked `per_package: true` (tools.rs:88, 103). They run once per member that owns a tsconfig; a member without one self-skips via the existing `condition`.
- circular, coupling — read each member's own `src/` dependency graph, fanned out in `run_with_overrides` (main.rs:207-231) building one graph per member.

Three gates stay root-anchored: knip resolves workspaces from the root manifest in one pass, depcruise is config-driven, and litmus already uses a recursive `**/*.test.ts` glob from the root (tools.rs:390) that covers every member, so per-package would double-report.

Per-member results are labeled with the member directory relative to the root (`ToolResult::scoped`, runner.rs:98-105, applied at the fan-out site via `scope_result`, main.rs:128-137). Two members that each fail `circular` render the same `✗ circular` header with bodies whose paths are relative to each member's own `src/`, so without the label they collide; the label is added only when the target differs from the root, keeping self-contained output byte-identical.

### Consequences

- Good, because a nested-package monorepo gets full coverage with no user config, closing the #102 false pass
- Good, because the either/or discriminator makes the non-monorepo path a single `[root]` target, so the 261 pre-existing tests pass unchanged — zero regression
- Good, because one thread per gate loops its target list, so the spawned-thread count stays one-per-gate regardless of member count
- Bad, because a root that owns a tsconfig and also nests packages runs only at the root: the discriminator treats a self-contained root as terminal, so members below an analyzable root are out of scope (documented limitation, not yet a real layout)
- Bad, because discovery is a per-edit downward `read_dir` walk bounded by depth and excluded dirs rather than reading the workspace manifest, so a member outside `MAX_PACKAGE_DEPTH` or under an excluded name is missed

### Confirmation

`project::tests` pin discovery: self-contained tsconfig/src roots return `[root]` (the zero-regression invariant), the #102 container layout discovers the tsconfig-owning member, the bug-reproduction test asserts a root-anchored `run_graph_gates` skips while the fan-out detects the cycle, and `node_modules` packages are excluded. `main::tests` pin the orchestration end to end: tsgo and circular fan out into `packages/app` and label the failure `[packages/app]`, two sibling members each rendering their own `✗ circular` block labeled `[packages/a]` and `[packages/b]` (the disambiguation `scoped` exists for), while a self-contained root failure carries no label.

## More Information

### Quality Attributes

Coverage of nested packages is chosen without sacrificing the zero-regression guarantee for flat repos: the either/or discriminator, not a union, is what buys both. The cost is paid in discovery's heuristic reach (depth bound, excluded names) rather than in the common-case path.

### Trade-offs

Downward `read_dir` discovery vs reading the workspace manifest: the manifest (pnpm-workspace.yaml, `workspaces` globs) would be authoritative but adds a parser and per-ecosystem format handling. The bounded walk reuses the existing `EXCLUDED_DIRS` downward-walk pattern (depgraph.rs, snapshot.rs) and finds shallowly-nested members, accepting that a member outside the bound is missed. Either/or vs union: union would cover the analyzable-root-with-nested-members layout this ADR leaves out, but double-reports every flat repo's root and breaks the byte-identical invariant, so the rarer layout is deferred until it appears.
