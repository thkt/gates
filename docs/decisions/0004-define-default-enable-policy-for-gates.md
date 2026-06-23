---
status: "accepted"
date: 2026-06-24
decision-makers: [thkt]
---

# Define the default-enable policy for gates

## Context and Problem Statement

gate enablement is read from the `gates` key in `.claude/tools.json` with a two-tier rule (config.rs:61-66): when no `gates` map exists, all gates are enabled; when a map exists, only gates explicitly set to `true` run (unlisted gates default disabled). README.md:222 documents the current behavior, but not the forward rule: when a new gate is added to the binary, does it run for users who have a `gates` map that predates it? Today the answer is no, the new gate stays off for those users, which is a silent rollout decision a contributor cannot infer. How should a newly-added gate default?

## Decision Drivers

- Zero-config users should get all gates (sensible defaults)
- Users who curated a `gates` map opted into an explicit allow-list
- Adding a gate must not surprise curated-config users with a new blocking check

## Considered Options

- Two-tier opt-out/opt-in: no map → all on; map present → allow-list, new gates off for map users (chosen, current behavior)
- Always-on unless explicitly `false` (opt-out everywhere)
- New gates default-on even when a map exists (announce via CONFIG_HINT)

## Decision Outcome

Chosen option: the existing two-tier policy, made binding, because it preserves "zero config = everything" while honoring an explicit allow-list as a closed set. A newly-added gate is therefore opt-in for users who already maintain a `gates` map: they must add `"<gate>": true` to enable it. This avoids surprising curated-config users with a new blocking gate after an upgrade.

### Consequences

- Good, because a curated `gates` map is a stable contract: an upgrade never adds a blocking gate behind the user's back
- Good, because new users still get full coverage with no config
- Bad, because map users silently miss new gates until they edit config; the new gate's value is invisible to them unless surfaced (CONFIG_HINT is the place to announce it)

### Confirmation

`is_enabled` (config.rs:61-66) and tests `missing_file_enables_all_gates` / `partial_gates_section` pin the two-tier rule. A new gate's default for map users is verified by adding it and confirming an existing partial-map test keeps it off.

## More Information

### Before / After comparison

Before: the rule was implicit in `is_enabled` + a README sentence. After: the rule is binding and the new-gate default (off for map users) is explicit.

### Reassessment Triggers

- A new gate is high-value enough that defaulting it off for map users is judged wrong, motivating an announce-and-default-on mechanism
- The `gates` key gains non-boolean per-gate config, blurring the boolean allow-list model
