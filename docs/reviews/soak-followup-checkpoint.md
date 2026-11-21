# Soak follow-up checkpoint

## Artifact loss and available evidence

Playwright's default output cleanup removed the ignored `test-results` contents during the earlier local test, including the external soak's original `events.jsonl`, rolling events and `summary.json`, and other ignored artifacts. Their complete inventory is unknown. Earlier searches found no workspace or `/tmp` copy; this follow-up found no soak-named file in the standard user trash locations checked. No artifact was recovered. The original raw logs are unavailable from these checks, not proven impossible to recover from every backup. Raw playlist bodies were not recorded by that run in the first place.

The numerical observations and event excerpts extracted before deletion survive in [the network investigation](external-soak-network-investigation.md) and [timestamp investigation](external-soak-timestamp-investigation.md). The latter's source path describes the now-deleted original, not a currently available file. The 3,600.945-second observation, frame progression, 5.371-second last-PDT decrease, and the cited event sequence are extracted evidence, not a replacement for the original log or decoded-content proof.

The remote build and configuration remain unknown. The 7.581-second wait has no established cause; the first observed network-change error was over 23 minutes later. The short controlled network recovery experiment previously passed; it does not establish behavior of the deployed version or sustained outages. No local production fix or deployment is claimed, and neither engine nor public HTML was changed in this follow-up.

## Observer review

The ordered parser associates PDT and EXTINF with the next URI, tracks discontinuity sequence, and stops forward inference across discontinuities or missing timing. Its four existing offline tests cover configuration, sparse PDT/discontinuity, classification, and playlist-body redaction/size limits; these tests and the prior short recovery proof were not rerun because the observer and playback code were unchanged. Snapshot capture retains before/current and next response after an anomaly, but the tests do not exercise that asynchronous capture path. Redaction hashes URI lines, URI attributes, and HTTP(S) URLs in playlist bodies; other metadata is retained and snapshot/event URL fields are not generally redacted. Treat observer output as potentially sensitive. Historical tag placement and discontinuities cannot be reconstructed from the deleted run.

## Prevention and boundaries

The shared Playwright config assigns results and HTML report to sibling directories under a fresh `/tmp/awesome-tv-playwright-<UUID>` per config load. The two harnesses with explicit `--output` already point into their own temporary directories; other harnesses now inherit the default. A Node config test loads the actual config concurrently, checks unique scratch paths and a manual `test-results` sentinel without running Playwright or testing deletion. Scratch output is not automatically removed by this guard and may need deliberate cleanup after review.

Playwright CLI `--output` overrides `outputDir` and can delete the chosen directory at startup. Never pass the project root, `test-results`, an ancestor of manual evidence, or any existing artifact directory; use a fresh test-owned directory. Reporter overrides and `PLAYWRIGHT_HTML_OUTPUT_DIR` can similarly select a shared HTML output location; avoid directing them at evidence. These overrides bypass the default safeguard. Tests or scripts that explicitly write files into `test-results` still do so and may overwrite their own named files; the guard only isolates runner-managed output.
