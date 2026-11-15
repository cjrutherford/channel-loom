# Parity checklist — deep v1

Status: locked for planning
Date: 2026-11-14

Legend: **B** = usable beta, **V1** = public deep-parity release, **L** = later.
Every V1 item is a release gate unless explicitly moved to L by a later decision.

## Sources and metadata

- [ ] **B** Local libraries: movies, shows, other videos.
- [ ] **B** Jellyfin source (URL + API key, libraries, path replacements).
- [ ] **B** Plex source (PIN auth, servers, libraries, path replacements).
- [ ] **V1** Emby source.
- [ ] **V1** Local music videos, songs, images.
- [ ] **V1** Remote HTTP/HTTPS/RTSP/script sources with live flag and duration.
- [ ] **V1** Remote stream metadata (title, plot, year, rating, thumbnail).
- [ ] **B** NFO metadata + minimal fallback metadata.
- [ ] **V1** Media-server metadata, artwork, people, genres, tags, external IDs.
- [ ] **V1** Path replacements, deep scan, missing/trash handling, path moves.
- [ ] **V1** Subtitles: embedded, sidecar, text and image; language/flags/title.
- [ ] **V1** Chapters, HDR/DV detection, rotation, interlacing, aspect ratio.

## Library and discovery

- [ ] **B** Manual collections with custom order.
- [ ] **B** Smart collections (saved query).
- [ ] **B** Playlists.
- [ ] **V1** Multi collections, rerun collections, playlist groups.
- [ ] **V1** Trakt lists (auto-refresh, optional generated playlist).
- [ ] **V1** Rich search: title, genre, tag, collection, plot, studio, people, library, rating, language, subtitle language, dates, chapters, duration, resolution, codec, bit depth, dynamic range, season/episode, external IDs.
- [ ] **V1** Relative-date searches (`released_inthelast`, `added_inthelast`, `released_onthisday`, etc.).
- [ ] **V1** Bulk add to collection/playlist; drag ordering.

## Scheduling

- [ ] **B** Lineup: ordered/shuffled source, count/duration.
- [ ] **B** Weekly dayparts, priority, coverage warnings.
- [ ] **B** Filler: pre/post/tail/fallback, weighted lists, cooldowns, fitting.
- [ ] **V1** Classic schedules: dynamic/fixed starts, strict/flexible/skip, chronological/random/shuffle/shuffle-in-order/season-episode, flood/one/multiple/duration, multiple modes, fill-with-group, tail mode, discard-to-fill, custom title, guide mode.
- [ ] **V1** Block schedules: blocks, block items, templates, decos, deco templates, playout templates, date ranges, priority.
- [ ] **V1** Sequential YAML: content sources, sequences, reset, all scheduling/control instructions, expressions.
- [ ] **V1** Scripted schedules (external executable + management API).
- [ ] **V1** External JSON playouts; documented import/export.
- [ ] **V1** Alternate/seasonal schedules with recurring date ranges.
- [ ] **V1** Mid-roll breaks from chapters/intervals; pre/mid/post/tail filler; filler expressions.
- [ ] **V1** Multi-part episode grouping; random start point; daily rebuild/reset.
- [ ] **V1** EPG groups, custom titles, hidden filler, guide modes.
- [ ] **V1** On-demand channels and checkpoints; mirror channels.
- [ ] **V1** Playout history, warnings, build status, preview, troubleshooting.

## Presentation

- [ ] **B** Channel number/name/group/logo.
- [ ] **B** Single watermark (position, scale, opacity).
- [ ] **V1** Multiple watermarks; permanent/intermittent/opacity expression; logo-as-watermark.
- [ ] **V1** Graphics engine: image, text, subtitle, motion, script; templates; timing; z-order.
- [ ] **V1** Stream selection: default/custom; audio/subtitle language, title allow/block, flags, codec, channels, content conditions.
- [ ] **V1** Subtitle modes: burn, convert to WebVTT; fonts, force style, forced/default/any.
- [ ] **V1** Per-channel profile; global profile defaults; config overlays.

## Transcoding and playback

- [ ] **B** HLS (TS segments); MPEG-TS gateway; HDHomeRun emulation; M3U; XMLTV.
- [ ] **B** Software, VAAPI, QSV, NVENC; capability probe; fallback.
- [ ] **B** Copy mode when source matches.
- [ ] **V1** AMF, VideoToolbox, RKMPP, Vulkan where available.
- [ ] **V1** H.264, HEVC, MPEG-2; 8/10-bit; 4:2:0; anamorphic/rotation; deinterlace; frame-rate normalization.
- [ ] **V1** Audio AAC/AC3/E-AC3/MP3/copy; channels, sample rate, bitrate; EBU R128 loudness.
- [ ] **V1** HDR10/Dolby Vision tonemapping; hardware and software paths.
- [ ] **V1** Segment cache (upcoming pre-segment; optional pre-seed); invalidation and quotas.
- [ ] **V1** Fast tune-in: ready segments, prefetch, MPEG-TS initial burst.
- [ ] **V1** Per-item fallback, retry, circuit breaker, diagnostics bundle.
- [ ] **V1** HLS fMP4 optional; TS gateway; tuner limits; idle stop.

## Clients and interoperability

- [ ] **B** Plex and Jellyfin end-to-end (HDHR + XMLTV).
- [ ] **B** Browser preview (HLS.js).
- [ ] **V1** Emby, Channels DVR, Kodi, TiviMate, generic IPTV.
- [ ] **V1** M3U and XMLTV metadata parity (icons, categories, episode numbers, ratings, credits).
- [ ] **V1** HDHomeRun discovery/lineup/status/device/auto/tuner endpoints.
- [ ] **V1** Import from ErsatzTV/Tunarr/dizqueTV via public APIs/exports.
- [ ] **V1** Export channel plans and timeline JSON.

## Operations, security, API

- [ ] **B** Docker amd64/arm64; SQLite; config; structured logs; health.
- [ ] **B** Single admin auth; API tokens; backup/restore.
- [ ] **B** Metrics; diagnostics; OpenAPI; UI.
- [ ] **V1** OIDC; roles (admin/editor/viewer); audit log.
- [ ] **V1** PostgreSQL optional; safe migrations; pre-migration snapshot.
- [ ] **V1** Prometheus metrics/dashboards; capacity calculator.
- [ ] **V1** SSRF/path/command protections; secret redaction; streaming tokens.
- [ ] **V1** Backup/restore verification; import/export; update check (opt-in).

## Explicitly later

- [ ] **L** Native apps / 10-foot UI; mobile companion.
- [ ] **L** Live sources beyond remote streams (sports/news/cameras with scheduling/failover).
- [ ] **L** DVR/recording and catch-up.
- [ ] **L** SSAI/SCTE-35/advanced broadcast graphics.
- [ ] **L** Plugin SDK and marketplace.
- [ ] **L** Multi-node/remote relay; cloud edge caching.
