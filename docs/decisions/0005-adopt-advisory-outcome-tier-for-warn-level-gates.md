---
status: "accepted"
date: 2026-06-30
decision-makers: [thkt]
---

# Adopt an advisory outcome tier for warn-level gates

## Context and Problem Statement

A gate run resolves to one of four `GateOutcome` variants (runner.rs:13-19): `Passed`, `Failed(_)`, `Skipped`, and `Warned(_)`. `Warned` is the advisory tier — it surfaces on stderr via `reporter::append_advisories` (reporter.rs:71) but is never promoted to a block decision: `is_failure` matches only `Failed` (runner.rs:70-72) and the block JSON is emitted only when `failures` is non-empty (main.rs:270, 279-286), so a `Warned` outcome reaches the agent as stderr text at exit 0. Three sites already mint `Warned` — litmus `Severity::Warning` (tools.rs:441), jscpd above-threshold without the `block` opt-in (tools.rs:685), and the TS2307 downgrade of a would-be `Failed` run (tools.rs:831) — but each carries only local rationale. No rule tells a gate author which tier a new finding belongs in. This is orthogonal to ADR-0001, which governs _can't-run → Skipped_; the advisory tier is _ran, found something, but must not block_.

## Decision Drivers

- A gate author adding a finding needs one canonical "should this warn or block?" rule
- The advisory-vs-block choice spans gate code, reporter, and the exit-code surface — no single type or test encodes the _author-facing decision_
- Low-confidence or whole-run-poisoning findings (TS2307 → `any` inference) should inform without forcing the agent to act

## Considered Options

- Adopt a three-way tier-selection rule binding every gate (this ADR)
- Keep only `Passed`/`Failed`/`Skipped`; force every finding to block or be dropped
- Encode tiers per-gate with no shared rule (status quo: three local rationales)

## Decision Outcome

Chosen option: a binding tier-selection rule, because the type system enforces _behavior_ (a `Warned` never blocks) but cannot enforce the _choice_, and three independent local derivations already invite drift. The rule for any gate:

- **Skipped** — the gate could not run (missing binary, timeout, panic, parse failure). Per ADR-0001.
- **Warned** — the gate ran but the finding is advisory: low confidence, opt-in severity, or a result that would poison a confident verdict (e.g. an unresolved import degrading type inference). Reaches the human via stderr, never blocks the agent.
- **Failed** — the gate ran and found a violation it is confident should block the edit cycle.

`Warned` deliberately decouples the advisory tier from the reserved `HookExitCode::Advisory(1)`: advisory findings travel via stderr at exit 0, and `Advisory(1)` stays dormant unless a future hook-spec change routes advisories to a distinct exit code (hook_exit.rs:29-34, issue #18).

### Consequences

- Good, because a new gate has one decision rule instead of three precedents to reverse-engineer
- Good, because warn-level findings (duplication, low-confidence type noise) inform the agent without false-blocking its edit loop
- Bad, because the tier is an author judgment the compiler cannot check: a gate that should block but emits `Warned` silently lets a violation through, caught only by review
- Bad, because the advisory/exit-code decoupling means `HookExitCode::Advisory(1)` is permanently dormant on the current hook path; the stale "every gate today is blocking" comment (hook_exit.rs:25) is a separate fix

### Confirmation

Code review of any new gate confirms its outcome tier matches the rule above. Behavior is pinned by `is_failure` matching only `Failed` (runner.rs:70-72), the block JSON keyed on non-empty `failures` (main.rs:270, 279-286), and `reporter::append_advisories` rendering `Warned` as a yellow advisory block that counts as ran-but-not-passed (reporter.rs:45, 71).

## More Information

### Quality Attributes

Agent edit-loop continuity (advisory findings never force action) is chosen over completeness (every finding blocks). Mirrors ADR-0001's reliability-over-completeness trade at a different layer: ADR-0001 protects against gate _machinery_ failure, this ADR against over-blocking on low-confidence _results_.

### Trade-offs

`Warned` vs forcing every finding to `Failed`: a finding the gate is not confident about (TS2307 cascade, opt-in jscpd) would otherwise either spam blocks or be dropped entirely. The advisory tier keeps the signal while leaving the act-or-ignore choice to the agent. The cost is one more outcome the author must reason about, and a tier choice review cannot mechanically verify.
