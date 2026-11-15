# Awesome TV — product and technical plan

Status: decisions confirmed; Phase 0 spikes complete on x86_64 (ARM cross-check passed)
Working name: **Awesome TV**
Related: `docs/RESEARCH.md`, `docs/CLEAN-ROOM.md`, `docs/PHASE-0.md`

**Confirmed direction:** all-in-one product; deep ErsatzTV parity in public v1; Rust backend + React UI; our own streaming engine; local + Jellyfin + Plex + remote HTTP/RTSP/script sources; Plex, Jellyfin, Emby, Channels DVR, Kodi/TiviMate/generic IPTV clients; multi-arch x86 GPU and low-power/ARM; single admin in MVP, OIDC/roles in v1; MIT or Apache-2.0.

---

## 1. Product definition

**Awesome TV is a self-hosted personal broadcaster.** It turns a media library into scheduled, always-on TV channels and delivers them to the devices people already use: Plex, Jellyfin, Emby, Channels DVR, Kodi, TiviMate, VLC, and any IPTV client.

It is not a media server, downloader, metadata agent, or DVR. It consumes libraries and broadcasts them.

### One-sentence positioning

> Program a real station in an afternoon, trust it unattended, and tune in instantly.

### Who it is for

1. **The household programmer** — wants a few themed channels (cartoons, sitcoms, movies, music videos) that feel like TV, without learning a broadcasting system.
2. **The tinkerer** — wants dayparts, seasonal schedules, marathons, precise start times, filler, watermarks, and hardware control.
3. **The operator** — wants observability, backups, auth, and predictable resource use on a NAS, mini-PC, or home server.

### What success looks like

- A new user connects a library and watches a channel in under 15 minutes.
- A channel survives a bad file, a missing item, a media-server outage, and a restart without going offline.
- A viewer tunes in and sees video in under 3 seconds on a warm channel, under 8 seconds cold.
- A programmer can express "weekday cartoons 6–9am, sitcoms 6–9pm, movies after 9pm, holiday specials in December" without YAML.
- An operator can see what is playing, who is watching, how fast it is transcoding, and why an item failed.

---

## 2. Principles

1. **One obvious model, advanced escape hatches.** Start with channels, dayparts, shows, and breaks. Reveal rules, expressions, and scripted scheduling only when needed.
2. **Never go dark.** A channel always has something to play: scheduled item → fallback filler → slate → black/silence. Errors are visible but not fatal.
3. **Timeline is the source of truth.** Streaming, EPG, now/next, and history all read the same materialized timeline.
4. **Deterministic by default.** Given the same plan, state, seed, and time zone, the timeline is reproducible and testable.
5. **Transcode less.** Prefer copy/direct play and cached segments; transcode only what needs it.
6. **Observable and recoverable.** Every channel has health, metrics, diagnostics, and a safe reset. Every change is previewable and reversible.
7. **Interoperate, don't trap.** HLS/TS, M3U/XMLTV, HDHomeRun, OpenAPI, import/export, and an optional external playout/engine contract.
8. **Small core, clear boundaries.** The scheduler is a pure library. The engine is a worker. Sources are adapters. The UI is an API client.
9. **Secure by default.** Auth on by default, least privilege, scoped tokens, redacted logs, no telemetry.
10. **Sustainable scope.** A feature must earn its maintenance cost. No infinite configuration surface.

---

## 3. Scope

### 3.1 MVP (internal walking skeleton, not public v1)

**Goal:** prove the end-to-end shape with one household: local media → collection → channel → HLS/TS → Plex/Jellyfin/IPTV, with a scheduler and engine that can carry the deeper model.

| Area | MVP |
|---|---|
| Sources | Local folders; Jellyfin; Plex. |
| Media kinds | Movies, TV episodes, other videos. |
| Metadata | ffprobe + NFO + media-server metadata; artwork; subtitles; chapters. |
| Organization | Manual collections, smart collections (saved query), playlists. |
| Scheduling | **Lineup** (ordered/shuffled source, count/duration) + **Dayparts** (weekly time blocks) + filler. |
| Filler | Pre/post/tail/fallback, weighted lists, cooldowns, duration fitting. |
| Presentation | Channel number/name/group/logo; watermark; subtitle burn; per-channel transcode profile. |
| Playback | HLS (TS segments) + MPEG-TS gateway; copy mode; software/VAAPI/QSV/NVENC/RKMPP; fallback slate. |
| Clients | HDHomeRun emulation, M3U, XMLTV; Plex, Jellyfin, browser preview. |
| Ops | Docker (amd64/arm64), SQLite, single admin, backup/restore, logs, health, diagnostics, OpenAPI. |
| UI | Setup wizard, library browser, collections, channel studio with timeline + preview, guide, sessions, settings. |

**Explicit MVP non-goals:** songs, images, remote streams, scripted schedules, full block/deco model, graphics engine, multi-user/RBAC, OIDC, Postgres, 4K/HDR, Windows/macOS native packages, DVR, SSAI, native TV apps, plugin marketplace.

### 3.2 Public v1 — deep parity (confirmed)

The user chose **deep parity**, not a deliberately reduced Tunarr-like scope. Public v1 therefore targets the ErsatzTV programming surface, while keeping the new UX and reliability model.

