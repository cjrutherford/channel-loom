# Wave 8 checkpoint — 2026-11-18

**Gate: BLOCKED.** Slice 16 is locally accepted for the scoped Studio forms; slice 14 is blocked at its client-availability stop. The complete Wave 8 checkpoint is not accepted. The Wave 7 prerequisite was locally closed in [its checkpoint](wave-7-checkpoint.md).

## Slice 16: accepted locally

Reviewed [the form evidence](wave-8-studio-forms.md), the actual [Chromium assertions](../../tests/e2e/studio-forms.spec.ts), and the server validation mappings. The test checks keyboard submission, focused status, field association and retention, correction and successful save, credential retention and clearing, a delayed simulated 503 with duplicate-save guard and no secret in status, folder validation and the library search name. Server response text is not rendered. Channel language errors do not identify the field: the form provides fixed guidance to check both codes instead of assigning blame. This is a bounded local acceptance, not a screen-reader or real-account certification.

After the review fix, `timeout 180s cargo build --offline -p awesometv-server` passed with existing warnings. A temporary copy of `scripts/run-isolated-e2e.py` selected the Studio spec and passed its generated password via `E2E_PASSWORD`; the bounded run passed 1 Chromium test, with disposable server and data cleaned up. See `/tmp/wave8-review-build.log` and `/tmp/wave8-review-forms.log`; screenshot uses fixture data.

## Slice 14: blocked

[Client evidence](wave-8-client-compatibility.md) and the [matrix](client-compatibility.md) record no client acceptance. The existing smoke ran a loopback disposable fixture, not a client. Review of the retained final snapshots confirmed the M3U XMLTV and stream links use the fixture's loopback address and port and `tvg-id=awesome.1` matches XMLTV `awesome.1`. The fixture isolates credentials and data, verifies listener ownership before writes, and terminates its server on normal exit or interruption; see the harness for details. Its prebuilt binary identity was not established for that smoke. FFmpeg/VLC headless protocol checks cannot establish guide UI behavior.

Minimum to unblock: one authorized, versioned real IPTV client with a usable graphical session on this machine, or an authorized usable client/device reachable through supported device tools and a safely reachable fixture. Import M3U and XMLTV, inspect IDs and displayed guide times, tune, observe a transition and reconnect, and retain timestamped evidence. Networking guidance is in the client evidence; no client/device was provisioned to bypass the stop.

## Next and remaining evidence

Wave 9 slices 17 and 19 are eligible as independent work under the plan's blocked-slice replan rule and their slice 3 prerequisite; this does not close Wave 8 or unblock slice 14. Slice 18 depends on accepted slice 16; slice 20 depends on slice 19. Hosted CI, manual assistive-technology testing, and real Google accounts/connectivity remain unverified, as do the external provider and live playback limits in prior checkpoints.
