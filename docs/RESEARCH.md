# ErsatzTV and the personal broadcast ecosystem — research notes

Status: research complete, November 2026
Purpose: establish a factual baseline before designing **ChannelLoom** (working name), a clean-room, modern replacement for ErsatzTV.

> **Clean-room note.** This document is based on public documentation, release notes, public project metadata, user-facing behaviour, and public issue trackers. No ErsatzTV, ErsatzTV Next, or Tunarr source code is to be copied into ChannelLoom. See `docs/CLEAN-ROOM.md`.

---

## 1. Executive summary

ErsatzTV is a self-hosted application that turns a personal media library into scheduled, always-on TV channels. It presents those channels to Plex, Jellyfin, Emby, Channels DVR, Kodi, TiviMate, and generic IPTV clients through HDHomeRun tuner emulation, M3U playlists, and XMLTV guide data. It transcodes with FFmpeg, supports hardware acceleration, and offers unusually deep scheduling (classic, block, sequential YAML, scripted, external JSON) plus watermarks, graphics, filler, chapters, music videos, images, and remote streams.

The project's history matters for product strategy:

- ErsatzTV Legacy (the original .NET application) reached its **final feature release, 26.12.0, on 6 November 2026**. It was archived in February 2026, then unarchived and renamed Legacy. It receives security updates, not feature development.
- **ErsatzTV Next** (Rust, MIT) is a rewrite of the **streaming engine only**. It consumes JSON playouts and produces HLS, M3U, and XMLTV. Scheduling, library management, and metadata are explicitly out of scope. It is bundled as Legacy's default engine and can run standalone.
- **Tunarr** (TypeScript/Fastify/React, zlib) is now the most actively developed all-in-one alternative. It has modern UX, Plex/Jellyfin/Emby/local sources, several scheduling tools, filler, watermarks, and an experimental integration with ErsatzTV Next as a streaming backend.
- The author of ErsatzTV and the author of Tunarr are collaborating on ErsatzTV Next as a shared low-level engine, while keeping their higher-level products separate.

This leaves a real gap: **no actively developed product combines ErsatzTV's scheduling depth and content breadth with a modern, reliable, understandable experience.** Tunarr is simpler and more polished but deliberately does less; ErsatzTV Legacy is deeper but frozen and operationally heavy; ErsatzTV Next is an engine, not a product.

ChannelLoom should therefore be:

> A reliable, understandable personal broadcaster: deep enough to program a real station, simple enough to get one on air in an afternoon, and observable enough to trust it unattended.

---

## 2. What ErsatzTV actually provides

### 2.1 Media sources

| Source | What it supports | Notes |
|---|---|---|
| Local files | Movies, TV shows, music videos, other videos, songs, images, remote streams | No media server required. Multiple libraries and paths. NFO metadata with minimal fallback. |
| Plex | Movies and shows | One-way metadata sync. Plex Pass required for Plex Live TV. Can stream from Plex or from disk with matching mounts/path replacements. Supports shared servers, Plex collections, Other Video libraries. |
| Jellyfin | Movies and shows | Server URL + API key. One-way sync. Stream from server or disk with path replacements. Handles external subtitles and authenticated media streams. |
| Emby | Movies and shows | Same shape as Jellyfin. |
| Remote streams | HTTP/HTTPS/RTSP/command sources | YAML definition with `url` or `script`, `is_live`, optional `duration`, title/plot/year/rating/thumbnail. Live content cannot work ahead. |
| Songs | Audio files | Embedded tags, album art, background, optional progress overlay. |
| Images | Stills | Default 15 seconds; per-folder durations with inheritance. |
| Music videos | Artist/album/track | Artist NFO, thumbnails, credits templates. |

Metadata is read from Kodi-style NFO files for local libraries, or supplied by the media server. Local fallback metadata is intentionally minimal (title/year/season/episode from folder and file names). Scans are incremental; there are deep scans, trash for missing files, and path-move support.

### 2.2 Organization and discovery

