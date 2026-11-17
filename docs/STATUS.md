# Status and remaining work

Date: 2026-11-16
Running: `https://atv.christopherrutherford.net` (public), `http://192.168.1.153:8409` (LAN), `http://100.120.253.100:8409` (Tailscale), `https://nirvana.tail5a4a0.ts.net:8447` (Tailscale HTTPS). Public TLS terminates in SWAG on the VPS; config lives in `deploy/nginx/swag/`.

## Plan coverage

The scope definition is [`PLAN.md`](PLAN.md) and [`PARITY.md`](PARITY.md).
Current implementation coverage, including done/partial/not-started status by
area, is in [`PLAN-REVIEW.md`](PLAN-REVIEW.md).

## Current state

| Area | State |
|---|---|
| Catalog | 10,254 episodes, 76 movies, 301 music tracks, 2 images; Jellyfin sync configured |
| Channels | 12: Movie Night, Animation Station, Adventure Time, Sci-Fi Theater, Doctor Who, Sitcom Central, One Piece, Reality Rundown, Drama After Dark, Family Movie Night, Music Mix, Gundam Channel |
| Collections | 70+ show, movie, music, and filler collections; Movie Night now holds all 76 movies and Adventure Time all 278 episodes |
| Programming | Weekly blocks, block templates, classic sequences, chapter-aware/manual mid-rolls, scripted YAML, external playout JSON, and date-range alternate schedules |
| Graphics | Per-channel text bug, static or animated image/video watermark, and lower third; captured in block templates |
| Streams | Resolution, preset, CRF, audio bitrate, global/per-channel languages, subtitles, HDR tonemap, software/VAAPI, segment size, prebuffer, cache depth, and startup prewarm |
| Precache | 24 h look-ahead, 100 GB LRU, raw copy or 720p proxy for slow sources; status and removal in the admin Sources view |
| Playback | HLS + MPEG-TS/HDHomeRun; trimmed in/out playback; catch-up restart; rolling DVR buffer; music/image items; subtitles, overlays, HDR tonemap, VAAPI; slate fallback |
| Auth | **Watch** at `/`; **Studio** at `/admin` (login, optional OIDC SSO, admin/operator/viewer roles, secure session cookies, login throttling, Basic auth and scoped API tokens) |
| Abuse controls | Per-client public budgets for API, HLS playlists, media segments, Cast, and MPEG-TS; global and per-client MPEG-TS stream leases; `429`/`503` with retry guidance |
| API tokens | Revocable Bearer tokens in Studio Settings; SHA-256 hashes at rest; read/control/admin scopes, expiry, and per-token limits |
| Audit | Durable `audit_events` for login, token, source, backup, and mutation activity; Settings activity view |
| SSO | Optional OIDC discovery/code/PKCE flow with admin/operator/viewer role mapping; env admin remains break-glass |
| Exposure | Optional `AWESOMETV_ADMIN_ALLOW_CIDRS` guard for Studio/admin APIs; security headers on every response |
| Backups | Create/list/download, staged restore on restart |
| Sources | Jellyfin import configured; Emby import implemented; cloud provider registry, HEAD/Range proxy hardening, source edit, background scans, full-rescan reconciliation, Library badges/filter, and per-source/Prometheus metrics |
| UI | ChannelLoom Watch and Studio: cinematic player, channel rail, guide timeline, cast mode, skeletons, responsive layouts, and a 10-foot TV mode |
| Apps | ChannelLoom PWA over Tailscale Serve HTTPS; Android TV wrapper APK; Tauri desktop wrapper |
| Cast | Cast button in Watch and Studio, cast-mode overlay with remote play/pause/mute/volume/stop, channel switching while casting, branded metadata, configurable Styled/Custom receiver, signed Cast HLS and CORS |
| CI | Rust, Docker, Android TV APK, Tauri desktop bundle, PWA asset validation |
| Runtime | Docker on Ubuntu 26.04 with FFmpeg 8.0.1 and VAAPI device passthrough |

## Brand

The product is **ChannelLoom** (one word), with the tagline “Your favorites are always on.”
Watch is `/` and Studio is `/admin`. Internal `awesometv` identifiers, package IDs,
environment variables, routes, and the database name are unchanged. See
[`BRAND.md`](BRAND.md).

## Completed

- **Phase 0:** ADRs, parity checklist, synthetic fixtures, scheduler spike, HLS/HDHR spikes, real-media probes, ARM cross-check for the scheduler/spikes.
- **Phase 1:** SQLite catalog, local scanning, one-channel HLS, M3U/XMLTV, browser UI, Docker.
- **Phase 2, largely done:** broadcast-control UI, source roots, Jellyfin metadata import, collections, weekly blocks, scheduler rebuild, HDHomeRun output, MPEG-TS gateway, producer supervisor, slate fallback, FFmpeg 8, sessions, backup/restore.
- **Phase 3 core complete:** classic ordered sequences, block filler, chapter-aware and manual mid-rolls, block templates, scripted YAML, external playout JSON, alternate schedules, text/image/animated graphics, music and image playout, audio/subtitle selection, subtitle burn-in, HDR tonemapping, and VAAPI/software encoding. Emby import is implemented alongside Jellyfin. Live Plex/Emby/ARM validation remains paused.

