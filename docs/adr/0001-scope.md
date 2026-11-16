# ADR 0001 — Product scope and v1 depth

Status: accepted
Date: 2026-11-14

## Context

ErsatzTV Legacy is frozen, ErsatzTV Next is an engine-only project, and Tunarr is the active all-in-one alternative but deliberately simpler. The approved product plan calls for a modern replacement that preserves ErsatzTV's programming depth.

## Decision

Build **ChannelLoom** as an **all-in-one** product:

- sources, catalog, library, scheduler, playout, streaming engine, API, UI, and operations;
- our **own streaming engine**, with no dependency on ErsatzTV Next;
- **deep ErsatzTV parity in public v1**, phased behind a usable beta;
- local, Jellyfin, Plex, and remote HTTP/RTSP/script sources;
- Plex, Jellyfin, Emby, Channels DVR, Kodi, TiviMate, and generic IPTV clients;
- multi-arch deployment: x86_64 with GPU and linux/arm64 low-power;
- single admin in beta; OIDC and roles in public v1;
- MIT or Apache-2.0 license (final choice before first public release).

## Consequences

- The project is large. Public v1 is a 9–15 month effort for 2–4 engineers, not a single quarter.
- The scheduler and engine must be independently testable so work can proceed in parallel.
- A usable beta must ship before full parity; parity is a checklist, not a big-bang release.
- Compatibility with ErsatzTV/Tunarr is an import/export feature, not an architectural dependency.
