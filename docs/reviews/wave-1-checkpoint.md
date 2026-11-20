# Wave 1 integration checkpoint

2026-11-18 · branch `review/application-feature-scorecard-2026-11-18` · base `f486be4`. Slices 1 and 2 accepted; fixture ingest evidence and source review only. Playback and renewal were not tested.

## Scope and artifacts

Baseline: modified `docs/TESTING.md`; untracked `docs/plans/`, `docs/reviews/`, `scripts/gen-wave1-fixtures.sh`, and `scripts/validate-wave1-fixtures.py`. Preserve all other untracked files. Deliverables: `scripts/gen-wave1-fixtures.sh`, `scripts/validate-wave1-fixtures.py`, `docs/TESTING.md`, `docs/reviews/schedule-renewal-semantics.md`, this checkpoint, and wave status in `docs/plans/application-quality-improvement-plan-2026-11-18.md`. The fixture scripts use `scripts/` rather than the originally proposed `tests/fixtures/`; the documented interface uses those actual paths.

## Existing evidence

`docs/TESTING.md` records ffmpeg 8.0.1-3ubuntu2, two independent clean-directory runs of `scripts/gen-wave1-fixtures.sh "$dir"` and `scripts/validate-wave1-fixtures.py "$dir"`, each passing the fixed metadata, decoded colors and tone checks. It records `cargo build --offline -p awesometv-server` and the documented start/login/scan/channel commands exercised on port 18419: two 4000 ms H.264/AAC media items and channel 99 `Fixture`. The record is a human-readable summary, not retained raw logs or media. It establishes fixture identity and ingest, not byte-identical output across ffmpeg versions. Successful checks were not repeated.

## Parent review

Read both scripts and compared their fixed expected duration, titles, codecs, dimensions, frame rate, decoded dominant color and measured audio frequencies: validation independently checks each generated directory against the same expectations, making the two recorded passes meaningful. Reviewed `docs/TESTING.md` commands against `config.rs:51-103`, `api.rs:349,435,439,472,1398-1462,1791-1814` and `scan.rs:18-49`: environment, routes, request/response fields, sorted scan and seed flow agree. The recorded run provides execution evidence; source inspection is not a rerun.

Traced `build_week`, `SchedulerState`, rebuild callers, generated and external transaction boundaries, and override selection. Updated `docs/reviews/schedule-renewal-semantics.md`: server block builds use fresh default state and discard returned state; priority selection is observable, equal-priority ordering is not fully specified. `git diff --check` passed during review; final scope and whitespace checks follow this checkpoint. No production files edited.

## Next-wave gate

Slice 3 may begin after the parent maps its exact CI, fixture and browser ownership and confirms the isolated server lifecycle. Before slice 4 implementation, agree the protected append boundary, cursor continuity across rebuild/restart/edits, timezone and DST policy, equal-priority override rule, atomicity and manual-edit behavior; the plan's defaults are proposals. If the chosen contract cannot preserve a mode, split the prerequisite as slice 4's stop criterion requires. No decision is needed to accept the source-review slice itself.
