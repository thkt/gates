---
status: "accepted"
date: 2026-06-30
decision-makers: [thkt]
---

# Adopt a mirror-by-default policy for surfaces shared with sister hooks

## Context and Problem Statement

gates is the `PostToolUse` half of a hook role-pair: guardrails (`PreToolUse`, the lightweight pre-edit guard) and formatter run alongside it on the same edits, and OUTCOME.md:27-30 records the split (guardrails = pre-edit gatekeeper, gates = post-edit full audit). Five surfaces already encode a per-site choice to either mirror a sibling or deliberately diverge: exit-code convention mirrors guardrails (hook_exit.rs:1-2), reporter separator widths mirror guardrails (reporter.rs:4), terminal color detection mirrors guardrails `io/color.rs` (color.rs:6-12), the `$HOME` ancestor fence mirrors formatter but diverges from guardrails (traverse.rs:8-12), and config loading diverges to fail-open where guardrails/formatter fail-closed (config.rs:73-75). Each site states its own decision, but no rule tells a future author whether a _new_ shared surface should mirror or diverge. The coordination is a cross-repo policy that lives in no single place.

## Decision Drivers

- A user sees gates, guardrails, and formatter output on the same edit; gratuitous presentation differences read as bugs
- Some divergences are load-bearing (gates' fail-open, guardrails' `OutsideProjectRoot` forensic boundary) and must not be "unified" away
- The rule cannot live in a comment, type, or test — it is a meta-rule spanning three repos

## Considered Options

- Mirror-by-default with documented role-based divergence (this ADR)
- A fixed taxonomy (e.g. "always mirror presentation, always diverge on safety")
- No rule: each surface decides ad hoc (status quo)

## Decision Outcome

Chosen option: **mirror is the default for any surface shared with a sister hook (protocol or presentation); divergence requires a documented justification grounded in gates' post-edit-audit role.** A fixed taxonomy was rejected because it does not survive its own sites: the `$HOME` fence diverges from guardrails for a _role_ reason (preserving guardrails' forensic signal), not a safety/presentation axis, so any category line drawn would have to carve out exceptions. The burden-of-proof framing fits all five precedents: mirror unless the author records why gates' role demands otherwise.

### Consequences

- Good, because a new shared surface has a default (mirror) and a single bar for divergence (write down the role-based reason), instead of an unanchored case-by-case call
- Good, because the load-bearing divergences (config fail-open, the guardrails fence boundary) are framed as justified exceptions, not drift to be reconciled
- Bad, because "mirror" is maintained by hand across separate repos with no shared library — a sibling changing its separator width or exit convention silently desyncs gates until someone notices
- Bad, because the justification bar is review-enforced prose, not a mechanism; an undocumented divergence is indistinguishable from an intentional one

### Confirmation

Code review of any new or changed surface shared with guardrails/formatter confirms it either mirrors the sibling or carries a comment naming the post-edit-audit reason it diverges. The five current precedents (hook_exit.rs:1-2, reporter.rs:4, color.rs:6-12, traverse.rs:8-12, config.rs:73-75) each already carry such a comment and serve as the worked examples.

## More Information

### Quality Attributes

Cross-tool consistency (the role-pair presents as one coherent toolchain) is chosen as the default, with gates' role-specific correctness (fail-open audit, the formatter-aligned fence) as the explicit, justified override. Consistency is the floor; a divergence must buy something the role needs.

### Trade-offs

Mirror-by-default vs extracting a shared crate: a shared library would mechanically enforce the mirror but couple three independently-released hooks at the build level. The cheaper choice is a documented policy plus hand-maintained parity, accepting silent-desync risk until a parity check (or shared crate) earns its complexity. config fail-open is the canonical divergence and is recorded in ADR-0001; this ADR cites it as a precedent rather than re-documenting it.
