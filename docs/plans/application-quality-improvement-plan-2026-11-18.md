# Application Quality Improvement Implementation Plan

**Goal:** Raise each of the eight areas in the 2026-11-18 scorecard toward a defensible 9–10 through narrowly scoped improvements and observed evidence.

**Architecture:** Establish an isolated, reproducible media and clock harness, then repair continuity and atomicity before verifying the complete source-to-client path. Each numbered slice has one owner and one observable outcome; source paths are likely ownership boundaries and must be confirmed before edits. Review each slice independently against its acceptance evidence.

**Tech stack:** Rust/Tokio, SQLite/sqlx, ffmpeg/ffprobe, HTML/JS, Playwright, Docker and GitHub Actions.

**Basis:** `docs/reviews/application-scorecard-2026-11-18.md`, server source, existing E2E configuration and CI. The scorecard identifies source-based risks, not observed seven-day outages; existing CI runs Rust tests and builds an image, but the reviewed browser tests require a populated external instance. No score increase is guaranteed by a code change. Preserve current work and adapt each slice to its checkout. Effort is relative: S ≤ 1 day, M 1–3 days, L 3–5 days; split an L slice if discovery exceeds its boundary.

## Working contract and proposed defaults

Each implementation slice starts with a focused failing behavior test, then the smallest change and its focused passing test. Record commands, environment, fixture and results in the review; retain logs, screenshots and traces on failure. Avoid real user data, real credentials and public services in CI. Existing tests should remain valid. Scope boundaries below are exclusive between workers in the same batch; tests are owned by their corresponding slice, and shared files must be sequenced. A likely path is not authorization to redesign an adjacent subsystem.

Proposed defaults pending product decisions: keep at least 7 days of generated guide, renew when below 5 days, check hourly and at startup; retain already published/current items and append from the exact persisted boundary; retry failures with bounded backoff and expose stale horizon; atomic all-or-nothing auto-build; sync every 6 hours with manual refresh and no deletion on incomplete scans; server-side validation is authoritative; compatibility claims list tested client versions. These are planning assumptions, not verified existing behavior. Confirm horizons, refresh cadence/deletion grace, supported clients/sources, and accessibility target with the product owner before release; do not block independent foundation work.

## Dependency and execution map

| Slice | Outcome | Depends on | Priority | Effort |
|---|---|---|---|---|
| 1 | Repeatable local media fixture | — | P0 | M |
| 2 | Schedule renewal semantics established | — | P0 | S |
| 3 | Isolated browser CI executes fixture flow | 1 | P0 | M |
| 4 | Generated horizon renews safely | 1, 2 | P0 | L |
| 5 | Renewal survives failure and restart visibly | 4 | P0 | M |
| 6 | Auto-build rejects invalid plans | 1 | P1 | M |
| 7 | Auto-build commits atomically | 6 | P1 | M |
| 8 | Source support and refresh behavior established | — | P1 | S |
| 9 | Local scan reconciles safely | 1, 8 | P1 | M |
| 10 | Remote sync recovers safely | 8 | P1 | M |
| 11 | Guide agrees with timeline across renewal | 3, 4, 5 | P1 | M |
| 12 | Stream follows timeline through transitions | 1, 4, 5 | P1 | M |
| 13 | Playback recovers from media/producer failure | 12 | P1 | M |
| 14 | External clients have recorded compatibility | 11, 12, 13 | P1 | M |
| 15 | Settings reject invalid values | 3 | P2 | M |
| 16 | Studio forms explain errors and state | 15 | P2 | M |
| 17 | Watch is usable across viewports and input modes | 3 | P2 | M |
| 18 | Studio is usable across viewports and input modes | 16 | P2 | M |
| 19 | Persistent container restart and upgrade proven | 3 | P1 | M |
| 20 | Backup and restore proven | 19 | P1 | M |

## Primary execution schedule

The parent orchestrator assigns two named lanes, A and B, with **at most two concurrent subagents total**, including a verifier. Each lane uses one GPT-6-Luna worker at low effort for its bounded slice; no nested delegation. The parent resolves decisions and integrates. A wave starts only after its listed predecessors have passed their acceptance gates and the prior wave checkpoint is accepted. A blocked slice is not a completed predecessor: pause its dependents, preserve evidence, and replan eligible independent work without duplicating a slice in the primary schedule. Product defaults above remain assumptions until confirmed for release.

