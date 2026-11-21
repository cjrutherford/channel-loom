# External read-only playback observation

The existing `soak-harness.py` provisions a local server and fixture; it is not a DNS or deployed-server test. This separate observer opens the public player on an explicitly selected existing channel. It creates no account or channel and blocks browser methods other than GET and HEAD. Playback GETs can still cause ordinary server-side streaming activity. No server is started or stopped locally or remotely.

Confirm the HTTPS origin **and existing channel number** before running. Repository status and SWAG configuration mention `atv.christopherrutherford.net`; that is only a historical hint, not confirmation of the requested target. Check that the chosen channel is intended for a sustained public playback request. Do not infer a marker fixture from ordinary media.

```sh
EXTERNAL_BASE_URL='https://CONFIRMED-HOST' EXTERNAL_CHANNEL='CONFIRMED-NUMBER' \
EXTERNAL_OUTPUT='/tmp/atv-external-soak-unique' \
node tests/e2e/channel-performance/external-soak.cjs
```

Default observation is 3600 seconds measured using a monotonic clock **after** navigation and selection; setup and cleanup are additional. `EXTERNAL_SECONDS` permits 5–3600 for short checks; only 3600 is an hour. The output directory must not exist. `EXTERNAL_LOG_BYTES` defaults to 104857600 bytes for the full append-only `events.jsonl`. Exceeding the budget or a write error stops the run and marks it incomplete; the separate 300-event rolling snapshot and summary add bounded overhead. The browser is closed on normal completion, errors, SIGINT and SIGTERM. SIGKILL or host failure cannot guarantee cleanup or a final summary. Keep sufficient disk space and protect artifacts: URLs, console messages, playlist paths and headers may contain sensitive information.

`events.jsonl` timestamps browser samples, player and HLS events, response status/selected proxy headers, peer address and browser-reported TLS details, request failures, local DNS answers, and playlist media sequence, PDT and segment references. `summary.json` records duration, decoded frame progression and limitations. A frame-progressing observation is not a quality pass: inspect stalls, errors, reconnects and timeline signals. Browser DNS may differ from the local resolver and proxies can hide hops; remote server logs, server build identity and proxy configuration are not observable here. HLS hooks depend on the deployed page's compatible hls.js construction. No HLS events can mean missing instrumentation, not no errors.

No source-content oracle is available for arbitrary deployed media. Player time, decoded frame counts, PDT and media sequence alone cannot prove or exclude a ten-second content replay. The isolated marker checker requires confirmed marker content **and** a trustworthy persisted schedule identity/mapping; do not run it against arbitrary media. This observer does not implement that separate oracle path.

Focused offline check: `node --test tests/e2e/channel-performance/external-soak.test.cjs`. It validates configuration and playlist parsing, not live playback or DNS/TLS.
