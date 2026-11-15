# Clean-room provenance log

Record what was learned from public sources and how it will be used. Do not paste competitor source code into this repository.

## 2026-11-14 — ErsatzTV product and feature research

Source: ErsatzTV public site and Legacy docs (`https://ersatztv.org/docs/`), Legacy README, changelog, and release notes.
Learned: feature inventory, scheduling models, media sources, streaming modes, graphics/watermarks, hardware backends, client setup, operational behaviour.
Use: requirements and parity checklist. No source code, internal schema, assets, or UI text copied.

## 2026-11-14 — ErsatzTV Next contract research

Source: ErsatzTV Next README and public JSON schemas (`playout.json`, `channel_config.json`, `lineup_config.json`) and docs.
Learned: engine-only scope, versioned playout/channel/lineup contracts, dynamic sources, graphics layers, fallback/session behaviour.
Use: engine requirements and optional import/export compatibility. Implemented independently; no Rust source copied.

## 2026-11-14 — Tunarr product research

Source: Tunarr docs, README, release notes, public issue tracker.
Learned: modern UX expectations, slot/time-slot scheduling, filler selection, stream selection profiles, backup, ErsatzTV Next backend.
Use: competitive analysis and product requirements. No source code or assets copied.

## 2026-11-14 — HDHomeRun HTTP API

Source: SiliconDust HDHomeRun HTTP development guide (`http://info.hdhomerun.com/info/http_api`).
Learned: `discover.json` fields, `lineup.json` shape, `/auto/` and `/tunerN/` stream URLs, MPEG-TS requirement, tuner count/503 behaviour.
Use: HDHomeRun spike and future tuner emulation. Public protocol only.

## 2026-11-14 — HLS authoring requirements

Source: RFC 8216 and Apple HLS authoring specification.
Learned: `EXT-X-PROGRAM-DATE-TIME`, `EXT-X-DISCONTINUITY`, `EXT-X-DISCONTINUITY-SEQUENCE`, live playlist minimums, target duration, independent segments.
Use: HLS playlist generation and validation.

## 2026-11-14 — Plex/Jellyfin Live TV docs

Source: Plex Live TV & DVR support pages; Jellyfin Live TV setup guide.
Learned: HDHomeRun tuner setup, XMLTV guide provider, MPEG-TS expectations, Plex Pass requirement.
Use: client compatibility checklist. No client code copied.