| Wave | Lane A: slice (predecessors) | Lane B: slice (predecessors) | Readiness and ownership gate |
|---|---|---|---|
| 1 | 1 (—) | 2 (—) | Fixture/docs versus semantics review; agree fixture interface and flag missing codec or unresolved mode decisions. |
| 2 | 3 (1) | 4 (1, 2) | Fixture and semantics accepted; CI/E2E versus scheduler, `programming.rs` and `db.rs`. Slice 3 requests any server hook from the parent; it does not edit slice 4 files. |
| 3 | 5 (4) | 6 (1) | Renewal append accepted; renewal worker/`main.rs` versus auto-build `api.rs`. |
| 4 | 7 (6; also after 4 for ownership) | 8 (—) | Validation accepted and slice 4 writes integrated; transactional `api.rs`/`db.rs`/`programming.rs` versus read-only source research and its review document. |
| 5 | 9 (1, 8) | 11 (3, 4, 5) | Source matrix and renewal evidence accepted; local scan and its DB/API functions versus guide-specific tests only. A guide defect returns to its owning slice. |
| 6 | 10 (8) | 12 (1, 4, 5) | Supported-source semantics and fixture/renewal ready; remote provider/worker files versus playback harness and, only if proven necessary, `engine.rs`. No shared bootstrap or test-helper writes concurrently. |
| 7 | 13 (12) | 15 (3) | Stream transition evidence accepted; `engine.rs`/producer lifecycle in `main.rs` versus settings `api.rs`/`config.rs`. |
| 8 | 14 (11, 12, 13) | 16 (15) | Guide, playback recovery and settings contract accepted; external-client evidence document versus Studio `web/index.html` and distinct Studio specs. Availability gates apply to 14. |
| 9 | 17 (3) | 19 (3) | Fixture CI accepted; Watch `web/public.html` and Watch specs versus container scripts/configuration and deployment docs. Prior image availability gates 19. |
| 10 | 18 (16) | 20 (19) | Studio form changes and upgrade evidence accepted; Studio `web/index.html`/specs versus `backup.rs`, backup tests and backup docs. |

**Ownership matrix:** 1 fixture generator/`docs/TESTING.md`; 2 renewal review; 3 CI and isolated browser fixture specs; 4 scheduler/`programming.rs`/`db.rs`; 5 renewal worker/`main.rs`; 6 auto-build validation in `api.rs`; 7 auto-build commit in `api.rs`/`db.rs`/`programming.rs`; 8 source review; 9 local `scan.rs` and local scan DB/API functions; 10 remote provider and sync worker modules; 11 guide-only specs; 12 playback spec/harness and narrowly justified `engine.rs`; 13 `engine.rs` and producer lifecycle; 14 client evidence; 15 settings `api.rs`/`config.rs`; 16 Studio forms/Studio specs; 17 Watch page/Watch specs; 18 Studio page/Studio specs; 19 container deployment files/test/docs; 20 backup module/test/docs. Focused tests belong to their slice, with exact paths reserved before launch. All other paths are read-only unless the parent explicitly reassigns them.

**Shared-file rule:** The parent maps exact existing and new paths before each launch, including shared test helpers, workflow files, `main.rs`, `api.rs`, `db.rs`, `programming.rs` and documentation. The matrix is a boundary proposal, not proof of disjoint paths. If both slices need a path, give it to one writer and defer the other edit to a later wave or parent integration; do not launch overlapping writers or invent separate ownership by line. In particular, 4 precedes 7, 6 precedes 7, 12 precedes 13, 15 precedes 16, and 16 precedes 18; serial work is unavoidable for their shared contracts/files. Slice 5 and 13 also serialize `main.rs`; 7, 9 and 15 serialize possible `api.rs`/`db.rs` changes. If 10 needs `main.rs` or a helper owned by 12, defer that edit or serialize the wave. Shared runtime resources and fixture directories must also be isolated.

