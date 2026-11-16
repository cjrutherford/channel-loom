# Execution plan

Status: active plan. Implementation coverage is reviewed in
[`PLAN-REVIEW.md`](PLAN-REVIEW.md) (2026-11-16).

## Current baseline

The repository includes:

- Rust server, scheduler, engine, DVR, catch-up, prewarm, metrics
- 12 live channels and the expanded media library
- public viewer/admin split and the nginx/SWAG deployment
- PWA, Android TV APK, Tauri desktop bundle, and native CI
- Google Cast, including the cast-mode overlay, branded receiver support, and
  continuous programme handoff
- cloud source core and Google Drive reference adapter
- schedule-driven precache for the slow NAS
- design/planning documents for cloud completion and P2P channels

Completed workstreams since the original baseline: CI, native builds, Google
Cast, public viewer/admin split, and precache. Remaining workstreams are cloud
completion, deep parity, P2P, and security/operations.

## Workstreams

### A. Cloud provider completion

Detailed plan: [`CLOUD-COMPLETION.md`](CLOUD-COMPLETION.md)

| Phase | Focus | Status |
|---|---|---|
| C0 | Real Google service account, test folder, provider test assets | not done |
| C1 | Background scans, incremental delta, cache pins, provider registry | partial — source edit, full reconciliation, badges, and metrics done |
| C2 | Interactive OAuth, webhooks, public/admin split | split done; OAuth/webhooks not started |
| C3 | Dropbox, OneDrive, S3-compatible, WebDAV | not started |
| C4 | Real-provider tests, security review, performance, docs | not started |

### B. Peer-to-peer channel sourcing

Detailed plan: [`P2P-CHANNELS.md`](P2P-CHANNELS.md)

| Phase | Focus | Gate |
|---|---|---|
| P0 | Peer identity, well-known endpoint, manual peer add | not started |
| P1 | Pairing, scoped grants, shared channel selection | not started |
| P2 | Remote guide, M3U/XMLTV, live HLS proxy | not started |
| P3 | Multi-peer hardening, optional catch-up/DVR/discovery | not started |

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

### D. Google Cast (first-class) — implemented

Cast is a first-class playback target, not an afterthought.

#### Backend

- `POST /api/v1/channels/{id}/cast` and
  `POST /api/v1/public/channels/{number}/cast` return a Cast-friendly media URL
  and title, warming the producer first.
- Public `/cast/{channel}.m3u8` and `/cast/{channel}/{segment}.ts` routes use a
  short-lived HMAC token and CORS headers.
- The playlist is rewritten with absolute segment URLs and hands off
  continuously across programme boundaries.
- `AWESOMETV_CAST_BASE_URL` selects the address a Cast device can reach (LAN or
  public). The current deployment uses `http://192.168.1.153:8409`.
- `AWESOMETV_CAST_RECEIVER_ID` selects the Default, Styled, or Custom receiver.
  The custom receiver page is served at `/cast-receiver`.

#### Frontend

- Cast buttons in the public viewer and admin.
- Cast SDK preloaded after configuration so the device picker keeps the click's
  user gesture.
- Cast-mode overlay: local video pauses, page controls drive the receiver.
- Play/pause, mute, volume, stop, and channel switching while casting.
- Graceful failure if the SDK is unavailable or the origin is not secure.

#### Operational requirements

- Chromecast must be able to reach `AWESOMETV_CAST_BASE_URL`.
- Tailscale-only URLs often cannot be reached by a Chromecast; a LAN URL is the
  normal self-hosted Cast path.
- Cast media URLs are scoped and expire.
- Public exposure is not required for LAN Cast.

#### Implemented baseline

- signed Cast HLS playlist and segment routes
- absolute segment URLs and token expiry
- Cast button in the public viewer and admin
- cast-mode overlay with remote play/pause/mute/volume/stop
- channel switching while casting and resume-local on stop
- configurable Default/Styled/Custom receiver App ID
- continuous playlist handoff across programme boundaries
- cast base URL in public config and Settings

### E. Security and public exposure

- Admin/public route split: **done**.
- Public login throttling and secure cookies: **done**.
- API tokens (Bearer, hashed, revocable in Studio): **done**.
- Rate limits for public HLS, Cast, catch-up, DVR, and webhooks: **public media and API budgets done**; webhook limits will land with webhook verification.
- Secrets encrypted at rest; key rotation with previous keys and startup re-encryption: **done**.
- SSRF protections for cloud and peer URLs: **reviewed for current surfaces**; repeat before P2P/provider webhooks.
- Provider/webhook signature verification: **not started**; no webhook receiver exists yet.
- No admin or library data on public/peer surfaces: **public API projections are clean; peer surfaces do not exist yet**.

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

- UI stack: keep the current vanilla JS UI or migrate to React/TypeScript.
- Real Google service account/test folder and OAuth redirect URI.
- Dropbox/OneDrive app registration timing.
- S3/WebDAV test endpoints.
- Cache budget and secret-key recovery policy.
- P2P discovery, pairing, share scope, numbering, re-sharing policy.
- OIDC/roles/audit scope and whether `/admin` stays LAN/Tailscale-only until then.
- Public capacity policy and capacity calculator (stream caps are in place).
- ARM target and hardware profiles.

Decided: public hostname and reverse proxy (`atv.christopherrutherford.net`),
LAN Cast base (`http://192.168.1.153:8409`), and the public viewer/admin split.
