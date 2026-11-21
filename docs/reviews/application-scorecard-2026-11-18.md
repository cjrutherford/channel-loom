# ChannelLoom application feature scorecard — 2026-11-18

## Scope and method

Assessment of the checked-out `review/application-feature-scorecard-2026-11-18` branch for a personal-media live-TV aggregator: ingestion, automated channel creation and programming, guide, playback, settings, UX, and operational readiness. Corpus: Rust server, scheduler, Watch and Studio HTML/JS, Playwright specs, Docker and project documentation. Source locations below are relative to the repository root. This is an evidence-based review, not a compatibility certification or ErsatzTV parity assessment.

Scores use 0–10: 0 absent, 3 basic/fragile, 5 usable but important gaps, 7 broad capability with material unverified risks, 9 reliable and validated end-to-end. Each score weighs user impact/correctness 40%, workflow completeness 30%, usability 15%, verification/operability 15%. Scores are judgment estimates, not measured percentages. Confidence means confidence in the assessment, not reliability of the product. Overall is the unweighted mean of the eight categories, 5.5/10; it must not hide the P0 continuity risk.

| Area | Score | Confidence | Basis |
|---|---:|---|---|
| Ingestion and library | 6 | Medium | Local scan and ffprobe, Jellyfin/Emby sync, cloud support; real-source behavior untested. |
| Automatic channel generation | 6 | Medium | Collection-based dry-run and creation exist; creation is multi-step and not visibly atomic. |
| Automatic scheduling | 4 | High for source finding; low for runtime impact | Classic/block generation and overrides exist, but finite timelines lack an identified automatic renewal call. |
| Guide | 6 | Medium | Public guide and XMLTV use persisted timeline; future horizon/renewal and client compatibility unverified. |
| Playback and distribution | 6 | Medium | HLS, TS, HDHomeRun, IPTV and producer recovery are implemented; no actual playback tested here. |
| Settings and forms | 6 | Medium | Broad configuration and labeled controls, with fragmented workflows and validation needing live review. |
| Design, responsive, accessibility | 5 | Low | Purposeful layouts, breakpoints, reduced motion and some accessible labels; no rendered or assistive-technology inspection. |
| Tests and deployment readiness | 5 | Medium | Scheduler unit checks pass; existing E2E requires seeded external instance and is not executed here. |

## Findings and evidence

### P0 — Schedule expiry threatens unattended TV

`programming.rs:44` rebuilds a timeline from the current UTC date; classic programming explicitly stops at seven days (`programming.rs:221`, `programming.rs:233`), and block programming uses `build_week` (`programming.rs:209`). Searches for `rebuild_channel(` found only request/script application call sites, not a periodic replenishment worker; startup tasks in `main.rs:105` prewarm producers and guard lag. `db.rs:1120` selects only items whose actual time interval contains now, and the guide queries actual intervals (`db.rs:1159`); `engine.rs:571` falls back to slate when no current item exists. Thus, without another rebuild, generated programming can expire, leaving slate and an empty future guide. This is a strong code inference, not a seven-day runtime reproduction. **Fix:** implement durable rolling-horizon replenishment for all generated modes, including startup recovery, failure visibility, atomic replacement, and stable continuity; test with a controlled clock across expiry/restart and verify guide and playback together.

### P1 — Auto-build is useful but creation can partially succeed

`api.rs:1878` chooses up to 20 collections, avoids existing names by default, assigns numbers, and supports dry-run; `api.rs:1951` creates shells, sets classic items, rebuilds, and refreshes each in sequence. No encompassing transaction or rollback is visible in this handler. A failure partway can leave earlier channels and a partially configured current channel; retrying requires inspection. Explicit selected IDs are silently filtered if missing (`api.rs:1900`). Studio offers preview and creation (`web/index.html:500`, `web/index.html:1058`), and the existing E2E covers preview only (`tests/e2e/studio.spec.ts:43`). **Fix:** validate the complete plan, make creation transactional or resumable with explicit per-channel outcomes, and test failures/retries as well as successful creation. Source-based finding, medium confidence.