**Scheduling parity**
- Classic schedules: ordered items, dynamic/fixed starts, strict/flexible/skip policies, playback orders, flood/one/multiple/duration, fill-with-group, tail mode, discard-to-fill, custom titles, guide modes.
- Block schedules: blocks, templates, decos, deco templates, playout templates, priority/date ranges, default filler, dead-air fallback, watermark control.
- Sequential YAML: content sources, sequences, reset instructions, all scheduling/control instructions, expressions.
- Scripted schedules and external playout JSON: a stable, documented contract (our own schema; optional import of ErsatzTV Next playout JSON for interop).
- Alternate/seasonal schedules with recurring date ranges; multi-part grouping; random start points; daily rebuild/reset.
- Filler: pre/mid/post/tail/fallback; count/duration/pad/random; chapters as media items; watermarks on filler; EPG grouping.
- Graphics engine: image, text, subtitle, motion, script elements; templates; timing; z-order; per-item/block/deco enablement.
- Watermarks: global/channel, multiple, permanent/intermittent/opacity expression, logo-as-watermark.
- Stream selection: default and custom rules; audio/subtitle language, title allow/block, flags, codecs, channels, content conditions; subtitle modes.
- On-demand and mirror channels; playout history, warnings, troubleshooting, preview.

**Media parity**
- Local: movies, shows, music videos, other videos, songs, images, remote streams.
- Plex, Jellyfin, Emby (Emby is a source too in public v1; it is not an MVP source).
- Remote HTTP/RTSP/script sources (confirmed for v1), live flags, duration, metadata, thumbnails.
- NFO + media-server metadata; path replacements; deep scans; trash; artwork; subtitles; chapters; HDR/DV detection.

**Presentation and delivery parity**
- Per-channel FFmpeg profiles; NVENC, QSV, VAAPI, AMF, VideoToolbox, RKMPP, Vulkan where supported; software fallback.
- H.264/HEVC/MPEG-2; copy mode; HDR/DV tonemapping; deinterlace; frame-rate normalization; 10-bit; anamorphic/rotation; 4:2:0.
- Audio: AAC/AC3/E-AC3/MP3/copy; channels/sample rate/bitrate; EBU R128 loudness.
- Subtitles: burn and WebVTT conversion; image/text; fonts; forced/default/SDH; external.
- HLS (TS/fMP4), MPEG-TS gateway, HDHomeRun, M3U, XMLTV; Plex, Jellyfin, Emby, Channels DVR, Kodi, TiviMate, generic IPTV.
- Multi-arch: linux/amd64 with GPU and linux/arm64/low-power (RKMPP/software); Windows/macOS binaries after v1.

**Product and operations parity**
- Channel Studio, library browser, search, collections/playlists, guide, sessions, diagnostics.
- Import from ErsatzTV/Tunarr/dizqueTV via public APIs/exports (not database internals).
- OIDC, multiple users/roles, scoped API tokens, audit log.
- SQLite default; optional PostgreSQL; Prometheus metrics; backup/restore; safe migrations.
- Single admin in MVP; OIDC/roles in public v1.

**Deep parity is a large v1.** It is a multi-phase effort, not a single release. The roadmap below sequences it so each phase leaves a usable product.

### 3.3 Later

- Native apps / 10-foot UI; mobile companion.
- Live sources (news, cameras, sports) with scheduling and failover beyond the remote-stream support.
- DVR/recording and catch-up (only if it does not compromise the core).
- SSAI/ad markers, SCTE-35, advanced broadcast graphics.
- Plugin SDK for sources, schedulers, engines, and overlays.
- Multi-node/remote playback relay; cloud edge caching.
- Optional external scheduler/engine contracts (compatibility, not dependency).

### 3.4 Non-goals (for now)

- Being a media server, downloader, or metadata agent.
- Downloading from streaming services or bypassing DRM.
- Monetization/ad network.
- Replacing Plex/Jellyfin/Emby as the library UI.
- Supporting every codec/client combination on day one.

---

## 4. Strategic options considered

| Option | Description | Pros | Cons | Recommendation |
|---|---|---|---|---|
| **A. All-in-one** | Sources + library + scheduler + engine + output + UI | Complete replacement; best UX; owns reliability | Largest scope; engine is hard | **Selected** |
| **B. Scheduler + UI, pluggable engines** | Build the product; use ErsatzTV Next or another engine | Faster; leverages a proven engine; clean boundary | Depends on Next's pace/schema; less differentiation in playback | Rejected |
| **C. Engine only** | Build a Next-class engine and let others schedule | Focused; reusable; lower support surface | Not a replacement for ErsatzTV; smaller audience | Rejected |
| **D. Scheduler only** | Build scheduling on top of Next | Fast; complements Next/Tunarr | Tunarr is already doing this; no engine differentiation | Rejected |

**Decision:** Build **A** with our **own engine**. The engine stays behind a narrow internal interface for testability and future flexibility, but there is no dependency on ErsatzTV Next. We may still import/export a documented playout format for interoperability, implemented independently.

---

## 5. Architecture

### 5.1 System context

```
 Media sources                Awesome TV                         Clients
┌──────────────┐      ┌──────────────────────────────┐      ┌──────────────┐
│ Local files  │─────▶│ Catalog / metadata           │      │ Plex         │
│ Plex         │      │ Collections / search         │      │ Jellyfin     │
│ Jellyfin     │      │ Scheduler / timeline         │─────▶│ Emby         │
│ Emby         │      │ Channel engine / cache       │      │ Channels DVR │
│ Remote/live  │      │ HLS / MPEG-TS / EPG / API    │      │ Kodi/TiviMate│
└──────────────┘      └──────────────────────────────┘      │ Browser      │
                                                             └──────────────┘
```

### 5.2 Modules