**Integration checkpoint after every wave:** wait for both lanes to finish or stop; inspect their scoped diffs and evidence, reconcile contracts and shared files, and record completed/blocked slice IDs, exact paths, commands, results, artifacts and next ready wave. Run each slice's narrowly required acceptance checks once by its owner and retain exact command/output; the parent checks that evidence and performs only missing cross-slice integration checks. If independent verification is needed, launch **one read-only verifier for the whole wave only after workers stop**, within the two-agent cap; give it the evidence and ask it to run only missing or disputed checks, not repeat successful tests. The parent accepts the checkpoint; failures reopen the relevant slice and block dependents. No score claim follows from implementation alone.

**Assignment template:** `OBJECTIVE: slice <ID> and its existing outcome. OWNED PATHS: <exact reserved files/globs>; READ-ONLY: <contracts and predecessor evidence>. PREDECESSORS: <IDs and accepted checkpoint>. ACCEPTANCE: <the slice's existing checks, exact focused command(s), fixture/environment and expected observations>. CONSTRAINTS: preserve others' work; no writes outside ownership, nested agents, installs, commits or pushes; stop on shared-file conflict or slice stop condition. EVIDENCE: scoped diff, commands/results, artifacts, unresolved assumptions and handoff. BUDGET/STOP: <deadline or bounded work>; return partial evidence when blocked.` The parent fills exact paths and commands after inspecting the checkout; workers do not silently broaden scope. Use Luna low for bounded implementation and tests. Terra medium is permitted only for unresolved ambiguity that prevents a bounded assignment or after a Luna correctness failure; state which criterion and first fix an oversized or unclear scope. Unavailable models are reported, not substituted silently; no priority tier.

**Stop and handoff:** Stop on a slice's stated blocker, missing prerequisite, conflicting ownership, unavailable required environment, repeated failure without new evidence, or work exceeding its boundary. Report goal, current state, changed paths, evidence and failures, constraints, and next action; preserve all edits. The parent resolves or records the blocker before dependent work resumes. No agent delegates further.

**First launch-ready batch:** After the parent confirms exact paths and fixture tools without installing anything, launch Luna low lane A for slice 1 (`tests/fixtures/`, `docs/TESTING.md`) and Luna low lane B for slice 2 (only `docs/reviews/schedule-renewal-semantics.md`). Use the template and each slice's unchanged acceptance and stop criteria. Neither writes server code; integrate both evidence bundles before wave 2. If the fixture tool is unavailable or semantics require a product decision, record the affected blocker and do not start dependent work.

**Wave 1 status:** slices 1 and 2 accepted on recorded fixture evidence and parent source review; see `docs/reviews/wave-1-checkpoint.md`. Wave 2 slice 3 may start after ownership mapping. Slice 4 requires agreement on the renewal contract and its stop criteria before implementation.

**Wave 3 integration status:** slices 5 and 6 accepted; integration gate closed on focused and isolated live lifecycle evidence in `docs/reviews/wave-3-checkpoint.md`. Wave 4 slice 7 may proceed with its atomic publication and allocation scope. Wave 2 slice 3 exact CI proof remains open and is not waived.

**Wave 4 integration status:** slices 7 and 8 accepted with documented evidence and limits; see `docs/reviews/wave-4-checkpoint.md`. Wave 5 slice 9 is eligible for independent replanning; slice 11 remains blocked on Wave 2 slice 3 exact CI provisioning proof. The identified cloud risks carry to slice 10 in Wave 6.

**Wave 5 integration status:** slice 9 accepted with documented limits; the repeated-broadcast projection defect is fixed and local slice 11 guide checks pass. The earlier live guide evidence and fresh fixed-clock checks are recorded independently of the open slice 3 hosted CI provisioning prerequisite. Wave 6 work may be scoped against its own prerequisites; the full guide-dependent gate is not accepted. See `docs/reviews/wave-5-checkpoint.md`.

**Wave 2 integration status:** slice 4 accepted on focused SQLite evidence, including guarded rebuild concurrency. Slice 3 and the Wave 3 ready gate remain open: the exact browser-smoke CI install/build/run job has not run in its entirety locally or on GitHub Actions. See `docs/reviews/wave-2-checkpoint.md` for local smoke evidence, environment differences, and the Wave 3 caller contract.

## Numbered slices

