# ADR 0003 — Scheduler model

Status: accepted
Date: 2026-11-14

## Context

ErsatzTV exposes five scheduling models with overlapping concepts. Deep parity is required, but the implementation must be testable, deterministic, and understandable.

## Decision

Use a **single pure timeline builder** with a small mental model:

```
Channel -> Plan -> Daypart -> Slot -> Candidates
```

- `Plan` is a weekly repeating plan with time zone, dayparts, priorities, date ranges, and fallback.
- `Daypart` is a local-time window with one or more slots.
- `Slot` selects a source, ordering, fill mode, filler, mid-roll policy, EPG grouping, graphics, and watermark behavior.
- Advanced ErsatzTV modes are represented as slot/plan variants, not separate schedulers.
- The builder is a pure function: `build(plan, state, seed, from, to) -> (items, state_delta, warnings, coverage)`.
- It has no database, wall clock, or global RNG. Time, IDs, and candidate resolution are injected.
- Published timelines are immutable; edits create a new version and publish at a safe boundary.
- State is explicit: cursors, shuffle bags, linked-slot groups, history, and cooldowns.
- Guardrails: bounded iterations, skip/trim policies, no infinite loops, warnings for impossible constraints.

## Consequences

- Classic, block, sequential, scripted, and external playouts become front-ends or plan compilers over the same timeline contract.
- Property tests can prove no overlaps, monotonic time, determinism, and coverage.
- The UI can preview and simulate a plan without touching playback.
- Deep parity will require careful mapping of every ErsatzTV option into this model; that mapping lives in the parity checklist.