### P1 — Library connectivity and unattended refresh need proof

Local scanning uses ffprobe (`scan.rs:17`, `scan.rs:52`), and Jellyfin/Emby sync and local scanning have API handlers (`api.rs:306`, `api.rs:307`, `api.rs:1420`). Cloud source handling also exists. README names Plex as a source direction (`README.md:9`); a native Plex library sync route was not found in the source routes reviewed, so do not assume parity. Scan/sync, metadata quality, changed/deleted files, credentials, large libraries, errors and periodic refresh were not exercised. **Fix:** establish supported source matrix, implement/verify automatic refresh with progress and errors, and test actual local and remote libraries including removals and auth failures.

### P1 — Guide and playback need real integration proof

The server exposes public guide, M3U, XMLTV, HDHomeRun and HLS/TS routes (`api.rs:428`, `api.rs:440`–`api.rs:449`); guide queries derive from stored timeline (`db.rs:1159`). Producer code has slate fallback and lag recovery (`engine.rs:571`, `main.rs:110`). These routes and recovery paths do not by themselves establish synchronized, continuous playback or compatibility with Plex, Jellyfin, Emby and IPTV clients. **Fix:** run seeded end-to-end playback over item transitions, producer restart, missing media, and week rollover; compare guide timestamps/content against actual playout and validate at least one real IPTV client. Record duration, browser/client, source and failures.

### P2 — Forms, layout and accessibility

Studio includes many labeled configuration fields and an auto-build preview (`web/index.html:500`, `web/index.html:575`); Watch uses native video controls and labeled icon buttons (`web/public.html:302`, `web/public.html:327`). CSS contains responsive breakpoints (`web/index.html:269`, `web/public.html:266`) and reduced-motion handling (`web/public.html:295`). However the library search input at `web/index.html:524` has a placeholder but no associated label in its markup. Source inspection cannot establish screen-reader announcements, keyboard focus, error recovery, contrast, visual hierarchy, or actual small-screen overflow. **Fix:** label search, audit all controls and error/status messaging, then exercise keyboard and screen-reader flows at desktop and phone widths with populated, empty, loading and failure states. Confidence low until rendered testing.

### P2 — Verification and operations

`tests/e2e/public.spec.ts:4` and `tests/e2e/studio.spec.ts:43` cover Watch and Studio flows, but documentation requires a populated server and credentials and says CI integration is not enabled (`docs/TESTING.md:7`, `docs/TESTING.md:25`, `docs/TESTING.md:91`). Long playback test is opt-in (`tests/e2e/playback.spec.ts:13`); the normal suite does not establish continuity. Docker packaging exists, but image build, persistence, backup/restore, upgrade, resource usage, hardware acceleration and deployment were not tested. **Fix:** create isolated deterministic fixtures and CI integration, add source-to-channel-to-playback/guide and renewal tests, then verify container startup, persistent restart and backup/restore.

## Verification and limits

- `timeout 60s cargo test -p awesometv-scheduler --offline`: exit 0; 8 passed, 0 failed, 0 ignored; binary and doc tests each ran 0. Log: `/tmp/awesome-tv-audit-test.log`. This proves scheduler unit tests only, not server integration.
- Environment: Node v22.22.1, Cargo 1.98.1, existing `node_modules`; no dependency installation performed. Git branch checked; worktree initially clean.
- T3 `preview_status` returned `available: false`; no preview tab could be opened. No screenshots, responsive interaction, browser accessibility audit, real media ingestion, streams, deployed client, Docker build, or authenticated E2E were verified. The documented E2E needs a seeded instance and Studio credentials; no existing instance was assumed safe to mutate. These limitations materially lower confidence in visual and functional scores.
- No application source was changed. Findings based on source are hypotheses about runtime behavior where explicitly stated; investigate against the running deployment before claiming an observed outage.