### 1. Deterministic media fixture
- **Outcome:** A fresh temporary instance can ingest two identifiable, short generated clips without external media.
- **Own:** new `tests/fixtures/` generator and fixture setup documentation in `docs/TESTING.md`; coordinate server test hooks with slice 4. **In:** pinned ffmpeg parameters, isolated temp data, clear cleanup and fixed media identity. **Out:** production media, live clients, CI changes.
- **Prerequisites:** none. **Checks/evidence:** document exact generation/start/seed commands; run twice in clean directories, assert ffprobe duration, distinguishable audio/video and identical expected metadata; report ffmpeg version and generated artifact checks. Avoid committing licensed media. **Effort:** M. **Stop:** required codec/binary unavailable or deterministic identity cannot be reproduced; report dependency instead of using a live library.

### 2. Establish renewal semantics
- **Outcome:** A reviewed contract identifies which persisted modes are generated and how cursor, overrides, time zones and existing timeline replacement behave.
- **Own:** a new `docs/reviews/schedule-renewal-semantics.md` only. **In:** inspect `programming.rs`, scheduler `timeline.rs`, `db.rs`, `scripted.rs`, `external.rs`, and callers; list actual transaction boundaries and clock assumptions. **Out:** code changes or claims of observed failure.
- **Prerequisites:** none. **Checks/evidence:** cite source locations and a state-transition table covering classic, block, scripted, external and manual timelines; specify append boundary, DST/restart and override treatment, and unresolved questions. **Effort:** S. **Stop:** any mode's ownership or stable cursor cannot be determined without a product decision; surface it before slice 4.

### 3. Fixture-backed browser CI
- **Outcome:** A pull request runs one isolated source-to-guide browser smoke test without a pre-seeded server.
- **Own:** `playwright.config.ts`, `tests/e2e/` setup and new fixture smoke spec, `.github/workflows/ci.yml`, `docs/TESTING.md`. **In:** start/health-check/stop a temporary server and fixture; bounded waits; test credentials generated only for the run; failure artifacts. **Out:** broad UI rewrites and real client certification.
- **Prerequisites:** 1. **Checks/evidence:** execute the exact CI job locally or in CI from a clean database; assert fixture title appears in guide, collect server logs/trace on failure, verify teardown even on failure; CI job must fail on missing server/fixture. **Effort:** M. **Stop:** server cannot be isolated or secrets would be exposed; document blocker.

### 4. Rolling generated horizon
- **Outcome:** Generated classic and block channels maintain a continuous future horizon without changing the currently playing or already published timeline.
- **Own:** `crates/awesometv-server/src/programming.rs`, `db.rs`, scheduler modules as needed, focused Rust tests. **In:** injectable clock, durable cursor/boundary, atomic append and concurrency protection; honor overrides and confirmed mode semantics. **Out:** background triggering and external/manual schedule mutation.
- **Prerequisites:** 1, 2. **Checks/evidence:** controlled-clock tests cross seven-day boundary, restart and DST; assert no gaps/overlaps, stable current item, deterministic cursor, retained overrides, idempotent concurrent renewal and rollback on injected persistence failure. Run focused server/scheduler tests and record commands. **Effort:** L. **Stop:** append semantics cannot preserve an existing mode or a migration requires a product decision; split migration before proceeding.

### 5. Renewal recovery and visibility
- **Outcome:** A generated channel renews after startup or transient failure and reports when its future horizon is at risk.
- **Own:** `crates/awesometv-server/src/main.rs`, new renewal worker module and its focused tests; expose status through structured logs/metrics within that module. **In:** startup and periodic trigger, single-flight, bounded retry, last success/error/horizon health; graceful shutdown. **Out:** changing generation algorithm or alert delivery integrations.
- **Prerequisites:** 4. **Checks/evidence:** fake-clock/failure test demonstrates failed run leaves old timeline intact, retry succeeds, restart catches up, duplicate ticks do not duplicate entries, and unhealthy status clears after recovery. Record observed status and focused test output. **Effort:** M. **Stop:** no safe worker lifecycle or durable status can be added within ownership; identify prerequisite.

