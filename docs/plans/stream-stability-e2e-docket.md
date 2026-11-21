# Stream stability E2E docket

Status: **open: full-hour run not executed**. The isolated encoded-marker harness and browser sampler are authored. A short run is a pipeline check, not hour-soak evidence. The binary hash identifies exactly what ran, but does not prove which source revision produced that binary; build provenance must be supplied independently. The production engine and Studio tests are outside this work.

## Case 1: channel startup

Run `E2E_CHANNEL_PERFORMANCE=1 E2E_BASE_URL=http://127.0.0.1:8409 E2E_BUILD_ID=<exact-build-id> ./node_modules/.bin/playwright test tests/e2e/channel-performance/channel-load.spec.ts --workers=1`. Supply a running, isolated representative server with at least two playable channels; its operator owns startup and cleanup. The test selects channels sequentially twice: first visit in the run and repeat visit. These labels do not guarantee an empty server or OS cache; record cache state and explicitly clear/restart only in a controlled fixture before the first pass. It records click-to-request, click-to-playlist response, click-to-first newly decoded frame, and click-to-usable playback (>1 second advance), HTTP status, per-channel errors, p50/p95/max. Calculate request-to-playlist and playlist-to-frame by subtraction. Optional `E2E_LOAD_THRESHOLD_MS` sets a usable-playback threshold; absent it, measurements establish a baseline, not an invented SLA. Timeouts are harness bounds, not performance targets. Preserve `test-results` JSON and report failures separately; compare distributions and each channel against an agreed baseline. This test does not provision fixtures or certify truly cold caches.

## Case 2: isolated decoded playback soak

`tests/e2e/channel-performance/soak-harness.py` generates a 4,200-second H.264 fixture with a 13-bit source-second counter in complementary white/black cells. The browser reads the decoded video via canvas every second and rejects ambiguous pairs. It provisions a fresh server and database, anchors one persisted 4,200-second broadcast near its beginning, checks the public guide against the persisted row, and requires that row to cover the entire run. Each marker must fit the row's source in/out and be within 60 seconds behind (or two seconds ahead of) its scheduled position. There is no permitted schedule transition or intentional seek in this fixture. Within the same row, a backwards decoded jump over one second fails, regardless of player clock, HLS sequence, or timestamp resets. Consecutive marker sampling gaps above four seconds and five seconds without marker progression fail; total source progression must be within five seconds of wall time.

The opt-in browser run starts its monotonic interval after the first decoded frame. `SOAK_SECONDS=3600` is required for the full run; shorter values are explicitly labeled short checks. The test timeout is duration plus 240 seconds, its subprocess deadline duration plus 300 seconds, and the outer deadline below covers fixture generation and cleanup. The harness confirms listener ownership and terminates its process groups on exit or signals. A heartbeat prints at least once a minute. The fixture and binary SHA-256, Git HEAD and dirty status, versions, settings, guide rows and fixture hash before/after are retained. Binary/source correspondence and OS cache state are not established by the harness.

Telemetry includes a 600-entry rolling window of decoded marker, player time, video frame counters, player events, source-buffer append/remove operations, playlist sequence/discontinuity/PDT/segment lines, and request failures. It captures a screenshot and last playlist on failure, a 1 MB rolling server log tail, and reports truncation. A freeze is counted after two seconds without a new decoded frame; waiting-to-playing intervals are separately accumulated as rebuffer time. `SOAK_MAX_STALL_SECONDS` is the predeclared maximum for each total; choose it before running, not after seeing results. Aborted requests are recorded but excluded from the error count because player retuning can cancel requests. Source-buffer event overflow or missing marker/playlist evidence fails. The browser records network failures, but does not instrument hls.js internal reconnect events; the summary explicitly reports that gap. The capture is a bounded pre-failure window, not a post-failure recording. Retain the artifact directory according to the operator's CI policy.

Build the intended server first and independently record its build provenance. With a dedicated worker and dependencies already installed, the exact full-hour command is:

```sh
timeout --signal=TERM --kill-after=30s 4800s env SOAK_SECONDS=3600 SOAK_MAX_STALL_SECONDS=10 SOAK_ARTIFACT_DIR="$PWD/test-results/channel-soak-hour" E2E_SERVER_BINARY="$PWD/target/debug/awesometv-server" python3 tests/e2e/channel-performance/soak-harness.py
```

The artifact directory must be empty. The 10-second stall budget is a proposed run setting, not an established SLA. A successful command is playback evidence for this fixture only; inspect the reported instrumentation gaps and independent build provenance before treating it as release acceptance. An interrupted, timed out, failed, or shorter run is not an hour pass. This command has not been run for an hour.

## Short checks

```sh
node --test tests/e2e/channel-performance/continuity.test.cjs
SOAK_SECONDS=8 SOAK_MAX_STALL_SECONDS=3 SOAK_ARTIFACT_DIR=/tmp/channel-soak-short python3 tests/e2e/channel-performance/soak-harness.py
```

The first injects a decoded replay into a sequence whose player clock continues forward. The second exercises actual encoded media, isolated server, guide, browser playback, and pixel decoding, but cannot establish long-run stability. Both require their output directory to be empty; the browser check also requires ffmpeg, a built server, Playwright Chromium and access to the public player's hls.js dependency. No full-hour run or production fix is claimed here.
