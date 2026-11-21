# External soak: network and playback investigation

## Evidence and limits

The observation ran 3,600.945 seconds from 2026-11-19 06:43:55.990 UTC, with 3,579 samples. Decoded frame count rose from 21 to 107,654, and dropped frames were 3 in the final sample. This is frame progression, not a decoded-content continuity proof. The remote build and configuration are unknown; there are no server logs or host network diagnostics. The initial playlist request aborted while the observer started a second HLS instance; the subsequent measured playback used one blob URL. The raw artifact directory was accidentally removed by Playwright's default output cleanup during the local test in this investigation; the figures here were extracted from the full log before that deletion. Other ignored files in `test-results` were also removed. No copy was found in the workspace or `/tmp`. The separate timestamp investigation retained its own extracted evidence.

## Timeline (UTC)

| Time | Full-log observation |
| --- | --- |
| 06:43:55–56 | Initial aborted playlist request; player started and began frame progression. |
| 06:44:48.602–06:44:56.183 | `waiting` to `playing` took 7.581 seconds; at 06:44:48.656 a nonfatal `bufferStalledError` occurred. Seven adjacent one-second sample intervals had unchanged frame counts; buffer ended at 140.024 while playback stopped at 139.970. Playlist responses at 06:44:47.489 and 06:44:51.495 were HTTP 200, and a segment response at 06:44:51.506 was HTTP 200; a new fragment was requested at 06:44:56.170. These observations do not identify why the player waited. |
| 07:07:56.986 | Nonfatal `levelLoadError`; playlist request failed `net::ERR_NETWORK_CHANGED`. Samples immediately before and after showed frames 43,042 → 43,072 and time 1520.47 → 1521.48. |
| 07:09:40.503 | Nonfatal `fragLoadError`, sequence 23400, with `ERR_NETWORK_CHANGED`; frames 46,151 → 46,181. |
| 07:16:28.409 | Nonfatal `fragLoadError`, sequence 23502, with `ERR_NETWORK_CHANGED`; frames 58,362 → 58,392. |
| 07:37:58.922 | Nonfatal `fragLoadError`, sequence 23823, with `ERR_NETWORK_CHANGED`; frames 97,054 → 97,084. |
| 07:43:55.921 | Final sample: time 3679.720, frames 107,654, readyState 4, unpaused. |

The five HLS errors comprise four network errors and one earlier media stall, all nonfatal. No additional `waiting` or `playing` events were logged around the network errors. They correlate with browser request failures, not a demonstrated server fault: `ERR_NETWORK_CHANGED` is a browser-side failure and does not identify the failing network hop. The earlier stall precedes the first network-change error by over 23 minutes. The observation does not prove zero brief stalls between samples or absence of repeated content.

## Playback and focused experiment

Watch configures hls.js live sync and retry settings. Its fatal network handler immediately calls `startLoad()` and fatal media handler calls `recoverMediaError()`; nonfatal errors are handled by hls.js. The observed four errors were nonfatal, playback kept moving, and the experiment below exercised transient failures without a page-level change. There is insufficient evidence to justify a production retry change or claim fatal/offline recovery is bounded in every case. In particular, the existing immediate fatal restart has no application-level backoff, and recovery after a long outage that exhausts hls.js retries remains unproven.

A local Playwright test uses the current `public.html`, hls.js 1.7.4 supplied as a local bundle, and a synthetic moving HLS window generated from decoded source-second markers. It injects one failed playlist request, one failed fragment request, and a brief browser offline interval. It checks frames advance after reconnect, marker and player time advance, sampled time updates have no 10-second rewind, live sync lag is under 10 seconds, there is one HLS instance, and a subsequent playlist failure does not unpause a paused video. This is a short controlled experiment, not another hour soak, a sustained-outage test, proof of every frame's content, or verification of the unknown deployed build.

## Observer changes and next evidence

The observer now records ordered segment URI hashes, sequence, EXTINF duration, explicit and forward-inferred PDT, and discontinuity sequence. Inference stops at discontinuities or missing timing; no backward inference is asserted. On a same-segment PDT change or a drop in the last segment's PDT, it records the immediately preceding and current redacted playlist bodies and the next body, bounded to 128 KiB each with truncation indicated. Explicit same-segment changes, inference-dependent changes, and window-last-PDT drops are distinguished. URI lines and URI attributes are hashed; other playlist metadata is retained. Existing historical logs cannot be reconstructed from these new fields. See `external-soak-timestamp-investigation.md` for the observed 24-to-1 window transition and its limits.

For a future authorized investigation, retain full bounded browser and playlist telemetry with source identity and an independently verified deployed build; correlate UTC timestamps with server/proxy access and error logs, upstream segment availability and producer timing, and client OS/browser network-change events or a browser netlog. Capture DNS/route/interface transitions and HTTP connection timing without recording credentials. Compare the same segment identity and discontinuity sequence before attributing a PDT rewrite or server failure. Test a longer offline period and fatal retry exhaustion separately before choosing any bounded-backoff production change.