- **Manual collections**: arbitrary mix of media kinds, custom ordering, add via browse or search.
- **Smart collections**: saved searches that update dynamically.
- **Multi collections**: collections of collections and smart collections, useful for box sets and franchises.
- **Playlists** and playlist groups: ordered lists whose items can be collections, shows, seasons, episodes, artists, movies, music videos, other videos, songs, images, remote streams. Each playlist item has playback order, "play all", EPG visibility, and count.
- **Rerun collections**: replay previously aired content in a controlled way.
- **Trakt lists**: import, auto-refresh daily, optionally generate a playlist from matched items.
- **Search**: rich field-based search across movies, shows, seasons, episodes, artists, music videos, other videos, songs, images, and remote streams. Fields include title, genre, tag, collection, plot, studio, actor, director, writer, library, content rating, languages/subtitle languages, release/added dates, chapters, duration, resolution, codec, bit depth, dynamic range, season/episode, show metadata, external IDs. Special relative-date fields (`released_inthelast`, `added_inthelast`, `released_onthisday`, etc.).

### 2.3 Scheduling and playout

ErsatzTV has five scheduling models plus external integrations. This is the main source of both its power and its complexity.

#### Classic schedules

An ordered list of schedule items. Each item selects a collection type (manual collection, show, season, artist, multi collection, smart collection, playlist) and defines:

- **Start type**: dynamic (start after previous item) or fixed (specific time).
- **Fixed start behaviour**: strict (wait for the exact time, even next day) or flexible (start immediately if waiting would cross into the next day; later changed to a 12-hour rule).
- **Playback order**: chronological, random (with repeats), shuffle (no repeats until exhausted), shuffle in order (groups kept chronological, groups shuffled), season/episode.
- **Playout mode**: flood (play forever/until next fixed start), one, multiple, or duration.
- **Multiple mode**: count, collection size, multi-episode group size, playlist item size.
- **Fill with group mode**: none, ordered groups, shuffled groups (fills a duration/multiple with one show/artist before rotating).
- **Tail mode** for duration items: none, offline image, filler.
- **Discard-to-fill attempts**: how many items to skip when nothing fits.
- **Custom title**: group items under one EPG title.
- **Guide mode**: normal or filler (hide filler from EPG and extend the previous entry).
- **Filler presets**: pre-roll, mid-roll, post-roll, tail, fallback with count/duration/pad/random-count modes.
- **Watermark and graphics overrides**.
- **Schedule-level options**: keep multi-part episodes together, treat collections as shows (cross-show multi-part), shuffle schedule items, random start point.
- **Alternate schedules**: date-range-based replacements, including recurring yearly ranges.
- **Daily rebuild**, playout history, reset, erase, preview, troubleshooting.

#### Block schedules

A daypart-oriented model built from:

- **Blocks**: fixed-duration collections of block items, each with collection type, playback order, EPG visibility, watermark disabling, and implicit playout mode "one".
- **Templates**: when blocks should run during a non-specific day (weekday/weekend patterns).
- **Decos** ("decorations"): watermark behaviour, default filler, dead-air fallback, trim-to-fit, watermark during filler.
- **Deco templates**: when decos apply.
- **Playout templates**: combine block templates and deco templates on specific days, with priority and date ranges.

This is the closest model to real broadcast dayparts: sitcoms 6–9pm, movies after 9pm, overnight reruns, seasonal changes.

#### Sequential schedules (YAML)

A declarative programming language:

- **Content sources**: search, show (by external GUID), collection, multi collection, smart collection, playlist, marathon (mixed shows/searches grouped by show/season/artist/album).
- **Sequences**: reusable instruction blocks.
- **Reset instructions**: `skip_items`, `skip_to_item`, `wait_until`.
- **Playout instructions**: `all`, `count`, `duration`, `pad_to_next`, `pad_until`, `sequence`.
- **Control instructions**: `epg_group`, `graphics_on`, `graphics_off`, `repeat`, `shuffle_sequence`, `skip_items`, `skip_to_item`, `wait_until`, `watermark`.
- **Parameters**: trim, discard attempts, stop-before-end, offline tail, fallback, custom title, filler kind, repeat, etc.
- **Expressions** for counts and conditions.

This is extremely expressive, but requires editing YAML and understanding the instruction model.

#### Scripted schedules

An external executable is called with the API host, build ID, build mode (`reset` or `continue`), and user arguments. It drives the build through ErsatzTV's OpenAPI endpoints. This enables arbitrary schedulers but requires programming.