| Module | Responsibility | Boundary |
|---|---|---|
| `catalog` | Sources, scanning, probing, metadata, artwork, subtitles, search index | Adapters per source; normalized `MediaItem` |
| `library` | Collections, smart queries, playlists, tags/genres/people | Pure domain over catalog |
| `scheduler` | Plans, rules, cursors, timeline materialization, EPG grouping | **Pure library**, no I/O except injected clock/IDs |
| `playout` | Timeline persistence, history, now/next, guide projection | DB + scheduler |
| `engine` | Segment production, cache, manifest generation, TS gateway, fallback | Per-channel worker; FFmpeg sidecar |
| `streaming` | HTTP endpoints, sessions, heartbeats, tuner allocation, auth | API/engine boundary |
| `api` | REST/OpenAPI, SSE/WebSocket events, validation, idempotency | Public contract |
| `web` | React UI, embedded static assets | API client only |
| `ops` | Config, migrations, backup/restore, jobs, metrics, logs, diagnostics | Cross-cutting |

### 5.3 Confirmed stack

**Backend: Rust (stable, current toolchain)**

- One workspace, multiple crates, single deployable binary with embedded web assets. Rust is the team's choice and fits an engine-heavy, long-running appliance.
- HTTP/API: `axum` + `tower` + `hyper`; `serde` for JSON; `utoipa` or `aide` for OpenAPI; `tracing` + `tracing-subscriber` for structured logs; `metrics`/`prometheus` for metrics.
- Async/runtime: `tokio`; process supervision for FFmpeg with `tokio::process`, timeouts, kill trees, and backpressure.
- Database: `sqlx` (compile-time checked queries, SQLite and PostgreSQL) + `sqlx::migrate`; WAL and FTS5 for SQLite.
- Validation: `serde` + `schemars` for versioned JSON schemas; `validator` for input; `ulid`/`uuid` for IDs.
- Testing: `cargo test`, `proptest`, `insta` snapshots, `wiremock` for source APIs, `testcontainers` for PostgreSQL if needed.
- Frontend: React 19 + TypeScript + Vite; TanStack Router/Query; Tailwind + shadcn/ui; dnd-kit for timeline; HLS.js; Recharts.
- Watch-outs: compile times and contributor pool. Mitigate with a small crate graph, `cargo nextest`, incremental CI, and a clear contributor guide. Keep the scheduler and engine independently testable so UI/API work can proceed in parallel.
- Why not Go/TypeScript now: the decision is Rust-first. Go remains a viable fallback if the team changes; TypeScript/NestJS is rejected as the primary backend for runtime/packaging reasons.

**Database:** SQLite (WAL, FTS5) default; PostgreSQL optional in public v1. Versioned migrations, pre-migration snapshot, backup/restore. JSON columns for versioned rule trees; normalized tables for queryable entities. No MySQL or external search service in v1.

**Packaging:** Docker multi-arch (linux/amd64, linux/arm64); single binary with embedded UI; systemd; Windows/macOS binaries after v1. Multi-arch is a v1 requirement, including low-power/ARM.

**FFmpeg:** external `ffmpeg`/`ffprobe`; capability probe at startup; prefer an LGPL build we can bundle or a user-supplied build; document hardware requirements. Do not require a fork-specific build for core functionality; add optional patches only if a documented bug demands it. Support VAAPI/QSV/NVENC/AMF/VideoToolbox/RKMPP/Vulkan where available, with software fallback.

### 5.4 Repository layout

```
awesome-tv/
  Cargo.toml                  # workspace
  crates/
    awesometv-server/         # binary: config, wiring, HTTP server
    awesometv-api/            # REST/OpenAPI, SSE/WebSocket
    awesometv-auth/           # sessions, OIDC, tokens, RBAC
    awesometv-catalog/        # media items, probing, search
    awesometv-sources/        # local, plex, jellyfin, emby, remote
    awesometv-library/        # collections, playlists, queries
    awesometv-scheduler/      # pure timeline builder
    awesometv-playout/        # persistence, history, EPG projection
    awesometv-engine/         # channel worker, ffmpeg, cache, manifests
    awesometv-streaming/      # HLS/TS/HDHR, sessions, tuners
    awesometv-jobs/           # scans, probes, backups, pre-seed
    awesometv-config/         # env/YAML, validation
    awesometv-db/             # schema, migrations, queries
    awesometv-observability/  # logs, metrics, diagnostics
  web/                        # React app
  docs/                       # this plan, research, ADRs
  deploy/                     # Docker, compose, systemd
  testdata/                   # synthetic media fixtures
```

### 5.5 Core data model (initial)

