# Wave 7 slice 13: playback recovery evidence

Local evidence on 2026-11-18, review branch, FFmpeg 8.0.1. The tests use isolated SQLite databases and local FFmpeg processes; the remote failure uses a loopback HTTP server, not a cloud provider.

The private spool replaces a failed source encoder's unpublished scheduled remainder with slate, then continues the existing queue. A failed slate encoder terminates the runtime instead of spinning. A terminated runtime is reported as failed, and request-driven restart has a five-second backoff. Restart reads the advertised playlist, verifies its segment files and reconstructs its sequence, discontinuity sequence and program dates before appending. Invalid or missing advertised state fails closed. Readiness after restart requires a newly published segment as well as the configured prebuffer threshold.

`timeout 100s cargo test --offline -p awesometv-server engine::tests::channel_runtime_recovers_ -- --nocapture --test-threads=1` passed three cases. They decoded red video and audio, dark silent slate, then blue video and audio, and checked the guide projection's unchanged item IDs and scheduled starts against playlist program dates within one second. Measured time from injection to the first observed resumed publication was 10,000 ms after file removal, 11,000 ms after the loopback HTTP 404, and 4,000 ms after SIGKILL of the FFmpeg process blocked on a loopback URL. The first resumed publication lagged its scheduled boundary by 2,000, 3,000, and 3,000 ms respectively. These are individual local samples, not latency guarantees; the test clock has one-second resolution. The URL case also aborted and restarted the runtime, checked failed status and restart backoff, and asserted that the existing advertised segment window remained a prefix after restart. `/tmp/awesome-wave7-measured.log` records the run.

`cargo test --offline -p awesometv-server engine::private_publisher::tests::restart_reconstructs_the_advertised_window -- --nocapture` passed the sequence, discontinuity and scheduled-cursor reconstruction test. The focused real-codec `production_command_produces_ahead_of_three_publication_deadlines` test passed with publication lags of 998, 996 and 994 ms; `channel_runtime_publishes_local_slate_local_in_order` also passed its decoded boundary checks. Logs: `/tmp/awesome-wave7-resume.log`, `/tmp/awesome-wave7-pacing.log`, and `/tmp/awesome-wave7-baseline.log`.

Limits: guide agreement here exercises the guide projection against the persisted schedule, not the HTTP guide route or an external client. Runtime restart is demand-driven. The tests do not establish a real cloud-provider outage, process/machine loss during an atomic publication, or an unbounded outage. A fresh calibrated capture passed with HLS transition error 42 ms, TS sampled age 2,749 ms, tolerance 4,735 ms below the 8,000 ms ceiling, and delayed negative control 17,752 ms; artifacts are in `/tmp/awesome-wave7-calibrated/` and `/tmp/awesome-wave7-calibrated.log`. Its existing assumptions remain in the Wave 6 checkpoint. Hosted CI and external clients remain unverified.

## Integration update (2026-11-18)

A 30-second no-closed-segment encoder timeout and a fallback failure timestamp for the five-second request backoff were added. The focused sleeping-child and backoff tests pass; the subsequent partial-generation stall proof is recorded below. Cancellation/restart retention, refresh limitations and the open slice 13 acceptance checks are recorded in `wave-7-checkpoint.md`.

## Refresh retention follow-up

Explicit refresh no longer clears the public HLS directory. A private runtime restart reconstructs its advertised window and continues sequence/PDT using the existing resume logic. The local real-FFmpeg runtime test now saves advertised segment bytes, refreshes, restarts, and checks that those bytes remain readable and sequence does not regress. This is a filesystem-level check, not an HTTP client test. Retired files whose timers are lost on refresh can remain on disk. Legacy producers still clear their own output when started fresh, so their refresh retention is not established.

The input-resolution deadline remains open: `resolve_media_input` includes synchronous filesystem checks and can spawn cache work with side effects. Wrapping the future in a timeout would not preempt a blocking poll or cancel spawned work; a bounded, cancellation-safe resolver contract is needed. The existing 30-second encoder bound and five-second request backoff are unchanged.


## Partial generation and real stall follow-up (2026-11-18)

A failed encoder previously returned failure before consuming already-closed segments in its private playlist. Those segments were discarded and slate began at an earlier public offset. The spool now transfers each closed segment first and reports failure only after the available tail is consumed; an absent playlist from an unsuccessful encoder still reports failure. `failed_encoder_retains_closed_segments_before_replacing_remainder` passed, including an unsuccessful child with two closed segments and the subsequent failure result.

`timeout 115s cargo test --offline -p awesometv-server channel_runtime_recovers_partial_generation_after_real_stall -- --nocapture` passed in 33.98 s. A test-only real-time input keeps the actual FFmpeg encoder alive; after its first segment is published, the harness sends SIGSTOP to that encoder and waits for the unchanged production 30-second stall deadline. The first observed next-source publication followed injection by 30,239 ms. Decoded frames and audio verified red/440 Hz, dark silent slate, and blue/880 Hz. The test checks retained first-segment bytes, sequence zero, continuous per-segment PDT, next-source PDT against the persisted guide boundary, ready/failure state, and the same live runtime task throughout recovery. This is one local Linux sample, not a latency guarantee; the real-FFmpeg scenario exercises a stall rather than SIGKILL, while the closed-tail failure regression uses a controlled child and playlist. HTTP segment retrieval was not exercised.

The resolver remains a separate seam: `resolve_media_input` performs synchronous filesystem checks and may spawn side-effecting cache work, so timing out its future alone cannot guarantee preemption or cancellation. A bounded cancellation-safe resolver contract and a real blocking-input proof remain for the next assignment. The integration checkpoint predates this follow-up and requires the parent integration gate to reconcile it.

## HTTP retention check (2026-11-18)

`cargo test --offline -p awesometv-server retained_advertised_segment_survives_refresh_over_http -- --nocapture` passed 1/1. A focused router serves the real segment handler after a settings update invokes refresh; the test retrieves the advertised synthetic segment and compares binary bytes. It does not exercise production authentication or itself generate/recover a real stream. The input-resolution cancellation and admission seam remains open as described in the integration checkpoint.


## Resolver admission follow-up (2026-11-18)

The earlier resolver-open statements above are superseded by the focused resolver review in `wave-7-checkpoint.md`. `resolver_deadline_keeps_blocking_capacity_until_exit` exercises the private publisher's actual deadline wrapper using an injected latch-controlled blocking operation and ticketed detached cache publication; `resolution_ticket_publication` preserves the real-file SQLite publication linearization proof. These are local tests, not a real cloud-provider or end-to-end slate playback test. An admitted commit may finish before cancellation returns; the deadline cannot preempt it.
