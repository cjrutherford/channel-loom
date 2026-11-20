# External playback soak: observability rerun

The observer records browser-side evidence from an existing channel using read-only requests. Its events are appended immediately to `events.jsonl`, with a bounded recent-event view, a periodically refreshed `status.json`, and a final `summary.json`. Important player, HLS, network, frame and playlist events also go to the console while the run is active. The event budget is a hard limit: exhaustion marks the run incomplete rather than dropping records. Playlists are parsed in order into per-segment sequence, URI hash, duration, explicit or inferred program date, and discontinuity context. When a comparison detects a change, redacted bounded before/current and following playlist snapshots are retained. URL values and segment URIs are hashed; unknown playlist tags are redacted.

A completed observation means the requested measurement interval elapsed and decoded frame counts progressed. It does not prove uninterrupted playback or content identity. Event counts include startup, so startup waiting must be distinguished from stalls after measurement starts. Frame freezes are detected at the sampling interval and can include short sampling artifacts. Buffer end is a browser-observed buffer edge, not necessarily the publisher's live edge. Content replay for arbitrary MovieNight is unknown without a decoded-content oracle. Browser request failures and TLS peer information do not establish the cause at a proxy or server, and no remote logs were inspected.

The runner uses a new evidence directory outside Playwright's output tree. Never point Playwright's `--output` or HTML report directory at this directory or an ancestor. The previous external run's original logs were lost to cleanup; see [the checkpoint](soak-followup-checkpoint.md), [network investigation](external-soak-network-investigation.md), and [timestamp investigation](external-soak-timestamp-investigation.md). This rerun cannot restore those records.

## Completed run

- Origin: `https://atv.christopherrutherford.net`; existing channel 1.
- Decoded measurement start: 2026-11-19 10:08:23.868 UTC; requested duration 3,600 seconds.
- Evidence: `/home/cjrutherford/.t3/soak-evidence/awesome-tv/20261119T100822Z-5d90cad4-aa62-46c2-9650-eb7177bb5329`.
- Observer SHA-256: `58ce0fbf90e383713b510a013b9aa7c205a7c4d71c8dd7039e591d7a6adf8281`; a copy of the exact observer is in the evidence directory.
- Node v22.22.1; Chromium 159.0.8154.6. DNS resolved to 45.79.204.77; browser TLS and peer details are in response events. No deployed build identifier was found in the fetched public page; build remains unknown.
- Focused Node tests: five passed before launch. A 500 MiB event budget and a 4,500-second outer deadline were configured.


The measurement completed at 11:08:23 UTC after **3,600.085 seconds**, with 3,580 samples and decoded frames increasing from 4 to 103,757. This is an observation-completion result, not uninterrupted playback: 18 waiting/playing episodes occurred after measurement start (plus one at startup). Browser waiting events can include brief transitions and do not necessarily represent perceptible stalls. There were 11 sampled frame-freeze episodes, all followed by sampled frame progression, and no sampled backward-time events. The 12,104,560-byte event file used about 2.3% of the 500 MiB cap. The observer closed its browser, wrote its final status and summary, and exited; no runner process remained.

### Player and error timeline

Times UTC. Waiting durations pair browser `waiting` and `playing`; the observer does not claim precise decoded-content continuity between them.

| Start | Duration seconds | Associated evidence |
| --- | ---: | --- |
| 10:11:51 | 0.425 | Nonfatal buffer stall |
| 10:17:04 | 16.717 | Two playlist timeouts and aborted requests |
| 10:17:59 | 0.029 | Brief waiting transition |
| 10:22:37 | 0.475 | Playlist timeout and buffer stall |
| 10:27:49 | 15.582 | Two playlist timeouts and aborted requests |
| 10:33:21 | 15.694 | Buffer stall, playlist timeout; player time later jumped forward |
| 10:38:15 | 0.068 | Brief waiting transition |
| 10:38:39 | 24.706 | Two playlist timeouts, then a forward player-time jump |
| 10:39:04 | 0.517 | Separate waiting transition immediately after the preceding one |
| 10:44:12 | 12.445 | Buffer stall |
| 10:44:29 | 4.861 | Buffer stall |
| 10:48:37 | 0.004 | Brief waiting transition |
| 10:49:41 | 10.015 | Playlist timeout and forward player-time jump |
| 10:51:27 | 2.441 | Buffer stall |
| 11:00:18 | 15.260 | Two playlist timeouts and aborted requests |
| 11:04:35 | 0.006 | Brief waiting transition |
| 11:05:43 | 19.437 | Two playlist timeouts and aborted requests |
| 11:06:38 | 0.883 | Buffer stall |

All 28 captured HLS errors were nonfatal: 13 `bufferStalledError`, 13 `levelLoadTimeOut`, one `aborted`, and one `fragLoadError`. The last was at 11:06:52, fragment sequence 26953, with `ERR_NETWORK_CHANGED`. Browser request failures totaled 15 `ERR_ABORTED` (one startup; the remainder during the measurement) and one `ERR_NETWORK_CHANGED`. HLS timeouts and browser aborts are correlated events, not independent proof of separate network incidents. The raw event file contains exact timestamps, sampled state, response metadata and HLS details.

### Playlist and provenance findings

The ordered parser recorded 876 successful media playlists and 20,335 segment observations, all with explicit PDT, representing 917 distinct `(sequence, URI hash)` pairs. Comparing repeated pairs across windows found no changes in explicit PDT. This is a check of observed segment metadata, not proof of decoded content or absence of replay. There were two discontinuity-sequence anomalies at 10:30:35 and 10:32:42: when a discontinuity tag left each window, the next response did not contain an `EXT-X-DISCONTINUITY-SEQUENCE` tag, so 23 shared segment identities in each transition were assigned discontinuity sequence 1 then 0 by the parser. Before/current/next ordered, URI-redacted playlist snapshots are preserved in the event file. The snapshots support the observed tag behavior; its effect on the player and any relationship to stalls remain unproven.

High confidence: repeated stalls occurred near the end of the browser buffer, and several coincided with playlist load timeouts. Low confidence: the component responsible for the timeouts or the discontinuity behavior; browser observations do not distinguish origin, proxy, network path, or client causes. No production change was made. The local DNS lookup and browser peer both reported `45.79.204.77`; the browser reported TLS 1.3, subject `christopherrutherford.net`, issuer `YE1`. No exposed deployed build identifier was established. No remote server or proxy logs were examined.

The run used the retained observer copy and hash listed above. After it finished, the page-hook function and request-failure attachment were extracted without changing their behavior so that a focused Chromium test could inject `waiting` and a failed request and assert both were emitted and persisted before browser closure. All six focused tests passed after that refactor. The on-disk runner also now separates measurement-only event counts and an explicit completed-hour flag from its uninterrupted flag, so a startup-only wait does not falsely mark the measurement interrupted. The evidence directory retains the exact observer and original summary used for this run.
