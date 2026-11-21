# Wave 9 slice 17: Watch usability

Local fixture evidence, 2026-11-18. Wave 8 remains blocked on slice 14; this independent work does not change that gate.

Watch now exposes the guide and fullscreen controls on phones, provides keyboard-operable guide tuning and a focusable horizontally scrolling guide, identifies the player and selected channel, preserves rail focus when it is rebuilt, and announces channel/guide loading, empty and failure states with retry controls. Keyboard shortcuts no longer intercept input on controls or the guide. Reduced-motion preference also applies to programmatic scrolling.

`timeout 90s node node_modules/@playwright/test/cli.js test tests/e2e/watch-usability.spec.ts --reporter=line`: **6 passed**. The isolated Playwright route fixture serves the actual Watch HTML and synthetic channel/guide responses on a distinct origin. It checks populated layouts at 320, 375, 768 and 1440 CSS pixels, document overflow, visible focus, keyboard channel and guide activation, player focus and Space input, button names, loading, empty and failure statuses, touch-style retry and tuning, and reduced-motion configuration. Screenshots are in `tests/e2e/watch-evidence/`. The shared T3 preview also opened a temporary local fixture; it was closed and the fixture server stopped.

## Real playback and focused accessibility follow-up

`timeout 240s python3 tests/e2e/watch-real-harness.py`: **3 passed** after `timeout 600s cargo build -p awesometv-server` exited 0 (11 warnings; `/tmp/awesome-wave9-watch-build.log`). The binary SHA-256 was `7a06aabf51aeb28eef3b5b7543086d6ad961b7371975019666d3d66d4e85faef`; current HTML SHA-256 was `3cd343964a2e0065a9dfa9ca3d9ab0de5d0f6d095efa9f70b3408e11c01dc83c`. Each test now asserts that the actual HTTP response from the server exactly equals the current HTML; it no longer substitutes a route fixture for that response. Chrome 156 used a disposable loopback server and database, generated FFmpeg media and two channels; hls.js loads from its configured CDN. The harness cleaned up its server and data. Output: `/tmp/awesome-wave9-watch-real.log`.

At 375px decoded frames advanced 27 → 84 and clock 0.947 → 2.712 seconds; at 1440px frames advanced 74 → 119 and clock 3.399 → 4.894 seconds. The test keyboard-paused and resumed the native player, focused and activated the guide, observed a successful second-channel manifest and subsequent clock advance, and used touch-style tuning at 375px. A separate test injects one 503 for the channel list and verifies Retry loads the real response. Screenshots: `tests/e2e/watch-evidence/real-375.png` and `real-1440.png`. Browser page errors were checked in the two playback tests. This is local fixture evidence, not continuous playback, real-device touch, casting or external-client proof.

Axe-core 4.10.3 focused WCAG 2 A/AA and 2.1 A/AA scan of the populated synthetic fixture: **0 violations, 21 passes**, with `color-contrast` and `video-caption` incomplete and requiring manual assessment. The first scan found insufficient contrast for empty-guide text and error-toast text; those colors were corrected. Existing fixture checks cover names, landmarks/regions, visible focus and status semantics in bounded cases; automated results are not a complete accessibility audit. Reproduce the scan with:

```sh
curl -fsSL https://cdnjs.cloudflare.com/ajax/libs/axe-core/4.10.3/axe.min.js -o /tmp/awesometv-axe-4.10.3.js
WATCH_AXE_PATH=/tmp/awesometv-axe-4.10.3.js E2E_CHANNEL=chrome timeout 45s node node_modules/@playwright/test/cli.js test watch-a11y.spec.ts --reporter=line
```

The named screen reader/browser manual pass was not performed. Orca is installed and loginctl reports an active KDE Wayland seat, but this shell has no DISPLAY or WAYLAND_DISPLAY, no running Orca was found, and no usable interactive screen-reader/browser session was available to perform and assess the manual pass. No graphical environment was provisioned. The plan explicitly lists that pass and says to stop and document unavailable assistive technology; consequently full slice 17 plan acceptance remains open for parent review, despite the bounded real-playback and automated evidence above. Hosted CI remains unverified.

The current contrast changes were inspected: empty-guide `#b4c1df` on panel `#151d36` calculates 9.23:1, and error-toast `#101820` on `#ff6b7a` calculates 6.51:1. These specific static color pairs do not replace manual assessment of the incomplete axe contrast and caption results. No CSS changed during this review; the six fixture checks were not repeated.