```
source(id, kind, name, config_json, status)
library(id, source_id, kind, name, paths_json, sync_enabled, last_scan_at)
media_item(id, library_id, kind, title, sort_title, year, season, episode,
           artist, album, duration_ms, release_date, added_at, content_rating,
           plot, studio, path, size_bytes, mtime, fingerprint, probe_json,
           status, metadata_json)
media_stream(id, media_item_id, kind, stream_index, codec, profile, language,
             title, channels, width, height, pix_fmt, frame_rate, bit_depth,
             hdr_kind, is_default, is_forced, external_path)
chapter(id, media_item_id, index, start_ms, end_ms, title)
artwork(id, media_item_id, kind, path/url, width, height, blurhash)
person(id, name), credit(media_item_id, person_id, role, character, order)
genre(id, name), media_genre(media_item_id, genre_id)
tag(id, name), media_tag(media_item_id, tag_id)
external_id(media_item_id, source, value)
collection(id, kind, name, query_json, created_at)
collection_item(collection_id, media_item_id, position, added_at)
playlist(id, name, group_id), playlist_item(playlist_id, position, source_json, order, play_all, show_in_epg)
schedule(id, kind, name, timezone, config_json, version)
channel(id, number, name, group, logo, enabled, timezone, stream_mode, profile_id,
        selector_id, watermark_json, playback_policy, schedule_id, epg_json)
playout(id, channel_id, version, horizon_start, horizon_end, seed, generated_at, status)
playout_item(id, playout_id, start, finish, kind, media_item_id, in_ms, out_ms,
             title, epg_group, filler_kind, graphics_json, source_json, status)
playout_history(id, channel_id, playout_item_id, aired_at, session_id, result)
iterator_state(id, scope_type, scope_id, position, bag_json, updated_at)
session(id, channel_id, client, started_at, last_seen_at, bytes, transcode_speed, errors)
job(id, kind, status, progress, started_at, finished_at, error)
user(id, username, password_hash, role, created_at)
api_token(id, user_id, name, hash, scopes, last_used_at, expires_at)
setting(key, value_json)
audit_log(id, user_id, action, target, detail_json, created_at)
```

Design notes:

- **Media identity**: source ID + stable path + size + mtime + optional content hash; handle moves, renames, and missing files without losing collection/schedule references.
- **Kind-specific metadata**: use a typed core plus `metadata_json` for source-specific fields; don't build one giant sparse table.
- **Rule trees**: store schedule config as a versioned JSON document validated by JSON Schema; store materialized timeline separately. This keeps edits cheap and timelines immutable.
- **Search**: FTS5 table over title, plot, tags, genres, people, studio, external IDs; filters in SQL. Smart collections store a query AST, not raw SQL.
- **History**: retain per-channel playout history for no-repeat/cooldown; allow prune/erase; never use history as the only source of truth for the current timeline.

---

## 6. Scheduler design

### 6.1 Mental model

```
Channel
  └─ Plan (weekly repeating)
       └─ Daypart (days + time range + priority + date range)
            └─ Slot (source + ordering + fill mode + filler + breaks + EPG)
                 └─ Candidates (media items, resolved from collections/queries)
```

This is intentionally smaller than ErsatzTV's classic/block/sequential/scripted model. Advanced modes are represented as additional slot types or plan nodes, not separate subsystems.

### 6.2 Timeline contract

```go
type Item struct {
    ID          string
    Start, End  time.Time
    Content     ContentRef     // media item or generated segment
    In, Out     time.Duration  // trim within source
    Parts       []Part         // mid-roll segments
    EPG         EPGInfo
    Graphics    []Graphic
    Filler      FillerKind
    Source      SourceRef      // dynamic/live override
}

type BuildResult struct {
    Items    []Item
    State    StateDelta
    Warnings []Warning
    Coverage Coverage
}

func Build(plan Plan, state State, from, to time.Time) (BuildResult, error)
```

- **Pure**: no wall-clock reads, no random source, no DB. Inject `Clock`, `IDs`, `Resolver`, and `Seed`.
- **Deterministic**: same inputs → same output. Random choices derive from a versioned PRNG seeded by `channel_id + plan_version + cycle`.
- **Immutable**: a published timeline version is never mutated. Edits create a new version and publish at a safe boundary.
- **Bounded**: the builder has a max item count and max iterations; it always returns, with warnings if coverage is incomplete.

### 6.3 Resolution and ordering

- A **source** resolves to an ordered candidate list: show/season/collection/playlist/smart query.
- **Ordering**: chronological, season/episode, shuffle-without-replacement (bag), random-with-repeats, shuffle-in-order, custom.
- **Cursor**: per slot (or linked group), persisted. Supports:
  - `continue`: next item after the last played.
  - `rerun`: same item until every linked slot has played it.
  - `reset`: restart at a date/item/season/episode.
- **No-repeat**: a bag of unplayed IDs; refill only when exhausted; new items join the bag; deleted items drop out.
- **Cooldown**: per item/collection to avoid immediate repeats.
- **Multi-part**: group by show + part metadata or explicit grouping; keep together and in order when configured.
- **Filters**: season include/exclude, duration, rating, genre, tag, library, added/released windows.

### 6.4 Time and boundaries

- Store all timestamps in UTC; schedule in the channel's IANA time zone.
- Dayparts align to local wall clock. DST transitions are explicit: a 2:00–3:00 block on a spring-forward day is 0 minutes; on fall-back it is 2 hours. Tests cover both.
- **Fixed starts** use an overflow policy:
  - `strict`: the item starts at the exact time; if the previous item overruns, insert a gap/fallback or cut? (Never cut content silently. Default: let previous finish, then start late if within tolerance, else skip to next fixed boundary.)
  - `flexible`: start immediately after the previous item if the wait would exceed a tolerance.
  - `skip`: skip the fixed item if it cannot start within tolerance.
- **Duration slots** fit complete items; tail behavior is explicit: advance, fill, or slate.
- **Flood** repeats until the next boundary.
- **Count** plays N items.
- Always make progress: if an item cannot fit, discard and retry up to a limit, then warn and fill.
- **No infinite loops**: detect impossible constraints and stop with a warning.

### 6.5 Filler