#### External JSON playouts

ErsatzTV can consume externally generated playout JSON. This is the bridge used by ErsatzTV Next and other tools.

#### Playouts and EPG

A playout assigns a schedule to a channel and tracks ordered playback. The EPG is generated from the materialized playout with configurable horizon, time zone (local/UTC), templates, block behaviour, artwork, and custom titles. Playout items are persisted with absolute start/finish times, history, warnings, and build status.

### 2.4 Channels and presentation

- Channel numbers can be whole numbers or one decimal (e.g. `500.5`).
- Channel name, group, logo upload/URL/generated logo, external logo URL.
- Streaming modes: HLS Segmenter, MPEG-TS, HLS Direct, MPEG-TS (Legacy). By 26.12 the legacy engine is removed; HLS Direct and TS Legacy URLs redirect to the Next engine.
- FFmpeg profile per channel.
- Stream selector mode: default or custom YAML rules based on audio/subtitle language, title allow/block lists, stream flags, codecs, channels, and content conditions (channel, time of day, day of week).
- Subtitle mode: none, forced, default, any.
- Watermark: global/channel, image, position, scale, opacity, permanent/intermittent/opacity expression, z-index, multiple watermarks, channel logo as watermark.
- Graphics engine: image, text, subtitle, motion, and script elements; Scriban templates; EPG and media-item variables; z-order; timing; per-schedule-item/block/deco enablement.
- Channel settings: slug seconds (black/silence between items, experimental), active mode/on-demand, idle behaviour, transcode mode, streaming engine, text subtitle mode, playout source, mirror channel, disabled/hidden, EPG settings, tuner count.
- Web preview and channel troubleshooting.

### 2.5 Transcoding

FFmpeg profiles are per channel and include:

- Hardware acceleration: NVENC/CUDA, Intel QSV, VAAPI, AMD AMF (Windows), Apple VideoToolbox, Rockchip RKMPP, Vulkan Video.
- Video codecs: H.264, HEVC, MPEG-2; AV1 was removed.
- Resolution, bitrate, buffer, frame-rate normalization, deinterlacing, scaling behaviour (scale/pad, stretch, crop), bit depth, color normalization.
- Audio: AAC, AC3, E-AC3, MP3, AAC-LATM (DVB-C), copy; channels, sample rate, bitrate, volume, loudness normalization (EBU R128 / target LUFS).
- Subtitles: burn-in or convert text to WebVTT; image subtitles always burned; fonts, force style, embedded vs external selection.
- Stream copy: copy video/audio when source matches; force transcode for graphics, burned subtitles, stills, Dolby Vision profile 5, AVI, etc.
- HDR10/Dolby Vision tonemapping, hardware and software paths, libplacebo, OpenCL, VAAPI, QSV.
- Per-channel config overlays (`default.json`, `{channel}.json`) for advanced Next engine settings.
- Error/offline fallback: background image, errors on black/silence, fallback duration, retry.
- Work-ahead, HLS segment count, MPEG-TS initial burst, idle timeout, session stop.
- Hardware capability probing and fallback to software per step.
- FFmpeg builds are custom-patched (ErsatzTV-ffmpeg) for hardware quirks.

### 2.6 Outputs and clients

- **HLS** per channel (`/iptv/channel/{n}.m3u8` or Next `/channel/{n}.m3u8`), with MPEG-TS or fMP4 segments, sliding window, program date time, discontinuities, subtitles, and work-ahead.
- **MPEG-TS** wrapper for HDHomeRun-style clients (especially Plex).
- **M3U** channel list with `tvg-id`, `tvg-chno`, `tvg-name`, `tvg-logo`, `group-title`, stream URL.
- **XMLTV** guide with channels/programmes, titles, sub-titles, descriptions, categories, episode numbers, icons, ratings, credits, dates.
- **HDHomeRun emulation**: discovery/control protocol on UDP/TCP 65001, HTTP `discover.json`, `lineup.json`, `lineup_status.json`, `device.xml`, `/auto/v{channel}`, `/tuner{n}/...`; tuner count; MPEG-TS output.
- Clients documented: Plex, Jellyfin, Emby, Channels DVR, Kodi, TiviMate; generic IPTV clients.
- Plex requires Plex Pass and MPEG-TS for direct client playback. Jellyfin/Emby can use M3U or HDHomeRun.
- Auth: OIDC for management UI; JWT query-string access tokens for streaming; API key for `/api`; separate UI/streaming ports; reverse-proxy base URL; instance ID for EPG disambiguation.