### 6. Validate auto-build plans
- **Outcome:** Preview and create reject invalid selected collections or conflicting channel numbers before making changes.
- **Own:** auto-build planning/validation in `crates/awesometv-server/src/api.rs`, focused API tests. **In:** shared preview/create validation, explicit missing IDs, duplicate names/numbers and actionable errors. **Out:** transactional persistence and UI redesign.
- **Prerequisites:** 1. **Checks/evidence:** API tests compare preview/create for valid selection, missing ID and collision; assert error identifies offending input and database remains unchanged. **Effort:** M. **Stop:** preview and create cannot share a stable plan without changing persistence contract; document required split.

### 7. Atomic auto-build
- **Outcome:** One create request either publishes every planned usable channel or leaves none of that request's channels.
- **Own:** auto-build commit path in `api.rs`, transaction-capable helpers in `db.rs` and `programming.rs`, focused integration tests. **In:** transaction or staging plus atomic publish, concurrent number allocation, retry behavior. **Out:** unrelated channel editing.
- **Prerequisites:** 6; coordinate ownership after 4. **Checks/evidence:** inject failure on second channel and timeline rebuild; assert zero shells/items after rollback, successful retry creates exactly the requested usable channels, simultaneous requests do not silently collide. Include response and database assertions. **Effort:** M. **Stop:** external side effects cannot be rolled back; specify resumable idempotent alternative before implementing.

### 8. Establish source support and sync semantics
- **Outcome:** A source matrix distinguishes implemented, verified and unsupported local/Jellyfin/Emby/cloud/Plex behavior.
- **Own:** new `docs/reviews/source-support-matrix.md` only. **In:** inspect `scan.rs`, `jellyfin.rs`, `cloud/`, `api.rs` and README; identify sync scheduling, deletion and auth behavior. **Out:** new integration or inferred Plex support.
- **Prerequisites:** none. **Checks/evidence:** cite routes and source references, list required credentials and safe test strategy, identify data-loss risks; label untested capabilities explicitly. **Effort:** S. **Stop:** a source requires unavailable credentials; record as unverified, not a defect.

### 9. Safe local scan reconciliation
- **Outcome:** A completed local scan reflects additions, changes and removals without treating an interrupted scan as deletion.
- **Own:** `crates/awesometv-server/src/scan.rs`, relevant local scan DB/API functions, focused tests. **In:** progress, bounded ffprobe failures and reconciliation only after a complete scan. **Out:** remote sync and scheduling UI.
- **Prerequisites:** 1, 8. **Checks/evidence:** temp-library test adds, updates and removes clips; inject unreadable/corrupt media and interrupted scan; assert prior records survive incomplete run and completed run reports per-file failures. Capture scan status and resulting records. **Effort:** M. **Stop:** removal semantics would break existing schedules without an agreed policy; propose non-destructive missing marker.

**Wave 6 integration status:** slice 10 has bounded local acceptance. Slice 12 calibrated capture now passes its timing and decoded transition assertions, but private prebuffer configuration remains unresolved; Wave 6 is open pending that fix and fresh timing proof. Slice 13 owns FFmpeg failure/restart and playlist recovery. See `docs/reviews/wave-6-checkpoint.md`.

### 10. Resilient remote refresh
- **Outcome:** A configured supported remote source refreshes on a schedule and recovers from transient/auth failures without losing its last successful library.
- **Own:** `jellyfin.rs`, relevant `cloud/` provider/manager modules, remote sync worker/status and focused mock-server tests; avoid local scanner. **In:** source-specific timeouts, pagination, bounded retry, credential-safe errors, status and incomplete-sync preservation. **Out:** claiming native Plex support or implementing new providers.
- **Prerequisites:** 8. **Checks/evidence:** mock paginated source with timeout, 401, partial page and retry; assert last good records/cursor preserved, success updates status and no credentials in logs. Separately record real-source verification when available. **Effort:** M. **Stop:** source matrix reveals materially different provider semantics; split by provider.

