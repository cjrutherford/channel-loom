# Wave 4 integration checkpoint

2026-11-18, `review/application-feature-scorecard-2026-11-18`, shared working tree. Scope: slice 7 atomic auto-build and slice 8 source support review. Other existing edits are preserved; no commit or push.

## Verdict

Slices 7 and 8 are accepted for their stated outcomes, with the limits below. Slice 7's transaction publishes channel shells, classic definitions, timeline items and generated continuation together. The SQLite write lock precedes revision and allocation rechecks. Failure before or during the second channel rolls back the whole request. The refresh follows commit, is best effort and does not return an error to the handler. A lost response or interruption after commit is ambiguous: there is no idempotency key, and replay is not assured to return the original success. Inspect persisted channels before retrying. See [atomic review](wave-4-atomic-autobuild.md).

Slice 8 distinguishes implementation from verification and documents credentials, safe test strategy and source-derived deletion/partial-refresh risks in the [matrix](source-support-matrix.md). In particular, the cloud limit boundary can incorrectly permit stale deletion after an incomplete listing, and updates/deletes need not be atomic. These are risks for slice 10, not a claim of verified remote operation; no real credentials were used.

## Evidence and remaining limits

- Prior slice 7 evidence: `timeout 180s cargo test -p awesometv-server auto_build_ -- --nocapture`, 2 passed, 0 failed, using a real Axum handler and disposable SQLite without production auth middleware. Triggers abort the second channel insert and its timeline insert; snapshots include definitions, items, generated state and revisions. A known rollback retry and concurrent explicit-number conflict were exercised. This existing test was not repeated because this review changed no implementation. See the atomic review for details.
- Source review followed staging through the transaction, revision triggers, classic continuation consumption and renewal ownership checks. It does not prove a live auto-build-to-worker cycle, process death at commit, response loss, or engine cleanup success. No new material defect was identified in the owned auto-build path, so no regression test was added.
- Source matrix findings are source review, not live provider tests. The cloud pagination boundary and partial-deletion behavior need focused regression coverage and repair in their owned follow-up; local/Jellyfin deletion and interruption behavior remains unverified.
- Wave 5 slice 9 is eligible for independent replanning, but the Wave 5 guide lane (slice 11) remains blocked by slice 3. Slice 10 is scheduled in Wave 6; the cloud findings should inform it. This is not a release-readiness claim. Wave 2 slice 3's exact browser-smoke CI provisioning/install/build/run proof remains open and is not waived.
