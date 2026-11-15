# Execution plan

Status: active plan after the Phase 4 core baseline.

## Commit boundary

The current repository state is the **Phase 4 + cloud core baseline**. It
includes:

- Rust server, scheduler, engine, DVR, catch-up, prewarm, metrics
- 12 live channels and the expanded media library
- PWA app shell over Tailscale HTTPS
- Android TV APK and Tauri desktop scaffold
- cloud source core and Google Drive reference adapter
- design/planning documents for cloud completion and P2P channels

Work after this boundary belongs to the next milestone commits: CI, native
builds, Google Cast, cloud completion, and P2P.

## Workstreams

### A. Cloud provider completion

Detailed plan: [`CLOUD-COMPLETION.md`](CLOUD-COMPLETION.md)

| Phase | Focus | Gate |
|---|---|---|
| C0 | Real Google service account, test folder, provider test assets | list/download works against Google |
| C1 | Core hardening: background scans, delta reconciliation, source edit, cache pins, source badges | production-grade Google path |
| C2 | Interactive OAuth, webhooks, public/admin split | browser auth and signed webhooks work |
| C3 | Dropbox, OneDrive, S3-compatible, WebDAV | shared provider conformance suite |
| C4 | Real-provider tests, security review, performance, docs | cloud feature complete |

### B. Peer-to-peer channel sourcing

Detailed plan: [`P2P-CHANNELS.md`](P2P-CHANNELS.md)

| Phase | Focus | Gate |
|---|---|---|
| P0 | Peer identity, well-known endpoint, manual peer add | two hosts identify each other |
| P1 | Pairing, scoped grants, shared channel selection | subscribe/revoke works |
| P2 | Remote guide, M3U/XMLTV, live HLS proxy | remote channel plays and guides correctly |
| P3 | Multi-peer hardening, optional catch-up/DVR/discovery | stable multi-peer behavior |

Recommended MVP: Tailscale/LAN direct HTTP, manual peer URL + invite code,
read-only remote channels, live/guide only, no re-sharing.

### C. Native applications and CI

#### Android TV

- Source: `android-tv/`
- CI builds debug APK and uploads it as an artifact.
- Release job builds signed APKs when signing secrets are configured.
- The TV URL defaults to the Tailscale HTTPS app and can be overridden with
  `-Pawesometv.url=...`.

#### Tauri desktop

- Source: `desktop/`
- CI installs WebKitGTK/GTK build dependencies and builds the desktop bundle.
- Linux is the first supported target; Windows/macOS need their own runners and
  signing.
- The app loads `AWESOMETV_URL` (default Tailscale HTTPS).

#### PWA

- Already installable over HTTPS.
- CI validates manifest, service worker, and icons through the server build.

### D. Google Cast (first-class) — implemented baseline

Cast is a first-class playback target, not an afterthought.

#### Backend

- `POST /api/v1/channels/{id}/cast` returns a Cast-friendly media URL and title.
- Public `/cast/{channel}.m3u8` and `/cast/{channel}/{segment}.ts` routes use a
  short-lived HMAC token.
- The playlist is rewritten with absolute segment URLs and works with the
  Chromecast Default Media Receiver.
- `AWESOMETV_CAST_BASE_URL` selects the address a Cast device can reach (LAN or
  public). If unset, the configured public URL is used. The current deployment
  uses `http://192.168.1.153:8409` for LAN Cast.

#### Frontend

- Cast button in the console.
- Cast SDK loaded lazily only when used.
- Uses `cast.framework.CastContext` and the Default Media Receiver.
- Casts the currently selected channel.
- Supports stop/end session.
- Graceful failure if the SDK is unavailable or the origin is not secure.
- Cast status shown in the UI.

#### Operational requirements

- Chromecast must be able to reach `AWESOMETV_CAST_BASE_URL`.
- Tailscale-only URLs often cannot be reached by a Chromecast; a LAN URL is the
  normal self-hosted Cast path.
- Cast media URLs are scoped and expire.
- Public exposure is not required for LAN Cast.

#### Implemented baseline

- signed Cast HLS playlist and segment routes
- absolute segment URLs and token expiry
- Cast button and lazy Cast SDK loading
- Default Media Receiver casting of the selected channel
- stop/end session handling
- Cast base URL in public config and Settings

### E. Security and public exposure

- Admin/public route split when public viewing is enabled.
- Rate limits for public HLS, Cast, catch-up, DVR, and webhooks.
- Secrets encrypted at rest; key rotation documented.
- SSRF protections for cloud and peer URLs.
- Provider/webhook signature verification.
- No admin or library data on public/peer surfaces.

## Sequencing

```text
Baseline commit
   │
   ├─ CI + native builds
   ├─ Google Cast
   │
   ├─ Cloud C0 → C1
   │     └─ decide public hostname
   │           ├─ C2 OAuth/webhooks
   │           └─ P2P P0 → P1 → P2
   │
   └─ C3 adapters in parallel with P2 hardening
         └─ C4 + P3 acceptance
```

Recommended order:

1. Commit the baseline boundary.
2. Land CI for Rust, Android TV, Tauri, and PWA assets.
3. Land Google Cast backend + UI + docs.
4. Validate real Google service accounts (C0).
5. Harden cloud core (C1).
6. Decide public hostname; start C2 and P2P in parallel.
7. Add remaining cloud adapters (C3).
8. Finish cloud (C4) and P2P (P3).

## CI matrix

| Job | What it builds | Artifact |
|---|---|---|
| `server` | fmt, clippy, tests, release binary | server binary |
| `docker` | production Docker image | Docker image |
| `android-tv` | debug APK | `app-debug.apk` |
| `desktop` | Tauri Linux bundle | AppImage/deb |
| `pwa-assets` | manifest/service-worker/icon validation | none |
| `release` (tags) | server, APK, desktop bundle, Docker image | GitHub release assets |

## Decisions pending

- Public hostname and OAuth redirect URI.
- Real Google service account/test folder.
- Dropbox/OneDrive app registration timing.
- S3/WebDAV test endpoints.
- Cache budget and secret-key recovery policy.
- P2P discovery, pairing, share scope, numbering, re-sharing policy.
- Cast base URL (LAN vs public) for the deployment.
