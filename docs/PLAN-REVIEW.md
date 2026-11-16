# Plan review — current state vs the original plan

Date: 2026-11-16
Scope: implementation in this repository plus the live deployment at
`https://atv.christopherrutherford.net`.

## Verdict

ChannelLoom is a **working self-hosted live-TV product**, not a prototype:

- 12 scheduled channels on a 10,254-episode / 76-movie / 301-track library
- public live/guide viewer and a separate login-gated admin console
- HLS, MPEG-TS/HDHomeRun, M3U, XMLTV, browser/PWA/Android TV/Tauri clients
- catch-up, rolling DVR, producer supervision, prewarm, metrics
- Google Cast with signed playlists, a cast-mode overlay, and continuous
  producer handoff
- schedule-driven precache that makes a slow NAS usable for live encoding
- native CI for Rust, Docker, Android TV, Tauri, and PWA assets

It is **not yet the deep-parity public v1** described in `PLAN.md` and
`PARITY.md`. The core product exists; the long tail of sources, scheduling
options, delivery profiles, provider adapters, P2P, RBAC, and multi-arch
packaging is still open.

## What is running

| Area | Current state |
|---|---|
| Public | `https://atv.christopherrutherford.net` (SWAG TLS on the VPS) |
| LAN | `http://192.168.1.153:8409` |
| Tailscale | `http://100.120.253.100:8409`, HTTPS on `:8447` |
| Channels | 12, back-to-back scheduled with fillers and breaks |
| Catalog | 10,254 episodes, 76 movies, 301 music tracks, 2 images |
| Collections | 76 |
| Precache | 10.4 GB ready, rolling 100 GB budget, 30 copies/2 proxies queued |
| Cast | working; Default Media Receiver until a receiver App ID is registered |
| CI | run `43789354460`: all five jobs pass |
| Branch/PR | `feature/cloud-cast-ci`, [PR #1](https://github.com/cjrutherford/awesome-tv/pull/1) |

Public streaming is capped at 900 kbps because every public segment travels
through the home upload. The NAS sustains roughly 7 MB/s, so raw 4K remuxes are
proxied locally before airing.

## Original roadmap coverage

| Phase | Plan | Status | Notes |
|---|---|---|---|
| Phase 0 — foundations/spikes | ADRs, fixtures, scheduler/HLS/HDHR spikes | **Done** | docs/adr, fixtures, spikes recorded |
| Phase 1 — walking skeleton | Rust, SQLite, scan, one channel, HLS/M3U/XMLTV, auth, Docker | **Done** | |
| Phase 2 — usable beta | sources, dayparts/filler, TS/HDHR, studio, backup, hardware, 7-day run | **Mostly done** | Plex source, live client validation, ARM, roles, diagnostics/OpenAPI still open |
| Phase 3 — programming parity | classic/block/scripted schedules, decos, graphics, stream selection | **Core done, long tail open** | many options and the full graphics/stream-rule model remain |
| Phase 4 — media/delivery/ops | music/images/remote, Emby, codecs, cache, import, OIDC, clients | **~One third** | music/images, Emby metadata, HDR tonemap, DVR/catch-up, Cast, precache done; remote/Plex sources, copy mode, codec breadth, OIDC/RBAC/Postgres, import/export, client validation open |
| Phase 5 — scale/polish | 4K/HDR, native apps, live sources, DVR, SSAI, plugin SDK | **Partial** | 4K/HDR/precache, native wrappers, Cast, DVR/catch-up done early; live sources/SSAI/plugin/multi-node not started |

## Parity checklist status

### Sources and metadata

| Item | Status |
|---|---|
| Local movies/shows/other, music, images | **Done** |
| Jellyfin metadata import | **Partial** — metadata import, not a direct streaming source |
| Emby metadata import | **Partial** — implemented, not live-validated |
| Plex source | **Not started** |
| Remote HTTP/HTTPS/RTSP/script media sources | **Not started** |
| NFO metadata | **Partial** — title only |
| Artwork, people, genres, tags, external IDs | **Not started** |
| Path replacements, deep scan, trash/missing handling | **Partial** — roots/scan only |
| Subtitles: embedded/sidecar text | **Done** (burn-in) |
| Image subtitles, WebVTT conversion, font/flag control | **Not started** |
| Chapters, HDR detection, tonemap | **Done** |
| Dolby Vision, rotation, interlacing, aspect-ratio handling | **Not started** |

### Library and discovery

| Item | Status |
|---|---|
| Manual collections | **Done** |
| Smart collections | **Not started** |
| Playlists / playlist groups | **Not started** |
| Trakt lists | **Not started** |
| Rich search and relative-date queries | **Not started** — UI search is title + kind only |
| Bulk add/drag ordering | **Partial** — selection exists; no drag/playlists |

### Scheduling

| Item | Status |
|---|---|
| Lineup/classic ordering, shuffled cycles | **Done** |
| Weekly blocks/dayparts, templates, alternate date ranges | **Done** |
| Filler and chapter/interval mid-rolls | **Done** |
| Full ErsatzTV classic option matrix (starts/policies/flood/tail/EPG modes) | **Partial** |
| Block decos/deco templates/playout templates | **Not started** |
| Sequential YAML, scripted YAML, external playout JSON | **Partial** — supported subset; no external executable scheduler |
| Full instruction/expression language | **Partial** |
| Multi-part grouping, random start, daily reset | **Not started** |
| On-demand and mirror channels | **Not started** |
| Playout history, warnings, preview, troubleshooting | **Partial** — rebuild warnings/UI, no history |
| Import/export from ErsatzTV/Tunarr/dizqueTV | **Not started** |

### Presentation

| Item | Status |
|---|---|
| Channel number/name, single watermark, lower third, text bug | **Done** |
| Channel group/logo | **Not started** |
| Multiple watermarks, intermittent/expressions, logo-as-watermark | **Not started** |
| Graphics engine (motion, script, z-order, timing) | **Partial** — static/animated watermark + text overlays only |
| Stream selection rules | **Partial** — audio language, subtitle mode; no allow/block/codec/condition rules |
| Subtitle burn / WebVTT conversion | **Partial** — burn only |
| Per-channel and global profiles | **Partial** — resolution/preset/CRF/audio/encoder/segments; no overlays or full profiles |

### Transcoding and playback

| Item | Status |
|---|---|
| HLS TS, MPEG-TS gateway, HDHomeRun, M3U, XMLTV | **Done** |
| Software and VAAPI | **Done** |
| QSV, NVENC, AMF, VideoToolbox, RKMPP, Vulkan | **Not started** |
| Copy mode | **Not started** |
| H.264/AAC | **Done** |
| HEVC/MPEG-2 output, 10-bit, deinterlace, frame-rate normalization, anamorphic/rotation | **Not started** |
| AC3/E-AC3/MP3/copy audio, loudness normalization | **Not started** |
| HDR10 tonemap | **Done** (software) |
| Dolby Vision tonemap | **Not started** |
| Segment cache / pre-seed | **Partial** — media-level precache, not segment pre-seed |
| Fast tune-in, prebuffer, TS burst | **Done** |
| Per-item fallback, retry, circuit breaker, diagnostics bundle | **Partial** — supervisor/slate, no circuit breaker/bundle |
| HLS fMP4 | **Not started** |
| Tuner limits / idle stop | **Partial** — idle stop done; HDHR tuner limit is advertised, not enforced per client |

### Clients and interoperability

| Item | Status |
|---|---|
| Browser/PWA | **Done** |
| Android TV APK | **Done** — debug build in CI |
| Tauri desktop | **Done** — Linux deb build in CI |
| HDHomeRun/M3U/XMLTV | **Done** |
| Plex/Jellyfin/Emby/Channels/Kodi/TiviMate live validation | **Partial/not validated** |
| M3U/XMLTV metadata parity (icons, categories, episode numbers, ratings, credits) | **Minimal** |
| Import/export channel plans and timelines | **Partial** — external playout apply; no import/export |

### Operations, security, API

| Item | Status |
|---|---|
| Docker amd64, SQLite, config, logs, health | **Done** |
| Single admin, backups/restore | **Done** |
| Public viewer/admin split | **Done** |
| Login throttling, secure cookies, signed Cast/cloud URLs, encrypted secrets | **Done** |
| API tokens | **Done** — revocable Bearer tokens, hashed at rest, managed in Studio |
| OIDC, roles, audit log | **Not started** — next identity milestone |
| PostgreSQL optional | **Not started** |
| OpenAPI, diagnostics bundle, capacity calculator, update check | **Not started** |
| Prometheus metrics | **Done** — endpoint only, no dashboards |
| Public media rate limits (HLS/Cast/DVR) | **Done** — per-client budgets for API, playlists, segments, Cast, and MPEG-TS |
| Concurrent MPEG-TS tuner caps | **Done** — global + per-client leases, advertised tuner count matches |
| SSRF/path/command review, secret rotation | **Done for current surfaces** — HTTPS/private-host validation, encoded remote IDs, path checks, key rotation with previous keys |
| Docker arm64 / ARM runner validation | **Paused/not done** — scheduler cross-check only |

## Workstreams

### Cloud providers

| Phase | Status |
|---|---|
| C0 — real Google credentials/test folder | **Not done** |
| C1 — provider trait, resumable scans, delta, edit, cache pinning, badges, metrics | **Partial** — core/Google adapter, source edit, full-rescan reconciliation, source badges/filter, and cloud metrics done; missing trait extraction, background/resumable scans, incremental delta, cache pins |
| C2 — OAuth, webhooks, public exposure | **Split done; OAuth/webhooks not started** |
| C3 — Dropbox, OneDrive, S3, WebDAV | **Not started** |
| C4 — conformance, security review, performance, docs | **Not started** |

Only Google Drive is implemented. The public viewer/admin split (a C2 dependency)
is complete.

### P2P channel sourcing

| Phase | Status |
|---|---|
| P0 — identity, well-known endpoint, manual peer | **Not started** |
| P1 — pairing, grants, shared channel selection | **Not started** |
| P2 — remote guide, M3U/XMLTV, HLS proxy | **Not started** |
| P3 — multi-peer hardening | **Not started** |

Design only: `docs/P2P-CHANNELS.md`.

### Native / CI / Cast / security

- **Native:** Android TV, Tauri Linux, and PWA are built in CI. No Windows/macOS
  packaging, signing, Play Store/Tauri distribution, or mobile companion.
- **Cast:** baseline, branded metadata, custom receiver page, overlay controls,
  channel switching, and continuous item handoff are done. The receiver App ID
  still needs to be registered by the operator.
- **Security:** public split, login throttle, secure cookies, signed media,
  encrypted cloud secrets, API tokens, public HLS/Cast/DVR rate limits,
  concurrent stream caps, SSRF/path review, and key rotation. Still open:
  OIDC/RBAC/audit, webhook verification, and optional Studio network restriction.

## Plan deviations and decisions

1. **UI stack.** The plan chose React + TypeScript. The shipped UI is a
   single-file vanilla HTML/JS app (`web/public.html`, `web/index.html`). It is
   fast and worked well for this stage, but it is a deliberate deviation. Decide
   whether to migrate to React before the admin surface grows further.
2. **Cloud scope.** The plan called for five providers. Only Google Drive is
   implemented; the trait/registry refactor is still needed before adding more.
3. **DVR/catch-up/precache/Cast/public exposure** were implemented earlier than
   the roadmap. They are useful, but they pulled effort from deep parity.
4. **Multi-arch and hardware.** Only amd64 Docker is built; QSV/NVENC/AMF/
   VideoToolbox/RKMPP are not exposed. The plan's low-power/ARM target remains
   unvalidated.
5. **Sources.** Jellyfin/Emby are metadata imports over local files, not direct
   streaming sources. Plex and remote/RTSP/script sources are absent.
6. **Public delivery** is constrained by the home upload (900 kbps cap) and the
   NAS read rate (7 MB/s). Precache is the mitigation, not in the original plan.
7. **Testing.** 39 unit tests and build CI, but no API integration suite, browser
   E2E, provider conformance suite, or capacity test.

## Highest-priority gaps

Recommended order, independent of calendar estimates:

1. **Public security hardening**
   - ~~rate limits for public HLS/Cast/DVR~~ (done)
   - ~~API tokens~~ (done)
   - ~~SSRF/path/command review; secret rotation runbook~~ (done for current surfaces)
   - decide whether `/admin` should be LAN/Tailscale-only until SSO exists
   - OIDC/roles/audit
2. **Cloud C0/C1**
   - validate a real Google service account and test folder
   - provider trait/registry, resumable scans, delta/deletion reconciliation
   - source edit, cache pinning, source badges, cloud metrics
3. **Deep-parity media and clients**
   - Plex source, remote HTTP/RTSP/script sources
   - copy mode, HEVC/MPEG-2, AC3/E-AC3/MP3, loudness
   - hardware profiles (QSV/NVENC/AMF/VideoToolbox/RKMPP)
   - live validation in Plex/Jellyfin/Emby/Channels/Kodi/TiviMate
   - richer XMLTV/M3U metadata
4. **Scheduling/graphics long tail**
   - full classic option matrix and block decos/playout templates
   - full sequential YAML/expressions, scripted external schedules
   - on-demand/mirror channels, playout history
   - graphics engine and stream-selection rules
5. **P2P P0–P2** if peer sharing is still a priority.
6. **Operations and packaging**
   - arm64 image/runner, optional PostgreSQL, OpenAPI, diagnostics, capacity
   - release signing/distribution for Android/Tauri; Windows/macOS
   - import/export from ErsatzTV/Tunarr/dizqueTV
7. **UI decision**
   - keep the vanilla UI, or migrate to React/TypeScript before adding deep
     scheduling/graphics editors.

## Operational limits to keep in mind

- Public streams are capped at 900 kbps; multiple public viewers share the home
  upload. 4K public playback is not realistic.
- Precache works with a 100 GB budget; 4K remuxes are proxied and may miss their
  first airing if they cannot finish in time.
- ISO images are excluded from scheduling; convert them to MKV.
- Sessions are in memory; a restart signs everyone out.
- The receiver is still Google's Default Media Receiver until an App ID is set.
- HLS producers are per-item processes; the playlist handoff is continuous, but
  slates/gaps still reset the playlist and transitions are not frame-accurate.
- Single admin, local backups, no roles/audit.
