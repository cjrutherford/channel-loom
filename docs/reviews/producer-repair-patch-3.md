# Producer repair patch 3: integration not completed

The public lifecycle has not been switched to the private publisher. Wave 6 is not accepted. No continuity, latency, resource-bound, or route-safety claim follows from the checks below.

The existing `ensure_inner` still selects by wall time and stops its child when the item changes. The private spool currently limits each invocation to ten seconds; production integration therefore needs a schedule-aware chunk controller, including source offsets and timestamp normalization, before it can handle longer items. The private publisher is not called by the public routes. Refresh and lag resync can clear the public directory, so retention and reads must also be coordinated when the lifecycle is replaced.

On 2026-11-18, `timeout 180s cargo test --offline -p awesometv-server private_publisher::tests::incremental_paced_segments_and_cancellation -- --nocapture` passed one test. It uses real FFmpeg but tests a single video-only private source and cancellation, not production audio/video codecs, public routes, or three transitions. `timeout 180s cargo build --offline -p awesometv-server` passed with eight warnings; this builds the existing lifecycle, not the requested integration. Logs: `/tmp/awesome-producer-patch3-test.log` and `/tmp/awesome-producer-patch3-build.log`.

After implementation and a fresh build, the existing calibrated external capture can be run with:

```sh
STREAM_EVIDENCE_DIR=/tmp/awesome-producer-patch3-capture timeout 180s python3 -u tests/e2e/stream_agreement.py > /tmp/awesome-producer-patch3-capture.log 2>&1
```

That capture has not been run for this review and cannot establish acceptance of an unimplemented integration. The required production-codec multi-transition tests, bounded backlog, shutdown, late-boundary behavior, and safe segment cleanup remain outstanding.