## Channel lineup

| # | Channel | Schedule |
|---|---|---|
| 1 | Movie Night | Classic, 4 movies per shuffled cycle, 30-minute chapter-snapped breaks, HDR tonemap |
| 2 | Animation Station | Adventure Time, Bob's Burgers, Futurama, Rick and Morty, The Great North, The Venture Bros., Voltron, X-Men, X-Men '97; 2 episodes each per cycle |
| 3 | Adventure Time | Dedicated classic channel, 4 episodes per shuffled cycle |
| 4 | Sci-Fi Theater | Star Trek, Discovery, Voyager, Stargate SG-1, Atlantis, Battlestar Galactica, The X-Files, Andromeda, The Expanse, For All Mankind, The Twilight Zone, Red Dwarf |
| 5 | Doctor Who | Dedicated classic channel, 4 episodes per cycle, 20-minute chapter-snapped breaks |
| 6 | Sitcom Central | Cheers, The Middle, Community, My Name Is Earl, Raising Hope, Baby Daddy, Maude, Mama's Family, Are You Being Served?, Freaks and Geeks, The War at Home, Little Britain |
| 7 | One Piece | Dedicated classic channel, 4 episodes per shuffled cycle |
| 8 | Reality Rundown | RuPaul's Drag Race franchise, Face Off, America's Funniest Home Videos, Carpool Karaoke |
| 9 | Drama After Dark | Supernatural, Shameless, Buffy, Dexter, Teen Wolf, Breaking Bad, Better Call Saul, The Handmaid's Tale, Animal Kingdom, The Boys, Cobra Kai, Black Sails, The White Lotus, Severance |
| 10 | Family Movie Night | 2 family movies per shuffled classic cycle, HDR tonemap |
| 11 | Music Mix | 12 shuffled music tracks per cycle, audio-only HLS |
| 12 | Gundam Channel | 13 Gundam series, 2 episodes each per cycle |

A **Holiday Movies** alternate schedule runs Family Movies on Movie Night from December 24–26.

## Public access

`https://atv.christopherrutherford.net` is live: DNS points at the Linode VPS,
SWAG terminates TLS with a certificate covering the hostname, and the proxy
reaches the app over Tailscale MagicDNS (`nirvana.tail5a4a0.ts.net:8409`).
Verified end-to-end: login, session cookie, config, channel list, HLS playlist,
MPEG-TS segment fetch, and real Chrome playback (video advanced with no fatal
errors).

Two public-path constraints were found and addressed:

- **Uplink.** Public segments travel `app -> Tailscale -> VPS -> viewer`, so the
  home upload caps every public stream. `AWESOMETV_STREAM_MAX_KBPS=900` keeps a
  single viewer inside the measured path throughput; the per-channel stream
  form can override it. Multiple public viewers share the same upload.
- **fail2ban.** SWAG's stock `nginx-unauthorized` jail banned the home IP after
  five 401s from a logged-out UI poll. The UI now stops polling when
  unauthenticated, and that jail is disabled; the app throttles its own login
  attempts instead.

Both interfaces share a broadcast-control visual language. The watch page caches
channels and guide data in local storage, and the service worker caches public
API responses plus immutable HLS segments so brief network drops and retries do
not restart the buffer.

The public viewer/admin split is in place:

| Route | Access |
|---|---|
| `/` | Public live viewer: channel rail, HLS player, 6-hour guide |
| `/api/v1/public/channels`, `/api/v1/public/guide` | Public, read-only projections |
| `/channel/*`, `/cast/*`, `/catchup/*`, `/dvr/*` | Public signed media routes |
| `/admin` | Login-gated console; anonymous requests receive the login page |
| `/api/v1/*` admin routes | Session or Basic auth required |

## Precache

The media share is a 100 Mbps CIFS mount that sustains roughly 7 MB/s with
~160 ms attribute latency. Direct playback of 4K remuxes (80 Mbps) cannot keep
up, so the producer drifts further behind real time.

The precache service reads the channel guide 24 hours ahead and stages upcoming
media in `/data/precache`:

| Source rate | Mode |
|---|---|
| At or below about 31 Mbps | Byte-for-byte local copy |
| Larger (4K remuxes) | 4 Mbps / 720p H.264 proxy, tonemapped when HDR |

- `AWESOMETV_PRECACHE_MAX_GB=100` LRU budget, 24 h look-ahead, 32-entry queue.
- Jobs are scheduled by deadline (air time minus estimated read time), so a
  multi-hour 4K proxy starts early enough instead of waiting behind near-term
  episodes.