- Kinds: pre, post, tail, fallback, mid (v1).
- Modes: count, duration, pad-to-interval, random count.
- Selection: weighted lists, per-list cooldown, per-item cooldown, fits-remaining-duration, longest-wait weighting.
- Trimming: cut the last item to fit only when configured; otherwise try another.
- Lazy mid-roll: resolve at playback time to use fresh cooldown state; record the actual selection in history.
- Filler can be hidden from EPG; the surrounding program absorbs its time.

### 6.6 EPG projection

- EPG is a projection of the timeline, not a separate scheduler.
- Item fields: title, subtitle, description, categories, episode number, icon, rating, credits, date, custom title, group.
- Filler can be hidden; grouped items can share one entry.
- Horizon: guide builds at least as far as the timeline; default 48–72 hours; configurable.
- XMLTV output is cached, gzip-capable, and consistent with now/next and the channel timeline.

### 6.7 State and history

- `iterator_state` is the durable cursor/bag per slot/group.
- `playout_history` records what actually aired, including lazy filler and dynamic items.
- Rebuilds replay state from the last published boundary; they do not mutate history.
- **Reset** is explicit: reset cursor, clear history, rebuild. Always preview the effect first.
- **On-demand channels** advance only while watched; checkpoints are persisted per channel (and optionally per user later).

### 6.8 Testing the scheduler

- Property tests: monotonic, no overlaps, no unintended gaps, bounded, deterministic, EPG matches items, filler fits.
- Golden timelines: fixed clock, time zone, DST, seed, source fixture.
- Scenario tests: long movie in a short slot, missing item, empty collection, fixed start overrun, midnight rollover, leap day, seasonal range.
- Simulation UI: run a week/month and show coverage/warnings before publishing.

---

## 7. Playback engine design

### 7.1 Goals

- Reliable, seamless-ish transitions between heterogeneous sources.
- Fast tune-in; no channel-wide failure because one item is bad.
- Efficient: copy/direct when possible, cache when repeated.
- Observable: per-item command, logs, speed, buffer, errors.
- Portable: Linux/Windows/macOS; hardware probe and software fallback.

### 7.2 Channel runtime

```
Playlist HTTP ─┐
Segment HTTP  ─┼─▶ Session manager ─▶ Channel runtime ─▶ Segment cache
TS HTTP      ─┘                         │                  │
                                        ├─▶ Planner (timeline now/next)
                                        ├─▶ Producer (ffmpeg per item)
                                        ├─▶ Publisher (manifest/segments)
                                        └─▶ Fallback producer
```

- One runtime per active channel, started on demand or kept warm.
- Session manager tracks playlist/segment/TS requests via heartbeat; idle timeout stops the runtime.
- Tuner count limits concurrent TS streams; return `503` when exhausted.
- Runtime exposes now/next, health, speed, bytes, errors, and a diagnostic snapshot.

### 7.3 Segment production

Per item (or cached artifact):

1. Resolve item and offset for `now`.
2. Compute cache key: `media_id + in/out + profile_hash + graphics_hash + subtitle_hash + copy_mode`.
3. If cache hit and valid, reuse segments.
4. Otherwise run FFmpeg to produce a short segment run:
   - Target 2-second GOP, 4-second HLS segments.
   - Normalize video/audio to the channel profile unless copy mode applies.
   - Burn subtitles or produce WebVTT (v1).
   - Composite watermark/graphics.
   - Align output timestamps to the item's wall-clock position using `-output_ts_offset` / `-copyts`.
   - Force keyframes at segment boundaries.
   - Write to a staging directory; atomically publish.
5. Publisher appends to the channel manifest with `EXT-X-PROGRAM-DATE-TIME`, `EXT-X-DISCONTINUITY` when encoding parameters change, and a monotonically increasing media sequence.
6. Prefetch the next item 30–60 seconds before the boundary; never work ahead more than the configured window.

**Transition strategy:** prefer per-item processes for fault isolation. If two adjacent items share identical target parameters and are cache hits, concatenate segments without a discontinuity; otherwise insert a discontinuity and accept a brief client reset. Test with Plex/Jellyfin/VLC/hls.js. A long-running concat pipeline is a fallback optimization, not the default.

**Copy mode:** if the source already matches target codec/resolution/frame rate/audio and no graphics/subtitles are required, remux rather than encode. Per-item decision, not per-channel. Copy mode is a profile option, not a separate engine.

### 7.4 Startup and startup latency

- Produce `ready_segment_count` (default 3) before signaling ready.
- Serve the live edge; clients start 1–3 segments back.
- Warm the next item's first segments while the current item plays.
- For MPEG-TS, send the first ~6 seconds immediately, then pace to real time.
- Cache the first segments of frequent items (station IDs, bumpers, fallback).

### 7.5 Failure handling

- Per-item failure → fallback (filler/slate/error card/black+silence) for that item's remaining time; channel continues.
- Retry with backoff; circuit-break repeated failures.
- Missing file → mark missing, skip, warn; do not fail the timeline.
- Media server unreachable → use cached/local path if available; otherwise fallback and retry.
- Engine crash → supervisor restarts; timeline is unaffected.
- Disk full → stop cache writes, keep streaming; alert.
- Every failure writes a diagnostic bundle: item, resolved command, ffmpeg log, probe, profile, timeline context, error classification.

### 7.6 Output modes

| Mode | Use | Notes |
|---|---|---|
| HLS (TS segments) | Default for browser, Jellyfin, VLC, IPTV | Broad compatibility; h264/aac safest |
| HLS (fMP4) | HEVC/AC3, modern clients | Less universal; opt-in |
| MPEG-TS gateway | Plex/HDHomeRun clients | Remux HLS segments to TS; initial burst; tuner count |
| HLS Direct | Low-power/direct play | Single-item playlist; client must handle transitions; advanced option |
| MPEG-TS direct | Legacy clients | Not default; higher risk |