### 11. Guide timeline agreement
- **Outcome:** Public guide and XMLTV show the same persisted items and intervals before and after renewal.
- **Own:** guide-specific `tests/e2e/` or integration specs only; report demonstrated query/serialization defects for a separately owned follow-up. **In:** UTC offsets, boundaries and horizon; **out:** player implementation and external client certification.
- **Prerequisites:** 3–5. **Checks/evidence:** fixed clock fixture asserts channel IDs, titles and exact start/end times against persisted timeline for now, boundary and renewed future; parse XMLTV timestamps and check no overlap/gap. Retain API responses. **Effort:** M. **Stop:** discrepancy originates in scheduling, return a minimal reproduction to slice 4 instead of changing its ownership.

### 12. Timeline-to-stream agreement
- **Outcome:** The actual stream advances through two fixture items at the times and in the order advertised by the guide.
- **Own:** `tests/e2e/playback.spec.ts` and a focused stream integration harness; fix only narrow playback mapping in `engine.rs` if proven. **In:** HLS and TS where supported, decoded media markers and measured timing tolerance. **Out:** failure recovery and third-party clients.
- **Prerequisites:** 1, 4, 5. **Checks/evidence:** capture stream around transition and compare decoded markers and timestamps with guide/timeline; record player, codecs, timing tolerance, duration and logs; include a clock rollover case. **Effort:** M. **Stop:** fixture cannot reliably distinguish actual content or timing; improve harness rather than asserting HTTP 200.

### 13. Playback failure recovery
- **Outcome:** A stream returns to scheduled programming after a missing item or producer restart with an observable fallback.
- **Own:** `engine.rs`, relevant producer lifecycle in `main.rs`, focused playback recovery tests. **In:** bounded restart/backoff, slate behavior and diagnostics; **out:** client-specific work.
- **Prerequisites:** 12. **Checks/evidence:** interrupt producer and remove a fixture item in isolated runs; capture fallback, resumed marker, guide consistency and recovery latency; assert no restart loop and no misleading healthy status. **Effort:** M. **Stop:** recovery requires changing published schedule semantics; obtain decision and hand back reproduction.

### 14. Real client compatibility evidence
- **Outcome:** A versioned compatibility matrix records actual tune, guide and transition results for supported clients.
- **Own:** new `docs/reviews/client-compatibility.md` and test evidence only. **In:** at least one real IPTV client and any claimed HDHomeRun/Plex/Jellyfin/Emby integrations available; M3U, XMLTV and stream URLs; record versions, platform, configuration and failures. **Out:** assuming protocol routes certify clients or modifying external accounts without authorization.
- **Prerequisites:** 11–13. **Checks/evidence:** tune fixture, inspect guide and observe transition/reconnect in each tested client; capture timestamps and results, mark unavailable clients unverified. **Effort:** M. **Stop:** no authorized real client/device is available; report the evidence gap without claiming compatibility.

### 15. Authoritative settings validation
- **Outcome:** Invalid configuration cannot be persisted through the settings API.
- **Own:** settings handlers/types in `api.rs`, `config.rs`, corresponding API tests. **In:** range, type, URL/path and cross-field constraints, safe secret handling and field-specific errors. **Out:** form layout.
- **Prerequisites:** 3. **Checks/evidence:** table-driven tests exercise boundary, malformed and conflicting values; assert rejected writes preserve previous settings, valid values survive reload, secrets do not appear in errors. **Effort:** M. **Stop:** constraints are ambiguous or changing existing valid data would be destructive; document proposed migration and ask for decision.

### 16. Actionable Studio form feedback
- **Outcome:** A Studio user can identify and correct a rejected setting without losing edits.
- **Own:** settings/form sections of `crates/awesometv-server/web/index.html` and focused Studio specs. **In:** associated labels including library search, field errors, status announcement, focus and retained inputs; **out:** general layout redesign.
- **Prerequisites:** 15. **Checks/evidence:** rendered keyboard test submits invalid data, reaches announced error, corrects and saves; inspect accessible names and retained values, including server failure. **Effort:** M. **Stop:** API does not identify fields or a form cannot retain sensitive input safely; report contract gap.

### 17. Accessible responsive Watch
- **Outcome:** Watch navigation, guide and playback controls remain operable at phone and desktop widths with keyboard and assistive technology.
- **Own:** `crates/awesometv-server/web/public.html`, Watch-specific E2E specs. **In:** overflow, focus visibility/order, labels, contrast and reduced motion in empty/loading/error/populated states. **Out:** Studio and stream implementation.
- **Prerequisites:** 3. **Checks/evidence:** rendered 320/375/768/1440px checks, keyboard-only tune and player control, automated accessibility scan plus a named screen reader/browser manual pass; retain screenshots and issues. **Effort:** M. **Stop:** real playback or assistive technology unavailable; document exact unverified flow.

