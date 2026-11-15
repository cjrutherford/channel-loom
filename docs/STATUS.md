# Status and remaining work

Date: 2026-11-15
Running: `https://atv.christopherrutherford.net` (public), `http://192.168.1.153:8409` (LAN), `http://100.120.253.100:8409` (Tailscale), `https://nirvana.tail5a4a0.ts.net:8447` (Tailscale HTTPS). Public TLS terminates in SWAG on the VPS; config lives in `deploy/nginx/swag/`.

## Current state

| Area | State |
|---|---|
| Catalog | 10,254 episodes, 76 movies, 301 music tracks, 2 images; Jellyfin sync configured |
| Channels | 12: Movie Night, Animation Station, Adventure Time, Sci-Fi Theater, Doctor Who, Sitcom Central, One Piece, Reality Rundown, Drama After Dark, Family Movie Night, Music Mix, Gundam Channel |
| Collections | 70+ show, movie, music, and filler collections; Movie Night now holds all 76 movies and Adventure Time all 278 episodes |
| Programming | Weekly blocks, block templates, classic sequences, chapter-aware/manual mid-rolls, scripted YAML, external playout JSON, and date-range alternate schedules |
| Graphics | Per-channel text bug, static or animated image/video watermark, and lower third; captured in block templates |
| Streams | Resolution, preset, CRF, audio bitrate, global/per-channel languages, subtitles, HDR tonemap, software/VAAPI, segment size, prebuffer, cache depth, and startup prewarm |
| Playback | HLS + MPEG-TS/HDHomeRun; trimmed in/out playback; catch-up restart; rolling DVR buffer; music/image items; subtitles, overlays, HDR tonemap, VAAPI; slate fallback |
| Auth | Session login/logout with `Secure` cookies behind TLS, per-IP login throttling (10 failures / 15 min); Basic auth retained for API clients |
| Backups | Create/list/download, staged restore on restart |
| Sources | Jellyfin import configured; Emby import implemented (no live instance configured) |
| UI | Broadcast-control console plus a 10-foot TV mode with large video, channel strip, keyboard navigation, and live clocks |
| Apps | Installable PWA over Tailscale Serve HTTPS; Android TV wrapper APK; Tauri desktop wrapper scaffold |
| Cast | Cast button, signed Cast HLS playlist/segments, Default Media Receiver, configurable Cast base URL |
| CI | Rust, Docker, Android TV APK, Tauri desktop bundle, PWA asset validation |
| Runtime | Docker on Ubuntu 26.04 with FFmpeg 8.0.1 and VAAPI device passthrough |

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

The public viewer/admin split is still pending, so the hostname currently
serves the protected admin console.

## Cloud sources

The shared cloud-source core and Google Drive reference adapter are implemented:
service-account/access-token auth, encrypted secrets, folder browsing, scan-time
ffprobe, signed byte proxy, hybrid stream/cache, and Library integration.
Interactive OAuth, provider webhooks, the public viewer/admin split, and the
Dropbox/OneDrive/S3/WebDAV adapters are deferred.

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
