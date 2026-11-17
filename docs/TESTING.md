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

Use `corepack pnpm e2e:headed` to watch the browser.

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

## Notes

- Tests run serially because they share the live database and producers.
- Channel switching and HLS startup may take several seconds on a slow source
  mount.
- The suite does not create or delete channels; auto-build is preview-only.
- CI integration is not enabled yet because it needs a seeded server fixture and
  a browser install step.