### 18. Accessible responsive Studio
- **Outcome:** Studio's library, channel creation and settings workflows are usable at phone and desktop widths with keyboard and assistive technology.
- **Own:** `crates/awesometv-server/web/index.html`, Studio E2E specs after slice 16. **In:** responsive layouts, focus order, dialogs and status announcements in populated/empty/loading/error states. **Out:** changing business rules.
- **Prerequisites:** 16. **Checks/evidence:** rendered 320/375/768/1440px, keyboard preview/create and settings flows, accessibility scan plus named screen reader/browser manual pass; screenshots and unresolved findings recorded. **Effort:** M. **Stop:** a control's semantics depend on undecided workflow; isolate and report it.

### 19. Persistence and upgrade proof
- **Outcome:** A container retains configuration, media references, channels and schedule across restart and a supported-version upgrade.
- **Own:** `Dockerfile`, `docker-compose.yml`, new isolated deployment test script and deployment documentation only as needed. **In:** disposable volumes, migration from an actual prior supported image/database, health and rollback documentation. **Out:** production deployment or destructive migration.
- **Prerequisites:** 3. **Checks/evidence:** seed, restart, upgrade and compare selected records and guide/playback; preserve previous-version fixture and logs; test failed upgrade on a copy without losing original volume. **Effort:** M. **Stop:** no prior release/image or migration policy is available; mark upgrade unverified and record exact missing artifact.

### 20. Backup and restore proof
- **Outcome:** An operator can restore a consistent backup into a fresh isolated instance and resume the seeded channel.
- **Own:** `crates/awesometv-server/src/backup.rs`, focused backup integration tests and backup/restore documentation. **In:** SQLite consistency, staged restore/restart, media path and secrets prerequisites, failure behavior. **Out:** production restore or external backup service.
- **Prerequisites:** 19. **Checks/evidence:** backup during writes, restore to new data directory, verify database integrity, configuration, channel/guide and actual fixture playback; reject corrupt backup without damaging existing database; document which media and secrets are excluded. **Effort:** M. **Stop:** restore requires destructive action on a non-disposable instance or unspecified secret migration; do not proceed.

## Observable score gates

Scores remain judgments under the scorecard's weighting; a 9 requires all stated evidence on supported configurations and no unresolved high-impact defect, while 10 additionally requires sustained operational evidence and no material known gaps. Record environment, versions, dates, limitations and failures; unavailable evidence caps confidence and must not be treated as a pass.

| Area | Observable 9 gate | Additional evidence toward 10 |
|---|---|---|
| Ingestion/library | 8–10: supported matrix; local add/change/remove and interrupted scan; remote pagination/auth/recovery tests and at least one real supported source | Multiple representative large real libraries, repeated refresh and recovery without data loss |
| Automatic channel generation | 6–7: preview/create validation, atomic failure/retry and usable generated channels demonstrated | Concurrent/repeated operations and representative large collections validated in practice |
| Automatic scheduling | 2, 4–5: controlled expiry, startup/retry, atomic continuity, modes/overrides and visible unhealthy horizon | Sustained unattended multi-week run across restart and time-zone transition, no unexpected gaps |
| Guide | 11: public guide/XMLTV match persisted current and future intervals across renewal and time zones | Multiple real client guide imports and extended horizon agreement |
| Playback/distribution | 12–14: decoded stream agrees with guide across transitions and failure recovery; at least one real IPTV client | Supported protocol/client matrix passes on representative devices over sustained playback |
| Settings/forms | 15–16: authoritative validation, persistence, recoverable field errors and keyboard correction | All supported settings and migration paths exercised, including representative invalid states |
| Design/responsive/accessibility | 17–18: rendered viewport/state coverage, keyboard and screen-reader passes with no serious unresolved findings | Independent assistive-technology review across agreed browser/device matrix |
| Tests/deployment readiness | 1, 3, 19–20: deterministic CI and failure artifacts; restart, upgrade and restore into isolated instances with playback | Repeated successful CI and documented recovery drills on supported deployment targets |

