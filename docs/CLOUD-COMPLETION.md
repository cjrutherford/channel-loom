# Cloud provider completion pathway

Status: shared core + Google Drive reference adapter implemented. This document
defines what remains before the cloud provider feature is complete.

## Scope boundary

Cloud providers answer: **what media files may ChannelLoom index and play?**

They do not provide channels, schedules, or guides. Peer-to-peer channel
sourcing is a separate feature.

## Current state

Implemented:

- cloud source/root/media schema
- encrypted secrets and key file
- provider trait/registry with Google Drive as the first adapter
- Google Drive access-token and service-account auth
- Google folder listing and Range downloads
- proxy HEAD, redirect validation, Range fallback signaling, and content headers
- folder browsing and scan-time ffprobe
- `cloud://` media in the normal library
- signed streaming proxy
- per-source `auto` / `cache_all` / `stream_all`
- background caching and LRU eviction
- engine, catch-up, and DVR resolution
- Sources UI for add/browse/scan/delete
- source edit: name, policy, enabled/disabled
- background scan jobs with progress, polling, and cancellation
- full-rescan reconciliation for missing cloud media
- Library source badges and Local/Cloud filter
- per-source and global cloud metrics (media, cached media, cached bytes, errors)
- Prometheus gauges for cloud sources, media, cache, and source errors
- public viewer/admin split (C2 dependency)

Not complete:

- real Google service-account validation
- interactive OAuth consent/callback
- provider webhooks
- shared-drive root browsing and shortcuts
- per-root editing and enable/disable
- incremental delta sync and rename reconciliation (full-rescan reconciliation is implemented)
- active-playback/DVR cache pinning
- Dropbox, OneDrive, S3, WebDAV adapters

## Phases

### C0 — Credential and test readiness

Goal: make real provider testing possible.

Deliverables:

- Google Cloud project with Drive API enabled
- service account JSON and a test folder shared to the service account
- Google OAuth client (Web application) once the callback hostname is chosen
- Dropbox and Microsoft OAuth apps when those adapters start
- local MinIO for S3 testing
- local WebDAV test endpoint
- test media folder with video, music, images, subtitles, and sidecars
- secure secret storage / key backup procedure

Exit criteria:

- real Google service account can list the test folder
- test media is accessible without copying secrets into the repository
- S3/WebDAV test endpoints are reachable

### C1 — Core hardening

Goal: make the Google path production-grade before multiplying adapters.

Deliverables:

- resumable scan persistence across restarts (current jobs are in-memory only)
- incremental delta sync and rename reconciliation
- shared-drive root browsing and Google shortcut handling
- root editing and per-root enable/disable
- cache pinning for active playback and DVR; cache usage UI

Exit criteria:

- source can be created, edited, scanned, used, disabled, and deleted
- full scan and incremental scan both reconcile correctly
- cache eviction never removes active playback/DVR content
- a 10k-file mock library scans without losing jobs or corrupting metadata

### C2 — OAuth, webhooks, and public exposure

Goal: finish interactive auth and near-realtime updates.

Dependencies: public HTTPS hostname. The admin/public route split is already in place.

Deliverables:

- OAuth start/callback with state and PKCE where supported
- encrypted provider client ID/secret configuration UI
- token refresh, revoke, and reconnect
- webhook endpoint with provider-specific verification
- subscription creation/renewal/removal
- public webhook exposure through the chosen ingress
- admin-only source management; public viewer has no admin routes

Exit criteria:

- a real Google account can be connected through the browser
- token refresh works after expiry
- a signed webhook triggers an incremental sync
- webhook failures fall back to delta polling
- public endpoints cannot reach admin/source APIs

### C3 — Additional providers

Build all adapters against the same provider interface and test matrix.

#### Dropbox

- OAuth
- cursor-based listing and delta
- webhook signature verification
- Range download

#### OneDrive / Graph

- OAuth
- delta links
- subscription validation and renewal
- Range download

#### S3-compatible

- endpoint, region, bucket, prefix, path-style flag
- access key/secret or instance role
- list-objects v2, Range GET
- optional S3 event notifications; listing reconciliation fallback

#### WebDAV

- base URL, basic/digest auth
- PROPFIND recursive listing
- Range GET
- periodic reconciliation (no change feed)

Exit criteria:

- each provider passes the shared conformance test:
  add source → browse → scan → probe → stream → cache → evict → delete
- each provider reports capabilities and health accurately
- no provider-specific code leaks into the scheduler/engine

### C4 — Completion and acceptance

Deliverables:

- real smoke tests for Google, Dropbox, OneDrive, MinIO/S3, WebDAV
- provider capability matrix
- scan/cache/stream performance test with a large library
- security review: SSRF, token leakage, proxy signature, secret rotation
- operational runbook and user documentation
- source health surfaced in the UI
- upgrade/migration path for the new schema

Definition of done:

- all five providers support add/auth/browse/scan/play/cache
- service accounts and OAuth both work where applicable
- large scans are resumable and observable
- cache stays within budget and never evicts active content
- cloud media can be scheduled, played, caught up, and DVR’d
- public viewer/admin split is in place if public exposure is enabled
- docs cover setup, credentials, policies, and troubleshooting

## Prerequisites before moving forward

| Prerequisite | Why |
|---|---|
| Real Google service account + test folder | Validate JWT/token/listing/download against Google |
| OAuth client IDs and redirect URI plan | Required for interactive Google/Dropbox/OneDrive auth |
| Dropbox/OneDrive developer accounts | Required for those adapters |
| MinIO or S3 endpoint | S3 adapter testing |
| Local WebDAV endpoint | WebDAV adapter testing |
| Cache disk budget and backup plan | Prevent disk exhaustion and key loss |
| Public hostname decision | OAuth callback and webhooks |
| Admin/public route split decision | Security boundary |
| 10k-file test library | Scale and reconciliation testing |

## Recommended order

1. C0 credentials and test assets.
2. C1 core hardening and Google production validation.
3. Decide public hostname and route split.
4. C2 OAuth/webhooks.
5. C3 adapters in parallel.
6. C4 conformance and docs.

P2P channel sourcing can begin after C1 because it reuses the signed proxy,
secret handling, rate limiting, and streaming path.

## Risks

- Google service-account JWT and shared-drive identifiers differ from My Drive.
- Scan-time ffprobe over the network is bandwidth-heavy and slow at scale.
- Provider rate limits require queuing/backoff.
- OAuth without a public HTTPS callback blocks interactive auth.
- Public webhooks require exposing a narrow endpoint without exposing admin.
- Cache eviction with active playback needs explicit pinning.
- Provider terms of service apply to redistribution/public viewing.
