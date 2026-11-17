# Cloud source design

Status: shared core and Google Drive adapter implemented. Interactive OAuth and provider webhooks remain deferred. The public live/guide viewer and protected `/admin` console are deployed.

## Decisions

| Area | Decision |
|---|---|
| Providers | Google Drive, Dropbox, OneDrive, S3-compatible, WebDAV |
| Google scope | My Drive, shared drives, admin-selected folders |
| Auth | OAuth where available, plus Google service accounts; S3 keys and WebDAV basic auth |
| OAuth apps | Admin supplies each provider's client ID/secret; nothing bundled |
| Delivery | Hybrid: per-source `cache_all`, `stream_all`, or `auto` |
| Cache budget | Environment setting (`CLOUD_CACHE_MAX_GB`) |
| Eviction | LRU by last access, with active playback/DVR pinned |
| Metadata | Provider filename plus ffprobe on scan through the proxy |
| Probe timing | During scan, with bounded concurrency |
| Content | Video, music, images/artwork, subtitles/sidecars |
| Duplicates | Cloud copies remain separate sources; no automatic merge |
| Offline | Cached playback only, background retry; unavailable otherwise |
| Streaming | Authenticated server proxy; provider tokens never reach FFmpeg or clients |
| DVR/catch-up | Local HLS segments; source may be remote |
| Change feeds | Public provider webhooks now, with delta cursors for reconciliation |
| Public access | Anonymous live channels + guide only |
| Admin access | Existing console moved to `/admin`, protected |
| Accounts | One admin/owner account |
| Library | Cloud media appears as normal library items with a source badge |

## Architecture

```text
provider APIs
   │  OAuth / service account / keys
   ▼
cloud adapters (Google, Dropbox, OneDrive, S3, WebDAV)
   │
   ├── folder browser + metadata/listing
   ├── authenticated byte proxy + range requests
   └── change feed/webhook normalization
   ▼
cloud source service
   ├── sync/index jobs
   ├── ffprobe-on-scan
   ├── hybrid cache + LRU eviction
   └── media_items updates
   ▼
existing catalog, scheduler, engine, DVR
   ▼
public viewer (live + guide) / admin console (/admin)
```

Cloud media is a normal `media_items` row with a synthetic path:

```text
cloud://{source_id}/{remote_id}
```

The engine resolves that path to either:

- a cached local file, or
- the authenticated proxy URL `http://127.0.0.1:8409/cloud/{source}/{remote_id}?sig=...`.

## Data model

### `cloud_sources`

- `id`
- `provider` (`google`, `dropbox`, `onedrive`, `s3`, `webdav`)
- `name`
- `auth_type` (`oauth`, `service_account`, `access_key`, `basic`)
- `policy` (`cache_all`, `stream_all`, `auto`)
- `enabled`
- `change_cursor` / `webhook_channel_id`
- `last_sync_at`, `last_error`
- `created_at`

### `cloud_source_secrets`

- `source_id`
- `key_version`
- `encrypted_payload` (OAuth refresh token, service-account JSON, access keys, webhook secret)

Encryption uses AES-GCM with a 0600 key at `data/secrets/cloud.key`, or an
`AWESOMETV_SECRET_KEY` environment override.

### `cloud_roots`

- `id`, `source_id`
- `remote_id` / `remote_path`
- `display_path`
- `enabled`

Selected folders from the provider tree. Shared drives use the same table with
provider-specific root IDs.

### `cloud_media`

- `media_id` → `media_items.id`
- `source_id`
- `remote_id`, `remote_path`
- `size_bytes`, `mime_type`, `etag`, `remote_modified_at`
- `cache_state` (`none`, `probing`, `cached`, `error`)
- `cache_path`, `cached_bytes`
- `last_seen_at`, `last_probe_at`

`media_items.path` stays unique using `cloud://...`; the `cloud_media` row holds
the provider-specific identity.