### 2.7 Operations, API, and support

- Cross-platform: Docker, Windows, Linux, macOS; amd64 and arm64 (armv7 dropped).
- SQLite default; MySQL/MariaDB experimental; Elasticsearch optional for search.
- Config folder, database, artwork cache, subtitles, sequential/scripted schedules, graphics templates, channel config overlays.
- Health checks for common misconfigurations (FFmpeg filters, hardware, database, docker tag, schedule emptiness, etc.).
- Troubleshooting: per-item playback test, per-channel test at a date/time, logs, FFmpeg profiles, hardware info, media info, download/upload encrypted results, media sample.
- REST API with OpenAPI/Swagger; scripted schedule API; API key; logs; UI language; light/dark mode.
- No built-in DVR/recording; no native TV apps; no first-class multi-user accounts beyond OIDC; no built-in backup/restore in Legacy (Tunarr has backup).

---

## 3. Architecture teardown

### 3.1 ErsatzTV Legacy

Public project structure and manifests indicate:

- **.NET 10 / ASP.NET Core** application, Razor/Blazor UI with MudBlazor, some Vue tooling.
- **CQRS/MediatR** application layer, domain/core projects, EF Core persistence with SQLite and MySQL providers, scanner worker, FFmpeg projects, tests.
- **External FFmpeg/ffprobe** processes, not in-process libav.
- **Scanner** imports local media or syncs Plex/Jellyfin/Emby metadata into the database.
- **Scheduler** materializes playout items into the database with absolute times.
- **Streaming engine** reads the current playout and runs FFmpeg to produce HLS/TS; Legacy now delegates to ErsatzTV Next by default.
- **Output layer** serves HLS, MPEG-TS, M3U, XMLTV, and HDHomeRun endpoints.
- **Search** over indexed metadata; optional Elasticsearch.

Strengths: deep domain model, broad media kinds, many scheduling modes, mature edge-case handling.
Weaknesses: large surface area, heavy configuration model, static playout building, many hardware-specific paths, operational complexity, and a codebase that grew beyond sustainable maintenance.

### 3.2 ErsatzTV Next

- **Rust workspace** with crates for FFmpeg pipeline, playout models, channel worker, HTTP server, core utilities, and a demo playout generator.
- **Contract-first**: JSON schemas for `playout.json`, `channel_config.json`, `lineup_config.json`, each versioned by URI. Breaking changes bump versions.
- **Playout files** are time-windowed (`{start}_{finish}.json`) and contain ordered items with absolute start/finish, sources, track selections, graphics, and optional dynamic sources.
- **Sources**: local file, lavfi, HTTP(S), RTSP, script (MPEG-TS stdout), dynamic (HTTP resolver returning a playout item at playback time).
- **Channel worker** normalizes every item to the channel's configured video/audio/subtitle format, handles HDR/DV tonemapping, deinterlacing, rotation, anamorphic content, copy mode, overlays, subtitles, fallback, retries, and diagnostics.
- **Output**: HLS per channel, M3U, XMLTV; no scheduler, no library, no metadata.
- **Capabilities**: runtime probe of hardware encode/decode/filter support; software fallback; per-channel config overlays.
- **Session model**: `.heartbeat`, `.ready`, idle timeout, segment window, work-ahead, ready segment count.
- **Why it matters**: it is a clean, well-documented engine contract and a potential interoperability target. It is not a complete product.

### 3.3 Tunarr

