# Schedule renewal semantics (slice 2)

Scope: source review of generation, persistence, overrides, time interpretation, replacement and failure boundaries. This is not runtime evidence. Line references below refer to the reviewed checkout.

## Observed transitions

| Mode/input | Generated or retained schedule | Renewal/replacement behavior | State/cursor |
|---|---|---|---|
| Classic (`programming.rs:76-80,223-343`) | Stored ordered classic items are expanded into playout parts; mode only selected when classic list is nonempty. | Rebuild builds base, overlays overrides, then replaces timeline. | Classic expansion builds from its definition on each rebuild; no durable cursor is used. |
| Blocks (`timeline.rs:10-15,70-100`; `programming.rs:44-66`) | Pure weekly builder takes plan, injected state and date; fixed UTC offset; dayparts ordered by priority. | Server rebuild creates date from current UTC and rebuilds a week. | Server passes `Default::default()` on every build and discards the returned state (`programming.rs:209-221`); no cursor continuity, including across rebuilds or restarts. |
| Scripted (`scripted.rs:88-128`) | YAML defaults to blocks when no items, classic otherwise; explicit mode supported, mixed items/blocks rejected. | Persists classic items or blocks then calls rebuild as separate operations. | No transaction spans definition write and rebuild. A rebuild failure can leave definition updated while timeline remains old. |
| External (`external.rs:49-132`) | Imported JSON is expanded from resolved start; loop option controls repeat to window. | Timeline replacement and mode update are separate calls. | Replacement transaction is atomic for item deletion/inserts/start_ms, but mode update occurs after it. |
| Manual/overrides (`programming.rs:355-425`; `db.rs:2674-2734`) | Date-bounded override YAML is parsed and applied over base parts; records have priority. | Override CRUD is separate from rebuild; no combined transaction shown. | Rebuild can fail after override mutation; timeline then may represent prior definition. |

## Time, append boundary, restart

- `timeline.rs:10-15` serializes `SchedulerState`; cursors contain position, shuffled bag and bag position. The server passes a fresh default and does not persist the returned state (`programming.rs:209-221`); serialization does not change that behavior.
- `timeline.rs:70-100,416-430` uses plan fixed UTC offset and computes week as seven fixed 24-hour days. This is not a named IANA timezone and does not model DST transitions. `programming.rs:44-66` chooses UTC date for rebuild.
- The timeline builder is a week builder, not a demonstrated append operation. `programming.rs:566-604` (generated) and `db.rs:1376-1415` (external) delete all channel items and insert the supplied set in separate replacement transactions; its boundary is full replacement, not append. No retention of a currently airing row is evident here.
- Overrides are selected by channel and ordered by start date then descending priority (`db.rs:2674-2693`); the overlay selects the highest priority per day (`programming.rs:391-408`). Equal priority selects the last matching row in query iteration; SQL does not specify a complete tie order. The desired tie policy remains a product choice.

## DST/restart and override decision table

| Case | Observed | Safe default recommendation |
|---|---|---|
| Classic | Ordered definition expanded per rebuild. | Preserve deterministic order; do not infer cursor continuity. |
| Block | Weekly generation with fixed offset and state parameter. | Require explicit timezone policy; persist state transactionally before claiming continuity. |
| Scripted | Definition mutation precedes rebuild. | Validate/build first, then atomically commit definition and timeline or retain previous version. |
| External | Replacement is transactional; mode follows separately. | Commit mode with replacement in same transaction; preserve imported absolute timestamps. |
| Manual/override | Date range, priority and YAML stored; rebuild separate. | Define tie/overlap policy; reject ambiguous equal-priority overlap until specified. |
| Append boundary | Current helper replaces complete timeline. | Introduce an explicit cutoff and preserve items before it; never silently replace currently airing content. |
| DST | Fixed offset, 7×24h week. | Prefer named timezone and documented gap/fold policy; until then use UTC/fixed offset explicitly. |
| Restart | Server block builds start with default state and discard the result on every rebuild. | Define and persist cursor transactionally if continuity is required; do not promise current continuity. |

## Open product decisions

1. Is renewal a full future-window replacement or append after a protected cutoff/current item?
2. Must shuffled/sequential slot cursors survive process restart and definition edits?
3. Are schedules tied to fixed offset or named local timezone, and how are nonexistent/repeated wall times handled?
4. Keep highest-priority override per day, and what deterministic rule should resolve equal priorities?
5. Should definition, generated timeline, mode and cursor commit atomically? Recommend yes, with build/validation before mutation and rollback on failure.
6. What should manual edits mean on next renewal: durable exception, next-occurrence override or discarded generated row?

## Sources / limitations

- `crates/awesometv-server/src/programming.rs:44-80,190-221,223-425,566-604`
- `crates/awesometv-scheduler/src/timeline.rs:10-100,416-430`
- `crates/awesometv-server/src/db.rs:1376-1415,2674-2734`
- `crates/awesometv-server/src/scripted.rs:88-128`
- `crates/awesometv-server/src/external.rs:49-168`

Caller trace: `api.rs:1983,2077,2104,2134,2147,2384,2466` invokes rebuild after auto-build, definition/override/template changes, or an explicit request; `scripted.rs:123` invokes it after scripted changes. Repository search for `build_week` and `SchedulerState` finds the server call above, a standalone example and scheduler tests; no other server caller persists its returned state. External rebuild returns without regenerating existing items (`programming.rs:48-55`). These are source observations, not runtime or renewal tests. Product decisions above remain open before slice 4 chooses its contract.

## Integration follow-up (2026-11-18)

Explicit rebuild checks input and timeline revisions under the SQLite write lock before replacement. Concurrent renewal, external replacement, newer rebuild, or input edit can cause a non-mutating conflict; retry with a fresh explicit call. See `wave-2-checkpoint.md` for evidence and the still-open CI gate.