### `cloud_cache_entries`

- `media_id`, `source_id`
- `path`, `bytes`, `last_access_at`, `pinned_until`

Used by the LRU evictor.

## Provider abstraction

Each adapter exposes capabilities and a small set of operations:

```text
capabilities:
  oauth, service_account, shared_drives, folder_listing,
  range_requests, delta_cursor, webhooks, signed_urls

list_children(root, cursor)
stat(remote_id)
open_range(remote_id, start, end)
download(remote_id, destination)
delta(cursor)
parse_webhook(headers, body)
subscribe_webhook(callback_url)
unsubscribe_webhook()
```

Provider-specific notes:

- **Google Drive:** `supportsAllDrives=true`, shared-drive roots, `changes` API,
  push channels with `X-Goog-Channel-Token` validation and channel renewal.
- **Dropbox:** cursor-based `files/list_folder/continue`, webhook HMAC SHA-256.
- **OneDrive/Graph:** delta links, subscription validation via `clientState`,
  subscription renewal before expiry.
- **S3-compatible:** list-objects v2, range GET, presigned or signed proxy
  requests; event notifications are optional and provider-specific. Polling the
  bucket listing/inventory is the reconciliation fallback.
- **WebDAV:** PROPFIND listing, range GET, basic/digest auth, no change feed;
  periodic reconciliation.

## Auth and secrets

- Admin enters provider OAuth client IDs/secrets in Settings.
- OAuth flow:
  - `POST /api/v1/cloud/sources/{id}/oauth/start`
  - provider redirect URI points at the public/admin callback
  - callback exchanges the code, stores only the refresh token encrypted
- Google service accounts:
  - admin uploads a service-account JSON file
  - the file is encrypted at rest
  - folders must be shared with the service-account email
- S3: access key/secret, endpoint, region, bucket, prefix, path-style flag.
- WebDAV: base URL, username, password.
- The admin UI never displays stored secrets after entry.

## Gateway security

- Public viewer:
  - `/` channel list, now/next, guide
  - `/channel/{number}.m3u8` and live segments
  - no collections, schedules, sources, DVR, catch-up, backups, or admin APIs
- Admin:
  - `/admin` and all `/api/v1/*` admin routes
  - session login or Basic auth
- Webhooks:
  - `/webhooks/cloud/{provider}/{source_id}`
  - per-source random secret/token
  - provider-specific signature/validation
  - enqueue work and return quickly; never do a full scan inline
- Public exposure:
  - a dedicated public hostname/port via Tailscale Funnel or a reverse proxy
  - admin hostname/path remains private or separately protected
  - webhook rate limits and concurrency caps (public HLS budgets are in place)

## Scanning and metadata

1. Enumerate selected roots with pagination.
2. Filter by supported extensions and content types.
3. Create/update `media_items` and `cloud_media`.
4. Probe with ffprobe through the byte proxy, bounded to 2–4 concurrent probes.
5. Store duration, dimensions, codecs, and probe status.
6. Reconcile deletions after a full listing or delta sync.
7. Record sync status and errors per source.

Cloud items appear in Library, Collections, Programming, Guide, and EPG exactly
like local items, with a provider/cache badge.

## Hybrid delivery and cache

Per-source policy:

- `cache_all`: download before playback; play from local cache.
- `stream_all`: always play through the proxy; only DVR segments are local.
- `auto` (default):
  - cache small files and formats that need remuxing/probing;
  - stream large direct-play-friendly files;
  - thresholds configurable per source.

Cache budget: `CLOUD_CACHE_MAX_GB` (environment). Eviction is LRU by last access,
never evicting active playback, active DVR, or recently probed segments.

Cached files live under:

```text
data/cloud-cache/{source_id}/{media_id}.{ext}
```

Downloads use the same proxy path as streaming, so provider tokens never leave
the server.

## Streaming proxy

Internal URL:

```text
GET /cloud/{source_id}/{remote_id}?sig=...
```

- HMAC signature with a short expiry, generated for FFmpeg.
- Supports `HEAD` and `Range`, returning 206 with correct `Content-Range`.
- Streams provider bytes without buffering the whole file.
- Handles token refresh and provider retries.
- Public clients never receive provider tokens or direct provider URLs.

## DVR and catch-up

- DVR/catch-up always write encoded HLS segments locally.
- Cloud-sourced items use the same proxy input as live playout.
- Cache-all sources may already have a local file; the engine prefers it.
- LRU eviction must skip media pinned by an active DVR session.

## Change feeds and webhooks

Webhook endpoint:

```text
POST /webhooks/cloud/{provider}/{source_id}
```

- Verify provider-specific secret/signature.
- Normalize to `{source, remote_id, change_type}`.
- Queue an incremental sync for that source.
- Maintain subscriptions:
  - Google channels expire; renew before expiry.
  - OneDrive subscriptions expire; renew before expiry.
  - Dropbox webhooks are app-wide; map to sources by account.
- Delta cursors remain the source of truth; webhooks only reduce latency.
- A nightly/manual reconcile repairs missed events.
- Public webhook exposure requires a dedicated public hostname; the admin
  console stays protected.

## API surface

Admin (`/api/v1`):

```text
GET    /cloud/providers
GET    /cloud/sources
POST   /cloud/sources
GET    /cloud/sources/{id}
PUT    /cloud/sources/{id}
DELETE /cloud/sources/{id}
POST   /cloud/sources/{id}/oauth/start
GET    /cloud/oauth/callback
POST   /cloud/sources/{id}/service-account
GET    /cloud/sources/{id}/browse?folder=...
POST   /cloud/sources/{id}/scan
GET    /cloud/sources/{id}/status
POST   /cloud/sources/{id}/webhook-subscribe
```

Public:

```text
GET /api/public/channels
GET /api/public/guide
GET /channel/{number}.m3u8
GET /channel/{number}/{segment}.ts
POST /webhooks/cloud/{provider}/{source_id}
```

Internal:

```text
GET /cloud/{source}/{remote_id}?sig=...
```

## Admin UI

Sources → Cloud sources:

- provider cards with capability and auth status
- add-source wizard:
  - provider
  - auth method
  - OAuth client app or service-account upload
  - folder tree browser with checkboxes
  - policy (`cache_all`, `stream_all`, `auto`)
  - scan/probe options
- source card:
  - item count, cached size, cache budget
  - last sync, change-feed status
  - scan/refresh, repair, disable, delete
- Settings:
  - OAuth client IDs/secrets per provider
  - cache budget and eviction status
  - webhook/public endpoint status

## Phasing

The user selected all providers in parallel. The shared core is built once and
all adapters target it:

1. **Core**
   - schema, secret encryption, source CRUD, folder browsing, sync jobs,
     ffprobe-on-scan, proxy, hybrid policy, LRU cache
2. **Google Drive**
   - OAuth, service account, My Drive + shared drives, changes API, push channels
3. **Dropbox + OneDrive**
   - cursors/delta, webhooks, subscriptions
4. **S3-compatible + WebDAV**
   - list/range/proxy; S3 event notifications optional; WebDAV polling
5. **Public viewer/admin split** — done
   - public live + guide at `/`, protected `/admin`, public read-only APIs
6. **Hardening**
   - rate limits, retries/backoff, provider quotas, observability, cache metrics

## Risks

- Provider webhook/public exposure conflicts with a private admin surface; a
  dedicated public hostname and strict route separation are required.
- Google Drive `changes` for shared drives and service accounts has different
  identifiers than My Drive.
- Large cloud libraries with scan-time ffprobe will be slow; bounded concurrency
  and resumable scan jobs are required.
- Streaming through the server consumes server bandwidth and CPU; provider
  signed URLs would be faster but weaken token isolation.