- **Backend**: Node.js/TypeScript, Fastify, Inversify, SQLite (better-sqlite3), Drizzle/Kysely, Meilisearch.
- **Frontend**: React, Vite, Material UI, TanStack Router/Query, Zustand, React Hook Form/Zod.
- **Media**: Plex/Jellyfin/Emby/local; no remote streams yet (open request).
- **Scheduling**: slot editor (fixed/dynamic, random/sequential, weights, cooldowns, pad times, flex style, season filters, slot linking), time slots (start times, pad, max lateness, max overflow), block shuffle, cyclic shuffle, mid-roll breaks, replicate/consolidate.
- **Playback**: HLS, HLS alt, HLS Direct, HLS Direct v2, MPEG-TS; per-program FFmpeg processes; hardware acceleration; watermarks; stream selection profiles; backup; ErsatzTV Next backend experimental.
- **Strengths**: polished UI, fast first-channel path, active development, good docs, backup, stream selection profiles, bulk actions.
- **Gaps**: static precomputed schedules, no true infinite/dynamic schedules yet, no block/daypart model as deep as ErsatzTV, no remote streams, limited multi-audio/soft-subtitle exposure, no smart-collection filler, no built-in restore, Plex Windows client caveat, search index dependency, and still pre-1.0.

---

## 4. Where ErsatzTV hurts

These are the recurring themes in documentation, release notes, issue trackers, and community discussion. They are design requirements for ChannelLoom, not reasons to copy ErsatzTV.

### 4.1 Product and UX

- **Too many overlapping concepts**: collection vs smart collection vs multi collection vs playlist vs rerun collection; schedule vs schedule item vs playout; block vs template vs deco vs deco template vs playout template; sequential vs scripted vs external JSON. Users must learn the data model before they can program a channel.
- **Configuration before confidence**: you build collections, schedules, playouts, then attach a channel, then configure clients. There is no single "channel studio" that shows the result.
- **Error recovery is opaque**: impossible durations, empty collections, missing files, and invalid items can stall a playout or take a channel offline. Fixes often require erasing history/resetting.
- **Hardware setup is a matrix**: accel + device + driver + codec + filter + container + client. Health checks help, but failures are still common.
- **Plex-specific constraints**: Plex Pass, MPEG-TS-only direct playback, client boundary issues.
- **No native client**: everything depends on third-party player behaviour.

### 4.2 Scheduling engine

- **Static precomputation**: build a horizon, then serve it. Schedule changes, newly added media, and late-arriving metadata do not flow naturally into the current window.
- **State and history are fragile**: shuffle bags, multi-part grouping, playlist/marathon positions, on-demand checkpoints, and mid-roll state can drift or reset.
- **Edge cases multiply**: fixed start + overflow + duration + filler + trim + discard + group mode + alternate schedules + DST.
- **No first-class simulation/invariants**: users discover scheduling bugs only when a channel goes wrong.
- **Channel-offline failure mode**: a single impossible item or build failure can stop programming.

### 4.3 Media and metadata

- **Scan/sync bugs**: duplicate tags, lost tags, stale metadata, path issues, external subtitles, missing artwork, library type changes.
- **Media server coupling**: Plex/Jellyfin/Emby APIs and auth change; tokens leak/rotate; path mapping is error-prone.
- **Local metadata is minimal without NFO**: no built-in metadata agent, so local libraries depend on external tooling.
- **Large libraries stress the database/search index**: artwork, people, tags, and orphan cleanup are recurring issues.

### 4.4 Playback and transcoding

- **Hardware/driver regressions**: QSV/VAAPI/CUDA/VideoToolbox each have model-specific quirks; custom FFmpeg patches are required.
- **Startup latency**: tune-in can take seconds; MPEG-TS clients need a burst; segment readiness matters.
- **Boundary glitches**: codec/resolution/framerate/audio changes between items cause client resets or stutter.
- **Transcode cost**: continuous channels consume GPU/CPU; capacity planning is left to the user.
- **Subtitle complexity**: burn vs convert, image vs text, fonts, external files, HDR, positioning, client support.

### 4.5 Operations

- **Database growth/corruption/locking** reports; WAL/checkpoint/vacuum needed.
- **No built-in backup/restore** in Legacy; Tunarr added backup but not restore.
- **No first-class metrics/alerting**; logs and health checks only.
- **Frozen feature development**; security-only maintenance; forks are encouraged.
- **Support burden from scope creep** was the stated reason the original project wound down.

---

## 5. Competitive landscape

