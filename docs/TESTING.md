# End-to-end testing

The browser suite uses Playwright and lives in `tests/e2e/`.

## Requirements

- A populated ChannelLoom instance (channels, media, guide).
- Chromium or Google Chrome.
- Studio credentials for authenticated tests.

## Run

```bash
corepack pnpm install
E2E_BASE_URL=https://atv.christopherrutherford.net \
E2E_CHANNEL=chrome \
E2E_USER=admin \
E2E_PASSWORD='...' \
corepack pnpm e2e
```

Use `corepack pnpm e2e:headed` to watch the browser. Public runs can set
`E2E_RETRIES=1` to retry a transient network failure once.

Without `E2E_PASSWORD`, the public Watch tests still run and the Studio tests
are skipped.

## Coverage

- Watch loads channels, guide, player, and channel switching.
- The buffer meter appears on `waiting` and disappears on `playing`.
- Public APIs: health, channels, guide, config, HDHomeRun, M3U, XMLTV, PWA
  manifest, service worker, icons, and Cast receiver.
- HLS playlist for channel 1.
- Studio login and every Studio view.
- Studio buffer meter.
- Auto-channel preview returns plans without creating channels.
- Authenticated APIs, role, audit, token, provider, and metrics endpoints.
- Anonymous access to protected APIs returns `401`.

## Long playback check

`tests/e2e/playback.spec.ts` samples the player clock for a longer window and
fails if it ever moves backwards. It is opt-in because it runs for minutes:

```bash
E2E_BASE_URL=https://atv.christopherrutherford.net \
E2E_CHANNEL=chrome \
E2E_PLAYBACK_SECONDS=180 \
corepack pnpm e2e:playback
```

Run it after changing HLS output, the producer supervisor, or the precache
policy. To provoke restarts during the run, kill the channel producer from
another shell:

```bash
pkill -f '^ffmpeg .*data/hls/1/'
```

## Manual playback checks

The automated suite does not assert live playback continuity because it depends
on producer timing. When changing HLS output or the producer supervisor, run a
browser session for several minutes while restarting a producer, then confirm
`player.currentTime` never decreases:

```bash
# in one shell, watch the player
node /tmp/playback-probe.mjs
# in another, force a producer restart
pkill -f '^ffmpeg .*data/hls/1/'
curl -s http://127.0.0.1:8409/channel/1.m3u8 | grep -E 'MEDIA-SEQUENCE|^1/'
```

`MEDIA-SEQUENCE` must increase or stay equal, never reset, and the first segment
of the new playlist must have a later MPEG-TS `start_time` than the last segment
of the old playlist.

## Notes

- Tests run serially because they share the live database and producers.
- Channel switching and HLS startup may take several seconds on a slow source
  mount.
- The suite does not create or delete channels; auto-build is preview-only.
- CI integration is not enabled yet because it needs a seeded server fixture and
  a browser install step.