- WebDAV and some S3 providers have no change feed; reconciliation polling is
  unavoidable.
- Public HLS needs abuse controls without breaking native players.
- Public redistribution of cloud content may have provider terms-of-service
  implications; the admin owns that policy.

## Verification plan

- Unit tests for provider URL/path builders, policy selection, cache eviction,
  signature verification, and path traversal protection.
- Mock providers:
  - local MinIO for S3
  - local WebDAV server
  - recorded Google/Dropbox/OneDrive API fixtures
- Real integrations:
  - Google service account in a test shared drive
  - Google OAuth test account
  - Dropbox/OneDrive developer apps
- End-to-end:
  - add source, browse folders, scan, probe
  - schedule cloud media
  - play via proxy and via cache
  - verify DVR/catch-up from cloud media
  - send a signed webhook and observe an incremental sync
  - confirm public live/guide works anonymously and `/admin` remains protected
```


## Implemented in this milestone

### Core

- `cloud_sources`, `cloud_roots`, `cloud_media`, and `cloud_provider_config` tables.
- AES-256-GCM secret encryption using `data/secrets/cloud.key` or
  `AWESOMETV_SECRET_KEY`.
- HMAC-signed internal proxy URLs with expiry.
- Cloud sources with `auto`, `cache_all`, or `stream_all` policy.
- Folder browsing and root selection by folder ID, including shared-drive
  folders when Google returns them.
- Scan-time ffprobe through the signed proxy; cloud media become normal
  `media_items` with `cloud://` paths.
- Background cache-on-play with `CLOUD_CACHE_MAX_GB` and
  `CLOUD_AUTO_CACHE_MAX_MB`; LRU eviction by last access.
- Streaming byte proxy with Range support and chunked response streaming.
- Engine, catch-up, and DVR resolve cloud media to either a cached local file or
  the proxy URL.

### Google Drive

- Access-token authentication for local development and mock testing.
- Service-account authentication: PKCS#8 key parsing and RS256 JWT token
  exchange using `ring`.
- OAuth refresh-token credentials are supported by the adapter, but the
  interactive browser consent/callback flow is not wired yet.
- Folder listing, shared-drive folder access via `supportsAllDrives`, file
  metadata, media download, and Range downloads.
- Shared-drive listing endpoint exists; the UI currently browses by folder ID.

### UI

- Sources → **Cloud sources** panel.
- Add Google Drive source with service-account JSON or access token.
- Policy selection and optional initial folder ID.
- Source list with scan/delete.
- Folder browser with folder navigation and “add current folder”.
- Cloud media appears in Library and can be scheduled like local media.

### Verified

- Mock Google Drive API served folders and a real MP4.
- Source creation, folder browse, and scan succeeded: 4 items scanned and probed.
- Cloud media appeared as `cloud://` media items with duration and codecs.
- Playback started from the signed proxy URL, then subsequent producers used the
  cached local file.
- Cache file written under `data/cloud-cache/...`; database state changed to
  `cached`.
- Proxy/ffprobe exercised Range requests against the mock provider.

### Deferred / known limits

- Interactive Google OAuth consent and callback.
- Provider webhooks.
- Dropbox, OneDrive, S3, and WebDAV adapters.
- Real Google service-account validation (the code path compiles but was tested
  only with access-token/mock credentials).
- Shared-drive root browsing in the UI; folder IDs work.
- Incremental delta sync and rename reconciliation; scans are full listings. A full rescan removes cloud media that disappeared.
- Scan jobs are in memory: they survive while the process runs, not across restarts.
- LRU eviction does not yet pin active DVR/playback sessions.


## Planning documents

- [Cloud provider completion pathway](CLOUD-COMPLETION.md)
- [Peer-to-peer channel sourcing](P2P-CHANNELS.md)
- [Cloud and P2P sequencing](CLOUD-AND-P2P-SEQUENCING.md)
