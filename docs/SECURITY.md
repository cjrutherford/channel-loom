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
| MPEG-TS / HDHomeRun | `/auto/*`, `/tuner/*` | Per-client budget |
| Cloud proxy | `/cloud/*` | Signed URL, per-client budget |
| Admin API | everything else under `/api/v1/*` | Session cookie or Basic auth |

The full admin JavaScript and markup are not sent to anonymous callers. The
public API projects only the fields Watch needs.

## Login throttling

Failed sign-ins are tracked in memory per client. After 10 failures in 15
minutes, `POST /api/v1/auth/login` returns `429` with `Retry-After`. A successful
sign-in clears the counter. Counters reset on restart, which only makes the
limiter more permissive for one window.

Sessions are in memory and expire after seven days. A restart signs everyone
out. Cookies are `HttpOnly`, `SameSite=Lax`, and marked `Secure` when the
request arrived over HTTPS (including a trusted `X-Forwarded-Proto`).

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

## Client identity and proxies

When the immediate peer is loopback, private, link-local, or Tailscale
(`100.64.0.0/10`), the app trusts `X-Real-IP` first and the last
`X-Forwarded-For` entry second. This is the deployment shape used by the nginx
and SWAG configs in `deploy/nginx/`.

A direct public peer cannot choose its own bucket by sending forwarding
headers; the peer address is used instead. If you put a new proxy in front of
the app, make sure it overwrites `X-Real-IP` rather than appending to a
client-supplied value.

## Still open

- API tokens for non-browser clients (Basic auth is the current option).
- OIDC, roles, and an audit log.
- A formal SSRF/path/command review for cloud and peer URLs.
- Provider webhook signature verification and webhook rate limits.
- Secret rotation runbook and key-recovery policy.
- Optional LAN/Tailscale-only Studio exposure until SSO exists.
- Enforced concurrent MPEG-TS tuner limits (the advertised tuner count is not
  yet a hard cap).
