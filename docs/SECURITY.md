# Security and public exposure

This page describes the controls that are implemented today and the ones that
are still open. It is a working security note, not a claim of completeness.

## Public surfaces

| Surface | Route | Protection |
|---|---|---|
| Watch | `/` | Public, read-only UI |
| Studio | `/admin` | Login-gated shell; anonymous callers get the sign-in page |
| Public API | `/api/v1/public/*` | Read-only projections, per-client budget |
| Public HLS | `/channel/*` | Per-client budget |
| Cast | `/cast/*`, `POST /api/v1/public/channels/{n}/cast` | Signed short-lived URLs, per-client budget |
| Catch-up / DVR | `/catchup/*`, `/dvr/*` | Per-client budget |
| MPEG-TS / HDHomeRun | `/auto/*`, `/tuner/*` | Per-client budget, global and per-client stream leases |
| Cloud proxy | `/cloud/*` | Signed URL, per-client budget |
| Admin API | everything else under `/api/v1/*` | Session cookie, Basic auth, or Bearer API token |

The full admin JavaScript and markup are not sent to anonymous callers. The
public API projects only the fields Watch needs.

## Login throttling

Failed sign-ins, Basic auth attempts, and invalid Bearer tokens are tracked in
memory per client. After 10 failures in 15 minutes, login and protected admin
routes return `429` with `Retry-After`. A successful sign-in clears the counter.
Counters reset on restart, which only makes the limiter more permissive for one
window.

Sessions are in memory and expire after seven days. A restart signs everyone
out. Cookies are `HttpOnly`, `SameSite=Lax`, and marked `Secure` when the
request arrived over HTTPS (including a trusted `X-Forwarded-Proto`).

## API tokens

Studio **Settings → API tokens** creates revocable Bearer tokens for scripts,
native clients, and automation.

- Tokens look like `cl_<id>_<secret>` and are shown once at creation.
- Only a SHA-256 hash of the secret is stored in SQLite.
- Send `Authorization: Bearer <token>` to any `/api/v1/*` admin route.
- A token grants full Studio API access. There are no scopes yet.
- Revoking a token takes effect on the next request.
- `last_used_at` is updated at most once per minute.

Basic auth and the browser session cookie remain supported.

## Public request budgets

Public media is limited per client and per route class in a fixed one-minute
window. Counters are bounded and pruned. A rejected request gets
`429 Too Many Requests`, `Retry-After`, and `Cache-Control: no-store`.

| Class | Routes | Default per minute |
|---|---|---|
| API | `/api/v1/public/*`, `/iptv/*`, `/discover.json`, `/lineup*.json`, `/device.xml` | 120 |
| Cast start | `POST /api/v1/public/channels/{n}/cast` | 20 |
| Playlist | `/channel/*.m3u8`, `/cast/*.m3u8`, `/dvr/*.m3u8`, `/catchup/*.m3u8` | 120 |
| Segment | all `.ts` and `/cloud/*` responses | 1800 |
| Stream | `/auto/*`, `/tuner/*` MPEG-TS sessions | 12 |

Set `AWESOMETV_RATE_LIMIT_ENABLED=false` to disable the budgets, or tune each
class:

```text
AWESOMETV_RATE_LIMIT_API_PER_MIN=120
AWESOMETV_RATE_LIMIT_CAST_PER_MIN=20
AWESOMETV_RATE_LIMIT_PLAYLIST_PER_MIN=120
AWESOMETV_RATE_LIMIT_SEGMENT_PER_MIN=1800
AWESOMETV_RATE_LIMIT_STREAM_PER_MIN=12
```

A value of `0` disables that class. The defaults are deliberately generous for
a household or a few remote viewers behind one NAT address; lower them for a
public deployment with a single upstream.

## Concurrent stream limits

MPEG-TS and HDHomeRun sessions hold a lease until the ffmpeg process exits:

| Setting | Default | Meaning |
|---|---:|---|
| `AWESOMETV_MAX_CONCURRENT_STREAMS` | 4 | Global tuner cap; also advertised in `discover.json` and `device.xml` |
| `AWESOMETV_MAX_STREAMS_PER_CLIENT` | 2 | Per-client cap; `0` disables the per-client portion |

When no lease is available, `/auto/*` and `/tuner/*` return `503`. HLS viewers
are not counted against this cap because they share the supervised producers.

## Client identity and proxies

When the immediate peer is loopback, private, link-local, or Tailscale
(`100.64.0.0/10`), the app trusts `X-Real-IP` first and the last
`X-Forwarded-For` entry second. This is the deployment shape used by the nginx
and SWAG configs in `deploy/nginx/`.

A direct public peer cannot choose its own bucket by sending forwarding
headers; the peer address is used instead. If you put a new proxy in front of
the app, make sure it overwrites `X-Real-IP` rather than appending to a
client-supplied value.

## SSRF, paths, and commands

- Google Drive API and OAuth endpoints must use HTTPS.
- Service-account and OAuth `token_uri` values pass the same validation.
- URLs with embedded credentials, `localhost`, `.internal`, `.localhost`, and
  private, loopback, link-local, unspecified, or Tailscale literal addresses are
  rejected.
- Google remote IDs are percent-encoded as one path segment, so a crafted ID
  cannot add query parameters or escape `/files/`.
- Segment handlers reject `/`, `\`, and `..`; HLS, DVR, catch-up, and cloud
  paths are built from server-generated IDs.
- ffmpeg and ffprobe are invoked with argument arrays, never through a shell.
- User-supplied schedule YAML/JSON is parsed as data; the app does not execute
  an external scheduler.

Residual risk: DNS rebinding and future provider adapters need their own
review. Repeat this review before adding webhooks or P2P peer URLs.

## Secret rotation

Cloud secrets and signed URLs use `AWESOMETV_SECRET_KEY`. To rotate:

1. Back up `data/` (including `data/secrets/cloud.key` if you use the file key).
2. Generate a new 32-byte base64 key.
3. Set the new value as `AWESOMETV_SECRET_KEY` and put the old value in
   `AWESOMETV_SECRET_KEY_PREVIOUS`. Comma-separate multiple old keys.
4. Restart once. Startup decrypts cloud-source secrets with the old key and
   re-encrypts them with the new primary key. The log reports the count.
5. Remove `AWESOMETV_SECRET_KEY_PREVIOUS` and restart again.

Short-lived Cast and cloud URLs signed with the old key stop validating at
rotation; they expire quickly anyway. If the key is lost and no backup exists,
delete and re-add the affected cloud sources. Local media and schedules do not
depend on the secret key.

## Still open

- OIDC, roles, and an audit log.
- Provider webhook signature verification and webhook rate limits. No webhook
  receiver is implemented yet.
- Optional LAN/Tailscale-only Studio exposure until SSO exists.
- API token scopes, optional expiry, and per-token rate limits.
- A repeat of the SSRF review before adding peer URLs or new providers.
