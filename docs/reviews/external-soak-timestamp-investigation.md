# External soak timestamp investigation

## Conclusion and confidence

The retained observer data shows a **5.371-second decrease in the last recorded PDT** between two successful playlist responses. It does not retain enough playlist syntax to prove which segment either PDT applied to or whether a discontinuity was present. A publisher timestamp regression is plausible, but its root cause and even the segment-to-PDT association are unproven. Confidence is high in the recorded values and request order, low in any explanation of the server-side transition. No publisher change is justified on this evidence; the deployed build and configuration are unknown.

## Observations

Source: `test-results/external-soak-net.20261119T064354Z.869250/events.jsonl`, lines below; `summary.json` reports 3,600.945 seconds of observation, without remote server logs or a decoded-content oracle. The directory contains only the event log, rolling events, and summary; no raw playlist bodies are retained. Times below are UTC.

| Event | Request/response evidence | Parsed playlist evidence |
| --- | --- | --- |
| 8370–8372 | load 07:43:19.093, HTTP 200 07:43:19.188, parsed 07:43:19.189 | media sequence 23882, 24 URIs and 24 PDTs; last URI suffix `_23905.ts`, last PDT 07:43:22.305 |
| 8375–8378 | load 07:43:21.094, HTTP 200 07:43:21.187, parsed 07:43:21.188 | media sequence 23906, one URI and one PDT; URI suffix `_23906.ts`, PDT 07:43:16.934 |
| 8385–8388 | load 07:43:25.093, parsed 07:43:25.155 | sequence 23906, two URIs and PDTs, dates 07:43:16.934 and 07:43:20.938 |
| 8399–8402 | load 07:43:31.093, parsed 07:43:31.168 | sequence 23908, one URI and PDT, 07:43:25.561; segment identity changed |

The URI generation identifier changes between 23905 and 23906. A reset or generation transition is a hypothesis, not proof of a restart or its cause. The preceding PDT is about 3.1 seconds ahead of its response; the new PDT is about 4.3 seconds behind its response. PDT describes media time, not necessarily request or playback time. No `hls-error`, `request-failed`, or `player-waiting` event appears between 07:43:14 and 07:43:33; this does not prove uninterrupted or non-repeated content. Neither discontinuity tags nor discontinuity sequence are logged, so their presence and continuity cannot be established.

All 926 parsed playlist events have equal counts of PDTs and URIs; assuming positional pairing, repeated `(sequence, URI)` pairs have consistent PDTs throughout. That is supportive, not a reconstruction of raw tag placement: the parser separately filters the arrays, losing order relative to URI lines, EXTINF durations, and discontinuities. Even equal counts cannot prove one correctly placed tag per segment. No raw playlist from this external run is available to resolve this ambiguity. The other retained local soak playlists are different runs and cannot establish this response's layout.

## HLS association and observer correction

[RFC 8216 §4.3.2.6](https://www.rfc-editor.org/rfc/rfc8216#section-4.3.2.6) associates a PDT tag with the **next** media segment. Tags may be sparse; [§6.3.3](https://www.rfc-editor.org/rfc/rfc8216#section-6.3.3) describes forward and backward extrapolation using durations or media timestamps. Discontinuities require their own tracking; §6.2.1 recommends a PDT on discontinuity segments and prohibits ambiguous mappings.

For the observer lane: parse lines in order and bind pending PDT and EXTINF to the next URI, recording media sequence plus discontinuity sequence/count and explicit versus inferred dates. Preserve a bounded raw playlist or sufficient ordered metadata; do not zip separately filtered arrays. Extrapolate only where durations and continuity support it, keeping unknown mappings unknown, and compare segment identities/sequence and discontinuity context across responses. Add sparse-PDT, discontinuity, and changing-window fixtures. This correction improves future evidence; it cannot recover discarded ordering from this run.

## Local publisher comparison

In `crates/awesometv-server/src/engine/private_publisher.rs`, `Publisher::publish` uses the existing tick cursor for each segment's date and increments it by the segment duration; `write_playlist` emits a PDT for every entry from anchor plus cursor. `Publisher::resume` restores the advertised entries, sequence, discontinuity sequence, anchor and cursor from the existing playlist and rejects a mismatch above 1 ms. Its missing-playlist path initializes a new publisher. Existing test `restart_reconstructs_the_advertised_window` covers restoration of an existing window, not the missing-playlist case or this external transition. The observed `seg_...` names differ from this local publisher's `segment_<uuid>.ts` names; this is further reason not to equate the observed deployment with the current code. It does not identify the deployed implementation.

## Checks and residual evidence

A read-only Python scan of the complete JSONL counted 926 playlist events, found one decrease in consecutive last-PDT values, checked the equal array counts and repeated positional pairs, and extracted the transition above. Source inspection covered the observer parser and local publish, playlist, and resume paths. No test or soak was rerun: there is no demonstrated local defect to reproduce, and a short local test cannot establish deployed behavior. To resolve the remaining question, obtain the actual ordered playlist bodies including `EXTINF`, PDT, discontinuity and discontinuity-sequence tags around this transition, together with deployed version and publisher/server logs. Compare the previous segment's start and duration with the next segment's explicit PDT and continuity context before assigning a production root cause.