- One job at a time. Copies throttle to about 1.3 MB/s while a live producer
  reads the mount; proxy transcodes are preempted and re-queued when a live
  reader appears.
- `resolve_media_input` prefers a ready cache entry, so cached items produce at
  1x and hold an 8–12 s HLS edge instead of drifting.
- A 4K item that cannot finish before its airing stays uncached for that
  airing; the proxy is used on the next airing.
- `.iso` Blu-ray images are excluded from scheduling; convert them to MKV for
  playout.
- `AWESOMETV_PRODUCER_MAX_LAG_SECONDS=0` disables the optional lag-restart
  guard by default. The cache keeps active producers within the guard threshold.
- The admin Sources view shows ready/queued/failed entries and removes them.

## Cloud sources

The shared cloud-source core and Google Drive reference adapter are implemented:
service-account/access-token auth, encrypted secrets, folder browsing, scan-time
ffprobe, signed byte proxy, hybrid stream/cache, and Library integration.
The public viewer/admin split is complete. Interactive OAuth, provider
webhooks, and the Dropbox/OneDrive/S3/WebDAV adapters remain deferred.

See [`docs/CLOUD-SOURCES.md`](CLOUD-SOURCES.md) for the architecture,
[`docs/CLOUD-COMPLETION.md`](CLOUD-COMPLETION.md) for the remaining work, and
[`docs/P2P-CHANNELS.md`](P2P-CHANNELS.md) for the separate peer-to-peer channel
sourcing design. Sequencing is in
[`docs/CLOUD-AND-P2P-SEQUENCING.md`](CLOUD-AND-P2P-SEQUENCING.md).

## Remaining Phase 2 work

| Step | Estimate |
|---|---|
| Plex / Emby / Channels DVR live validation | paused per request |
| Per-channel health, metrics, and session visibility | 3–5 working days |
| Coverage/slate status in the UI and EPG | 2–3 working days |
| Multi-arch Docker build + ARM runner validation | paused per request |
| Backup upload, restore verification UX, roles | 3–5 working days |
| **Phase 2 polish (excluding paused client/ARM work)** | **8–13 working days (~2–3 weeks)** |

## Phase 3 — deep parity

Phase 3 core is implemented: classic/block/YAML/external scheduling, alternate schedules, templates, filler and mid-rolls, music/image playout, graphics, subtitles, HDR tonemap, stream profiles, VAAPI, and Emby import.

Remaining refinements: content-aware scene/silence break rules, remote streams, richer importers, and live Emby/Plex/ARM validation.

Estimate: **5–10 working days** for content-aware break rules and Phase 3 polish, excluding paused client/hardware validation.

## Phase 4 — scale and polish

Started: stream tuning/metrics, global language defaults, catch-up replay, channel prewarm, a 10-foot TV mode, an installable PWA app shell over Tailscale HTTPS, a rolling DVR buffer, an Android TV wrapper APK, and a Tauri desktop wrapper scaffold. Remaining: Plex/Emby hardening, live sources, SSAI, plugin SDK, multi-node, and signing/distribution for the wrappers. QSV/NVENC are deferred because the current host does not initialize them.

Estimate: **50–70 working days (~2.5–3.5 months)**.

## Public v1 horizon

From today, a realistic public-v1 range is **3–5 months for 1–2 full-time engineers**, or **2–4 months for 2–4 engineers** with parallel work. Plex/Emby/ARM validation is paused, so those risks remain open.

## Known limits right now

- HLS transitions are per-item processes with a supervisor; they are not frame-accurate yet.
- VAAPI is validated on the current host; QSV/NVENC profiles are not exposed yet.
- Global language defaults are English audio and subtitles off. Channels inherit these defaults; per-channel overrides still win. Files with no English track fall back to the first non-commentary audio track, and commentary tracks are skipped when tagged.
- HDHomeRun/TS endpoints are implemented and locally verified, but not yet validated inside Plex/Emby/Channels (paused).
- Metrics are available at `/api/v1/metrics` and Prometheus `/metrics`; the Settings view shows producer health and language defaults.
- Catch-up restarts the current program from its media start in a separate HLS session; it is a single-program replay, not a full DVR buffer.
- Prewarm starts producers for opt-in channels at boot; it consumes encoding CPU per enabled channel.
- Single admin account; no roles.
- Backups are local only; restore requires a restart.
- Mid-rolls use an interval snapped to chapters or explicit per-item markers; scene/silence-aware break detection is not implemented yet.
- Block templates cover weekly blocks and channel graphics; they do not yet capture classic sequences or stream profiles.
- External timelines replace the materialized channel timeline and are preserved until replaced or the channel is rescheduled.
- Graphics support text, static images, animated GIF/video watermarks, and lower thirds; other deco types are not implemented.
- External playout JSON is applied on demand; it is not polled from a remote URL.
- Emby import is implemented but has not been exercised against a live Emby instance.
- Stream profiles support software x264 and VAAPI; QSV/NVENC are not exposed.