| Product | Model | Strengths | Weaknesses / gaps |
|---|---|---|---|
| **ErsatzTV Legacy** | All-in-one, .NET | Deepest scheduling (classic/block/sequential/scripted), broad media kinds, graphics, remote streams, mature edge cases | Frozen; complex; heavy; static playout; hardware matrix; operational support burden |
| **ErsatzTV Next** | Engine only, Rust | Reliable normalized HLS, versioned JSON contract, hardware probing, overlays, fallback, dynamic sources | No library, metadata, scheduler, or UI; pre-1.0; requires external playout producer |
| **Tunarr** | All-in-one, TypeScript | Modern UI, active, Plex/Jellyfin/Emby/local, slot/time-slot/block/cyclic/mid-roll, filler, stream selection profiles, backup, Next backend | Static precomputed schedules; less scheduling depth; no remote streams; limited multi-audio/soft subs; no smart-collection filler; no restore; Plex Windows caveat |
| **dizqueTV** | All-in-one, Node | Established, Plex-only, simple | Maintenance mode; Plex-only; basic scheduling; old stack |
| **Threadfin / xTeVe** | M3U/XMLTV proxy | Mature proxy/filter for IPTV sources | Not a channel generator; no library scheduling |
| **Channels DVR** | Commercial | Polished whole-home DVR, clients, guide, recording | Paid; not a personal-library channel generator in the same sense |
| **QuasiTV / NostalgiaTV / Coax** | Client-side | Zero server, good TV UX | Limited control, platform-specific, not a server |
| **channelarr / showrunner / fast-tv-playout-core** | Adjacent engines | VOD-to-live, pre-segmented stitching, SSAI, EPG | Not personal media all-in-one; different target (FAST/OTT, YouTube, etc.) |

**Positioning gap:** an all-in-one product that keeps ErsatzTV's scheduling intent and content breadth, adopts Tunarr's clarity and modern UX, and uses a Next-class engine with better observability, recovery, and performance.

---

## 6. What "modern" should mean

Modern is not a framework choice. It should mean:

1. **Understandable model**: one channel studio, progressive disclosure, no need to learn five schedule types to start.
2. **Reliable playout**: channels do not go offline because one item is bad; fallbacks, retries, and self-healing are default.
3. **Fast tuning**: pre-warmed or cached segments, fast first frame, no ten-second startup.
4. **Efficient**: transcode only when needed; copy/direct-play when possible; pre-segment or cache intelligently; on-demand by default.
5. **Observable**: per-channel health, now/next, viewer sessions, transcode speed, buffer, errors, diagnostics, metrics.
6. **Recoverable**: built-in backup/restore, export/import, safe migrations, undo for schedule edits.
7. **Interoperable**: HLS/TS, M3U/XMLTV, HDHomeRun, Plex/Jellyfin/Emby, import/export, public API, optional external engine/scheduler contracts.
8. **Secure by default**: local auth, OIDC, scoped API tokens, streaming tokens, secret redaction, SSRF/path/command protection.
9. **Sustainable**: small core, clear module boundaries, automated tests, hardware matrix CI, no feature sprawl.

---

## 7. Sources

- ErsatzTV site and docs: <https://ersatztv.org/>
- ErsatzTV Legacy docs: <https://ersatztv.org/docs/>
- ErsatzTV Next docs: <https://ersatztv.org/next-docs/>
- ErsatzTV Legacy repository and changelog: <https://github.com/ErsatzTV/legacy>
- ErsatzTV Next repository and schemas: <https://github.com/ErsatzTV/next>
- ErsatzTV final release announcement: <https://github.com/ErsatzTV/legacy/issues/2839>
- Tunarr docs: <https://tunarr.com/>
- Tunarr repository: <https://github.com/chrisbenincasa/tunarr>
- Tunarr 2026.11.2 release notes: <https://tunarr.com/blog/2026.11.2/>
- HDHomeRun HTTP API: <http://info.hdhomerun.com/info/http_api>
- Plex Live TV & DVR: <https://support.plex.tv/articles/225877347-live-tv-dvr/>
- Jellyfin Live TV setup: <https://jellyfin.org/docs/general/server/live-tv/setup-guide>
- HLS RFC 8216 and Apple HLS authoring specification: <https://datatracker.ietf.org/doc/html/rfc8216>