### 7.7 Capacity and scheduling

- Global limits: max concurrent transcodes, per-GPU limits, per-channel priority.
- Probe hardware capabilities and session limits; warn when oversubscribed.
- Queue tune-ins; never silently degrade an active channel.
- Metrics: active sessions, transcode speed vs realtime, segment buffer depth, cache hit rate, GPU/CPU, errors.
- On-demand default; optional always-on for instant tuning at higher cost.

---

## 8. API and UI

### 8.1 API

- REST/JSON under `/api/v1`; OpenAPI 3.1 generated from code; typed client for the UI.
- Resources: sources, libraries, media, collections, playlists, schedules, channels, timelines, guide, sessions, jobs, settings, users, tokens, system.
- Cursor pagination; ETags; idempotency keys for mutations; structured error codes; rate limits.
- SSE for job progress, scan progress, now/next, logs; WebSocket optional.
- Public streaming endpoints: `/iptv/channels.m3u`, `/iptv/xmltv.xml`, `/channel/{n}.m3u8`, `/channel/{n}.ts`, HDHR endpoints.
- Auth: session cookie (UI), bearer API token, streaming token; scopes (`read`, `manage`, `stream`); OIDC in v1.
- Webhooks/events for external automation (v1).

### 8.2 UI

**Setup wizard**
1. Create admin.
2. Connect a source (local path / Plex / Jellyfin).
3. Scan and review library.
4. Pick a channel template.
5. Preview and tune in.
6. Copy M3U/XMLTV/HDHR URLs into Plex/Jellyfin.

**Channel Studio**
- Left: channels.
- Center: day/week timeline with dayparts and items; drag/drop; zoom; coverage meter.
- Right: inspector for the selected slot/daypart (source, order, fill mode, filler, breaks, EPG, graphics).
- Top: now/next, live preview, health, viewer count, transcode speed.
- Bottom: warnings ("3 items are longer than this slot", "collection is empty", "item missing"), with fixes.
- Actions: preview 24h/7d, simulate date/time, publish, revert, duplicate channel, export.

**Other pages**
- Library browser with faceted search, bulk add to collection/playlist.
- Collections/playlists with drag ordering.
- Guide (channel × time) with now line.
- Sessions (who is watching, bitrate, speed, stop).
- Settings: sources, profiles/hardware, stream selection, auth/users, backups, logs, diagnostics.
- Import from ErsatzTV/Tunarr/dizqueTV; export channel/plan JSON.

**UX rules**
- No jargon in the default path: "channel", "daypart", "show", "break", "filler".
- Every destructive action is undoable or confirmed with a diff.
- Validate at edit time, not at playback time.
- Show the consequence: "This change will air at the next boundary (21:00)".
- Accessible: keyboard timeline, focus rings, reduced motion, contrast.

---

## 9. Security and operations

### 9.1 Security

- First-run admin creation; Argon2id; secure, HttpOnly, SameSite cookies; CSRF protection; login rate limiting.
- Roles: admin, editor, viewer. Per-channel visibility later.
- OIDC (v1); local auth always available as fallback.
- API tokens hashed at rest, scoped, expiring, revocable; display once.
- Streaming tokens signed, short-lived, optionally bound to channel/IP; never log tokens.
- Secrets encrypted at rest; redact logs and diagnostics.
- SSRF protection for remote streams/artwork; allowlist/denylist; block link-local/metadata IPs by default.
- Path traversal protection; media roots only; no arbitrary file read.
- Script/command sources disabled by default; explicit allowlist; run with least privilege; environment scrubbing.
- CORS same-origin by default; reverse-proxy trust configured explicitly.
- Audit log for auth, settings, channel changes, backups.
- No telemetry by default; opt-in crash reports with redaction.

### 9.2 Operations

- Docker multi-arch; compose example; healthcheck; read-only root where possible; non-root user.
- Config via env/flags/YAML; data dir; clear paths; validation with helpful errors.
- Migrations: pre-migration snapshot; refuse downgrades; backup before upgrade.
- Backup: DB + config + artwork metadata + channel plans; scheduled; retention; built-in restore (staged, verified).
- Export/import: channel plans, schedules, collections as JSON; media paths are environment-specific.
- Logs: structured, level per module, ring buffer in UI, file optional, no secrets.
- Metrics: `/metrics` Prometheus; health `/healthz`, `/readyz`; pprof opt-in.
- Diagnostics: one-click bundle (version, config redacted, hardware probe, recent logs, channel timeline, ffmpeg commands); local download only by default.
- Update check: opt-in, no auto-update.
- Resource limits: cache quota, temp cleanup, max sessions, max upload size.

---

## 10. Testing and verification

