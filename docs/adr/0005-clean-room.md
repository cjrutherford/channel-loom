# ADR 0005 — Clean-room process

Status: accepted
Date: 2026-11-14

## Context

The project is a clean-room replacement for ErsatzTV. We must avoid copying source code, internal schemas, assets, or protected expression from ErsatzTV, ErsatzTV Next, Tunarr, dizqueTV, or their forks.

## Decision

Follow `docs/CLEAN-ROOM.md`:

- Use public documentation, public protocol specifications, release notes, issue trackers, and black-box user-facing behaviour.
- Do not copy or translate competitor source code, internal schemas, assets, UI text, or branding.
- Document provenance for competitor research in `docs/clean-room/provenance.md`.
- Import user data only through documented APIs or user exports; do not reverse-engineer internal databases.
- Implement compatibility contracts independently from their public specifications.
- Choose MIT or Apache-2.0 for our code; review FFmpeg and dependency licenses before distribution.
- Run a trademark search before public launch; "Awesome TV" is a working name.

## Consequences

- Some shortcuts are deliberately unavailable.
- Features take slightly longer, but the project has a defensible provenance.
- Public compatibility (M3U, XMLTV, HLS, HDHomeRun, Plex/Jellyfin/Emby APIs) remains allowed and encouraged.
