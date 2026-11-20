# Wave 7 integration gate — 2026-11-18

**Decision: closed for bounded local acceptance of slices 13 and 15.** Wave 8 may proceed with the limits below. The evidence below is local; hosted CI, actual provider accounts and failures, system provisioning, live midnight playback, and external clients remain unverified.

## Slice 13: playback recovery

The follow-up in `wave-7-playback-recovery.md` passed the closed-segment failure regression and the real-engine partial-generation stall test: SIGSTOP after first publication, the production 30-second deadline, decoded red/440 Hz → silent slate → blue/880 Hz, 30,239 ms to next publication, continuous sequence and PDT, guide boundary and retained bytes. This is one local Linux sample, not a latency guarantee. These tests were not repeated; the follow-up changes below affect resolver cancellation, not producer stall recovery.

The focused `retained_advertised_segment_survives_refresh_over_http` test passed locally (1/1). It puts a known segment and playlist in the output directory, performs the settings HTTP update that calls refresh, reads the still-advertised segment name, and fetches identical bytes through the actual HLS segment handler. It uses the focused test router, a synthetic playlist and no production authentication; it is complementary to, not a replacement for, the real-engine recovery evidence.

**Resolver review:** The private publisher enforces a two-second asynchronous resolution deadline, prioritizes an expired timer, cancels its ticket without waiting on cache work, discards the late resolution and selects scheduled slate. File probes and token/download/copy blocking closures use four-permit admission retained until each closure exits. Detached cache jobs have separate four-job admission retained until the job exits. Cancellation prevents subsequent publication/eviction admission; an already admitted commit or eviction can finish afterward, still within the cache-job bound. As with Tokio timeouts generally, an operation that blocks an executor thread rather than yielding cannot be preempted by the timer; the reviewed blocking operations use bounded `spawn_blocking`.

`resolution_ticket_publication` passed 1/1 after a regression was demonstrated: cancellation previously waited on the held gate. It now verifies prompt cancellation while an admitted real-file/SQLite commit can complete, and rejects subsequent admission; cancellation before admission preserves the target and database. `resolver_deadline_keeps_blocking_capacity_until_exit` passed 1/1: a stalled admitted blocking closure keeps its permit after timeout, five further attempts enter no closure, stale ticket publication is rejected with unchanged SQLite, and a fresh resolution succeeds after release. Its shortened deadlines exercise admission, not production timing.

`resolver_stall_publishes_slate` passed 1/1 using a test-only injected pending resolver in the actual channel publisher, its production two-second deadline, SQLite schedule and FFmpeg. It asserts a published segment arrives between two and four seconds and decodes its video as dark slate. It does not exercise a provider, audio, or resume; the separate real-engine evidence above covers decoded slate audio and resumed programming.

Refresh preserves the public window and restart reconstructs its state, but retirement timers lost on restart can leave old files on disk. Process-crash publication and real remote failure remain unverified.

## Slice 15: settings validation

The documented language, channel-stream numeric, cloud source credential and root validation contracts have focused HTTP/SQLite evidence in `wave-7-settings-validation.md`, including the completed cloud validation test. New Google source config is restricted to empty/null and credentials are validated before encrypted storage. Legacy nonempty config remains stored and is returned on readback; it is neither migrated nor implicitly deleted and may contain secrets. Operators must inspect legacy data. Validation of HTTPS endpoints is syntactic, not DNS or connectivity proof; focused routers omit production authentication. The previous checkpoint's cloud URL blocker is superseded by that evidence. No claim covers every unrelated configuration endpoint.

## Wave 8 readiness

Slice 16 may use the locally verified settings contract. Slice 14 and external-client/device certification belong to Wave 8, not slice 13 acceptance. Hosted CI, actual provider accounts and failures, provisioning, live midnight playback, and client compatibility remain unverified; they are not local Wave 7 blockers.