| Layer | What |
|---|---|
| Unit | Scheduler invariants, ordering, filler fitting, time zones/DST, XMLTV escaping, auth, path safety. |
| Property-based | Timeline never overlaps, always progresses, deterministic, coverage matches, no infinite loops. |
| Golden | Fixed clock/seed/TZ timelines for classic scenarios; manifest snapshots. |
| Media fixtures | Synthetic `testsrc`/`sine`, multiple codecs/resolutions/frame rates, interlaced, HDR, subtitles (SRT/ASS/PGS), chapters, missing audio. |
| Engine integration | Run FFmpeg, fetch HLS, ffprobe segments, validate durations/PDT/discontinuity; kill/restart; disk full; missing file. |
| Client matrix | Plex, Jellyfin, Emby, VLC, mpv, hls.js, Kodi/TiviMate where feasible; nightly/self-hosted runner. |
| HDHR | `discover.json`, `lineup.json`, `lineup_status.json`, `/auto/v{n}`, tuner limits, 503 behavior. |
| API | Contract tests from OpenAPI; auth/scopes; idempotency; pagination. |
| UI | Playwright: setup, create channel, preview, edit daypart, guide, backup/restore. |
| Load | N channels × M viewers; CPU/GPU saturation; cache hit rates; startup latency. |
| Security | Dependency scanning, secret scanning, SSRF/path/command injection tests, auth bypass. |
| Migration | Upgrade from each schema version; backup/restore round-trip. |

**Definition of done for a channel feature:** it works from the UI and API, has a timeline test, an engine test if it affects playback, appears correctly in XMLTV, and is documented.

---

## 11. Roadmap

Deep parity is a substantial v1. Estimates assume a small team (2–4 engineers) or one strong full-time engineer with focused scope; re-estimate after Phase 0. A realistic public-v1 window is **9–15 months**, not a single quarter.

### Phase 0 — Foundations and spikes (3–4 weeks)

- Lock the parity checklist, license, name, target ARM devices, migration artifacts.
- Write ADRs: scheduler model, engine model, data model, API/auth, clean-room policy.
- Build synthetic media fixtures and a test harness.
- Spike: Rust HLS segment pipeline with 3 heterogeneous clips; measure startup, transitions, CPU/GPU.
- Spike: HDHomeRun + Plex/Jellyfin/Emby/Channels tune-in against a fake channel.
- Spike: scheduler timeline builder for one week with dayparts/filler; property tests.
- **Exit:** spikes pass on Linux + one GPU + one ARM device; architecture accepted.

### Phase 1 — Walking skeleton (6–8 weeks)

- Rust workspace, SQLite schema/migrations, config, logging, health.
- Local movies/shows; scan/probe; manual collections; simple playlist/lineup.
- One channel; sequential timeline; HLS + M3U + XMLTV; browser preview.
- Docker (amd64/arm64); basic single-admin auth.
- **Exit:** a local folder plays as a 24/7 channel in VLC/hls.js; guide matches; restart resumes.

### Phase 2 — Beta: usable product (8–12 weeks)

- Plex and Jellyfin sources; path mapping; artwork; subtitles; chapters.
- Dayparts + filler + watermark + per-channel profile.
- MPEG-TS gateway + HDHomeRun; Plex/Jellyfin tune-in.
- Channel Studio (timeline, now/next, warnings, preview).
- Backup/restore, diagnostics, metrics, API tokens.
- Hardware: VAAPI/QSV/NVENC/RKMPP with probe/fallback.
- **Exit:** 3 channels run for 7 days unattended; Plex and Jellyfin play; no channel-offline incidents; backup restores.

### Phase 3 — Deep parity, part A: programming (10–14 weeks)

- Classic schedules: all start/ordering/mode/filler/EPG options.
- Block schedules: blocks, templates, decos, deco templates, playout templates, alternates.
- Sequential YAML and scripted schedules; external playout JSON.
- Mid-roll/chapters, multi-part grouping, random start, daily rebuild/reset.
- Stream selection rules; graphics engine; watermarks.
- **Exit:** the scheduling parity checklist passes; timeline tests cover the edge cases.

### Phase 4 — Deep parity, part B: media, delivery, operations (8–12 weeks)

- Music videos, songs, images, remote HTTP/RTSP/script sources; Emby source.
- Subtitle burn/WebVTT; HDR/DV tonemap; deinterlace; frame-rate normalization.
- Cache modes (upcoming pre-segment; optional pre-seed); copy-mode hardening.
- Import from ErsatzTV/Tunarr/dizqueTV; export.
- OIDC, roles, audit log; PostgreSQL; Prometheus dashboards.
- Client hardening: Emby, Channels DVR, Kodi, TiviMate, generic IPTV.
- **Exit:** public v1; feature parity on the chosen matrix; 10+ channels; documented capacity; migration path for common setups.

### Phase 5 — Scale and polish (ongoing)

- 4K/HDR, native apps/10-foot UI, live sources beyond remote streams, DVR/catch-up, SSAI, plugin SDK.
- Performance, reliability, documentation, community.

---

## 12. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Scope creep repeats ErsatzTV's history | Project stalls | Phased parity gates; ship a usable beta before deep parity; modular boundaries; say no |
| Rust compile times / contributor pool | Slower iteration, fewer contributors | Small crate graph; `cargo nextest`; incremental CI; strong contributor guide; keep scheduler/engine independently testable |
| FFmpeg/hardware matrix | Playback failures, support load | Capability probe; conservative defaults; software fallback; diagnostics; CI hardware |
| Client compatibility (Plex/Jellyfin) | "It doesn't work" | TS gateway for Plex; HLS for browser; test matrix; known-issue docs |
| Scheduler edge cases | Wrong programming, lost trust | Pure builder; invariants; simulation; golden tests; guardrails |
| Cache/pre-segment complexity | Disk/CPU cost, stale segments | Quotas; LRU; invalidation; off by default; metrics |
| Transcode capacity | Stutter/offline | On-demand; limits; queue; backpressure; capacity calculator |
| Clean-room/legal | Rework, licensing risk | Policy, provenance log, no code copying, license review |
| Security (remote streams/scripts) | Host compromise | Disabled by default; allowlists; sandbox; SSRF/path protections |
| Data loss/migration | User trust | Backups, pre-migration snapshots, restore verification |
| Maintenance burden | Burnout | Small core; automated tests; docs; contributor guide; no exotic deps |
| Name/trademark conflict | Rebrand | Trademark search before public launch |