**First implementation batch:** slices 1 and 2. They establish the reproducible media and exact timeline semantics needed to address the P0 without modifying production behavior prematurely. Follow immediately with slices 3 and 4; do not infer a 9 from a passing fixture alone. Product choices still requiring later confirmation are horizon and renewal cadence, deletion/grace and refresh policy, supported source/client versions, accessibility target and upgrade support window; defaults above enable preparation but do not represent authorization to change those policies in production.

**Wave 6 wired playback follow-up:** the later calibrated capture passed its unchanged ceiling, and a wired Engine test decodes audio/video across local → slate → local. Prebuffer configuration and warning-log proof remain open; see `docs/reviews/wave-6-checkpoint.md`.


**Wave 6 local acceptance / Wave 7 handoff (2026-11-18):** Slices 10 and 12 are locally accepted with the bounded prebuffer behavior and fresh calibrated capture documented in `docs/reviews/wave-6-checkpoint.md`. Hosted CI, external clients, live midnight playback, actual remote providers and remote failure/recovery remain Wave 7 or external evidence; nondefault prebuffer real-codec timing is not established. Earlier open-status notes are historical.


**Wave 7 integration gate (2026-11-18): OPEN.** Local recovery and global-language evidence are bounded; encoder stall safety and channel numeric validation were added. Slice 13 still needs partial-generation and live-stall decoded recovery proof; slice 15 still has a persisted cloud credential URL validation gap and an unresolved path contract. Wave 8 prerequisites are therefore not fully accepted. See `docs/reviews/wave-7-checkpoint.md` for exact tests, retention and external limits.

**Wave 7 gate reconciliation (2026-11-18): OPEN.** Slice 15's documented cloud and settings contract has local HTTP/SQLite evidence, with legacy config retained and readable. Slice 13 now has local partial-generation real-stall/decode evidence and focused HTTP retained-segment retrieval after refresh; bounded cancellation-safe input resolution remains the acceptance requirement. Wave 8's overall prerequisite is not yet met. See `docs/reviews/wave-7-checkpoint.md` for scope and external limits.

**Wave 7 final local gate (2026-11-18): CLOSED for slices 13 and 15.** The focused resolver deadline/admission and injected channel slate evidence, prior real-engine recovery and refresh evidence, and bounded settings HTTP/SQLite contract are recorded in `docs/reviews/wave-7-checkpoint.md`. Slice 14 client/device certification is Wave 8 work. Hosted CI, real providers, provisioning and live midnight playback remain unverified; this is not external certification. Earlier open gate entries are historical.


**Wave 8 local review (2026-11-18): BLOCKED overall.** Slice 16 is locally accepted; slice 14 stops for lack of an authorized usable real IPTV client/device. See [the checkpoint](../reviews/wave-8-checkpoint.md) for evidence and the minimum unblock. Under the independent-work replan rule, Wave 9 slices 17 and 19 are eligible on their slice 3 prerequisite; starting them does not accept Wave 8 or certify any client.

**Wave 9 review (2026-11-18): OPEN formal gate; bounded local evidence accepted.** Slice 17 still requires its named screen-reader/browser manual pass and manual review of incomplete contrast/caption checks. Slice 19's recorded baseline-to-review snapshot pair is accepted as a bounded prerequisite, while supported-release provenance, failed-upgrade and upgraded-container playback evidence remain open. See [the checkpoint](../reviews/wave-9-checkpoint.md). Under the independent-work replan rule, Wave 10 slice 18 is eligible on accepted slice 16 and slice 20 on the accepted slice 19 tested pair; neither action closes the formal Wave 9 gate or Wave 8's separate client/hosted-CI gaps.

**Wave 10 review (2026-11-18): OPEN formal gate; bounded local slice 18/20 evidence accepted.** Manual Studio assistive-technology and axe review, version/legacy migration evidence, and prior Wave 8/9 and hosted-CI gaps remain open; the separate channel benchmark/hour soak is ongoing. See [the checkpoint](../reviews/wave-10-checkpoint.md). No overall 10/10 or long-run pass is claimed.
