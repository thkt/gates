---
status: "accepted"
date: 2026-06-24
decision-makers: [thkt]
---

# Adopt fail-open invariant for all gates

## Context and Problem Statement

gates runs as a PostToolUse hook on every AI edit. A gate that throws, times out, finds no binary, or panics must never block the agent's edit cycle. Today this behavior is implemented ad hoc at each gate site (tools.rs:48,619; main.rs:96,300,489; config.rs:68; audit.rs:1-3; runner.rs:13) and stated as a fact in OUTCOME.md:49, but no rule obliges a future-added gate to follow it. A new contributor can write a gate that returns `Result` and propagates an error, silently breaking the never-block contract. How do we make fail-open a binding rule rather than a coincidence of the current code?

## Decision Drivers

- The agent's edit loop must continue even when a gate's machinery fails
- Tools cannot enforce this: no type or lint can detect "this gate can block"
- The invariant spans 6+ sites and every future gate

## Considered Options

- Document the forward rule as an ADR (this record) plus funnel all gate error paths through `runner::GateOutcome::Skipped`
- Rely on OUTCOME.md statement-of-fact only (status quo)
- Introduce a type-level guarantee (e.g. a gate trait whose only error variant maps to Skipped)

## Decision Outcome

Chosen option: ADR + funnel-to-Skipped convention, because the invariant is not tool-enforceable and a binding rule with a single error sink is the cheapest way to bind every future gate. A type-level guarantee was rejected as premature: the gate set is dispatched by hand-written spawn arms today, and a trait abstraction would add indirection before a second enforcement need exists.

### Consequences

- Good, because every new gate has one rule to follow: any error/timeout/panic/missing-binary path resolves to `GateOutcome::Skipped`, never a propagated error or non-zero hook exit
- Good, because the agent's edit cycle is protected by an explicit contract, not scattered precedent
- Bad, because enforcement stays manual (code review), so a gate that crashes the process (e.g. stack overflow, see Reassessment Triggers) can still bypass it

### Confirmation

Code review of any new gate: confirm every error path returns `GateOutcome::Skipped` and the gate's thread is joined through `main::join_into` with fallback names. Existing tests `invalid_json_enables_all_gates`, `query_skips_corrupt_lines`, and the panic→skipped join tests guard the established sites.

## More Information

### Quality Attributes

Reliability (the agent is never blocked by gate failure) is chosen over completeness (a failed gate produces no signal that run). This is the deliberate trade: a missed gate run is recoverable on the next edit; a blocked agent is not.

### Reassessment Triggers

- An uncatchable abort (stack overflow / SIGABRT) is found to bypass the Skipped funnel and kill the hook process (tracked as a bug-fix, not a reversal of this ADR)
- A second gate-enforcement need appears, justifying a gate trait that encodes fail-open at the type level
