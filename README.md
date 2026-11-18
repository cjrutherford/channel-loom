<img src="assets/icon.png" width="64" height="64" alt="ChannelLoom">

# ChannelLoom

A clean-room, modern replacement for [ErsatzTV](https://ersatztv.org/): turn a personal media library into live TV channels and stream them to Plex, Jellyfin, Emby, and any IPTV client.

**Status:** Phase 1 complete; Phase 2 largely complete; Phase 3/4 core features are running, including a public viewer, Cast, precache, and native wrappers. Deep-parity work is still in progress. See the [plan review](docs/PLAN-REVIEW.md).

**Direction:** all-in-one, deep ErsatzTV parity in v1, Rust backend, our own streaming engine, local/Jellyfin/Plex/remote sources, Plex/Jellyfin/Emby/Channels/Kodi/TiviMate clients, multi-arch x86 + ARM, MIT or Apache-2.0. The current UI is a single-file vanilla JS app; the plan's React/TypeScript stack is an open decision.

- [Status and remaining work](docs/STATUS.md)
- [Plan review: current state vs the original plan](docs/PLAN-REVIEW.md)
- [Phase 1 vertical slice](docs/PHASE-1.md)
- [Phase 0 results and spike commands](docs/PHASE-0.md)
- [HTML review report](docs/report.html)
- [Research: what ErsatzTV provides](docs/RESEARCH.md)
- [Product and technical plan](docs/PLAN.md)
- [Current execution plan](docs/EXECUTION-PLAN.md)
- [nginx reverse proxy](deploy/nginx/README.md)
- [Casting and branded receivers](docs/CAST.md)
- [Getting a Cast receiver App ID](docs/CAST-APP-ID.md)
- [Native and installable apps](docs/NATIVE.md)
- [Cloud sources](docs/CLOUD-SOURCES.md)
- [P2P channel sourcing](docs/P2P-CHANNELS.md)
- [Canonical media sources](docs/SOURCES.md)
- [Parity checklist](docs/PARITY.md)
- [ADRs](docs/adr/)
- [Brand guide](docs/BRAND.md)
- [Security and public exposure](docs/SECURITY.md)
- [Release process](docs/RELEASE.md)
- [End-to-end testing](docs/TESTING.md)
- [Clean-room policy](docs/CLEAN-ROOM.md)

## Serving

- `/` — **Watch**: the public live viewer with channel rail, player, and guide.
- `/admin` — **Studio**: the login-gated place to build channels and arrange what plays.
- `/api/v1/public/*` — read-only viewer APIs; all other `/api/v1/*` routes require auth.
- `/iptv/channels.m3u`, `/iptv/xmltv.xml`, `/discover.json` — IPTV and HDHomeRun endpoints.

Created in [T3 Code](https://t3.codes).
