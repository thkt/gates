---
status: "accepted"
date: 2026-06-24
decision-makers: [thkt]
---

# Commit to a stable audit.jsonl on-disk schema

## Context and Problem Statement

Every gates run appends one JSON object per line to `$XDG_DATA_HOME/gates/audit.jsonl` with the shape `{ts, project, decision, failed[]}` (audit.rs:14-21). `gates show --json` reads these lines back and emits them as a machine-readable array for AI agents and external readers. The schema is a persisted wire format, but no rule records that fields are stable: renaming or removing a field silently breaks every previously-written log file, and there is no migration path. How do we treat this schema as a compatibility commitment rather than an internal struct?

## Decision Drivers

- `gates show --json` is an agent-facing public API (OUTCOME.md:14)
- Old log lines persist on disk across gates upgrades; there is no rewrite step
- serde will silently drop unknown fields and error on missing required ones

## Considered Options

- Treat the schema as a stable public contract: additive-only evolution with serde defaults (chosen)
- Leave the struct internal and change it freely
- Version the schema with a `v` field and migrate on read

## Decision Outcome

Chosen option: stable public contract with additive-only changes, because the file outlives the binary that wrote it and consumers parse it directly. New fields must carry `#[serde(default)]` so old lines still deserialize; existing fields (`ts`, `project`, `decision`, `failed`) must not be renamed or removed. Explicit versioning was rejected as premature: additive-only evolution covers the foreseeable changes without a migration engine.

### Consequences

- Good, because old `audit.jsonl` files remain readable by `gates show` after upgrades
- Good, because external readers can rely on the four field names
- Bad, because the four current fields are now frozen: changing them requires a superseding ADR and a documented migration

### Confirmation

Any change to `AuditEntry` (audit.rs:14-21) is reviewed against this ADR: additions carry `#[serde(default)]`, removals/renames are rejected. The `query_skips_corrupt_lines` test confirms unreadable lines degrade rather than abort.

## More Information

### Quality Attributes

Backward compatibility of persisted data over struct-evolution convenience. The append path (audit.rs:37-50) writes one line atomically within PIPE_BUF; an entry exceeding PIPE_BUF loses cross-process atomicity (separate concern, noted for future bounding).

### Reassessment Triggers

- A breaking field change becomes unavoidable, requiring a `v` version field + read-time migration (supersede this ADR)
- `failed[]` lists grow large enough to exceed PIPE_BUF and tear concurrent appends
