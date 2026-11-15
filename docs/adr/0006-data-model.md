# ADR 0006 — Data model and public contracts

Status: accepted
Date: 2026-11-14

## Context

The product must store a large media catalog, versioned schedule plans, materialized timelines, playback history, and operational state. The public API and playout format must be stable enough for import/export and third-party integration.

## Decision

- **Storage:** SQLite default (WAL, FTS5), PostgreSQL optional. `sqlx` migrations; pre-migration snapshots.
- **Core entities:** sources, libraries, media items, streams, chapters, artwork, people/credits, genres/tags, external IDs, collections, playlists, plans, timeline versions, timeline items, history, iterator state, channels, profiles, sessions, jobs, users, tokens, settings, audit log.
- **Media identity:** source + stable path + size + mtime + optional fingerprint; missing/moved items must not break collection or plan references.
- **Plans:** versioned JSON documents validated by JSON Schema, with typed Rust models. Do not store arbitrary SQL or executable code in plans.
- **Timelines:** immutable, versioned, absolute UTC timestamps. Published versions are never mutated.
- **Public contracts:** OpenAPI 3.1 for the management API; a documented JSON `Timeline`/`Playout` format for export/import. Optional import of ErsatzTV Next playout JSON for interoperability, implemented independently.
- **Search:** FTS5 in v1; no external search service. Smart collections store a query AST, not raw SQL.
- **History:** append-only per-channel playout history; prune/erase is explicit and audited.

## Consequences

- Schema migrations are a first-class release concern.
- The UI and API share generated types; the timeline format is testable without the engine.
- Import/export is decoupled from the database schema.