---

## 13. Confirmed decisions

Locked in November 2026:

| Decision | Choice |
|---|---|
| Product scope | **All-in-one** |
| v1 depth | **Deep parity with ErsatzTV** (phased; public v1 is a multi-phase effort) |
| Backend | **Rust** |
| Frontend | **React + TypeScript** |
| Engine | **Own engine only**; no ErsatzTV Next dependency |
| v1 sources | **Local, Jellyfin, Plex, remote HTTP/RTSP/script** |
| v1 clients | **Plex, Jellyfin, Emby, Channels DVR, Kodi/TiviMate/generic IPTV** |
| Deployment | **Multi-arch: x86 with GPU and low-power/ARM** |
| Auth | **Single admin in MVP; OIDC + roles in public v1** |
| License | **MIT or Apache-2.0** (choose before first public release) |
| Name | **Awesome TV** (working name; trademark search before launch) |

## 14. Still open

These do not block Phase 0, but should be decided before Phase 2:

1. **Team and time**: who is building, how many hours per week, and what is the first demo target?
2. **Exact v1 parity checklist**: the plan lists the full surface, but we should mark each item must-have / nice-to-have / later.
3. **Migration**: which ErsatzTV/Tunarr artifacts must import cleanly (channels, collections, schedules, playouts, settings, history)?
4. **Low-power target**: which ARM devices/chips are representative (Raspberry Pi 4/5, RK3588, ARM NAS)?
5. **License choice**: MIT vs Apache-2.0 (Apache adds patent grant; MIT is simpler).
6. **Name**: keep "Awesome TV" or choose a launch name.

---

## 15. Immediate next steps

1. Write `docs/adr/0001-scope.md`, `0002-stack.md`, `0003-scheduler.md`, `0004-engine.md`, `0005-clean-room.md`.
2. Create synthetic fixtures and the HLS/HDHR spike in Rust.
3. Define the JSON schemas for `ChannelPlan`, `Timeline`, and `PlaybackProfile` before code.
4. Build the scheduler as a standalone Rust crate with tests, before UI/DB.
5. Build the channel engine spike against the timeline contract.
6. Build the walking skeleton end-to-end.
7. Only then add UI polish and integrations.

---

## Appendix A — Feature parity matrix (planning view)

| Capability | ErsatzTV Legacy | Tunarr | ErsatzTV Next | Awesome MVP | Awesome v1 |
|---|---|---|---|---|---|
| Local media | Yes (all kinds) | Yes (movies/shows/other/music) | No | Movies/shows/other | All kinds |
| Plex | Yes | Yes | No | Yes | Yes |
| Jellyfin | Yes | Yes | No | Yes | Yes |
| Emby source | Yes | Yes | No | No | Yes |
| Remote streams | Yes | No | Yes (engine) | No | Yes |
| Smart collections | Yes | Yes | No | Yes | Yes |
| Playlists | Yes | Yes | No | Yes | Yes |
| Classic sequencing | Yes | Partial | No | Lineup | Full |
| Dayparts/blocks | Yes (deep) | Time slots | No | Dayparts | Full |
| Mid-roll | Chapters | Yes | Graphics only | No | Yes |
| Sequential/scripted | Yes | No | No | No | Yes |
| Filler | Deep | Yes | No | Yes | Yes |
| Watermark | Yes | Yes | Yes | Yes | Yes |
| Graphics engine | Yes | No | Overlays | No | Yes |
| Music videos | Yes | Partial | No | No | Yes |
| Songs/images | Yes | Partial | No | No | Yes |
| Stream selection | Yes | Yes | Per-item tracks | Basic | Yes |
| Subtitles burn/convert | Yes | Yes | Yes | Burn | Both |
| Hardware accel | 7 backends | 4 backends | 7 backends | 4 backends | 7 |
| HDR/DV tonemap | Yes | Experimental | Yes | No | Yes |
| HLS/TS/HDHR | Yes | Yes | HLS only | Yes | Yes |
| M3U/XMLTV | Yes | Yes | Yes | Yes | Yes |
| Clients | Broad | Plex/Jellyfin/Emby | Generic | Plex/Jellyfin | Plex/Jellyfin/Emby/Channels/Kodi/TiviMate |
| OIDC/multi-user | OIDC only | Partial | No | No | Yes |
| Backup/restore | No | Backup only | No | Yes | Yes |
| Metrics | No | No | No | Yes | Yes |
| Import from ErsatzTV | — | Partial | Playout sync | No | Yes |

## Appendix B — Glossary

- **Channel**: a virtual TV station: number, name, plan, output profile.
- **Daypart**: a recurring local-time window with programming.
- **Slot**: a unit of programming inside a daypart (source + ordering + fill).
- **Timeline / playout**: the materialized list of items with absolute start/finish.
- **Filler**: content used to pad or bridge.
- **Flex**: time intentionally left for filler at playback time.
- **Cursor / iterator**: the position within a source.
- **EPG**: electronic program guide (XMLTV).
- **HDHR**: HDHomeRun tuner emulation.
- **Copy mode**: remux instead of transcode when the source already matches the target.
- **Pre-seed**: pre-segmenting content before it airs to enable instant, cheap playback.
