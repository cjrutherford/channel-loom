# Peer-to-peer channel sourcing

Status: design proposal.

## Scope boundary

P2P channel sourcing answers: **can another ChannelLoom host share its live
channels and guide with this host?**

Cloud providers feed local media into the local library and scheduler. P2P does
not import remote media files or remote schedules. The remote host owns the
channel and its guide; the local host subscribes to the channel as a read-only
source.

## Goals

- Another ChannelLoom host can expose selected channels.
- A subscriber can add a peer, pair, browse shared channels, and subscribe.
- Subscribed channels appear in the local channel list, guide, M3U, and XMLTV.
- Live HLS plays through a local proxy.
- Remote guide data drives now/next and EPG.
- The peer can revoke access at any time.
- Default deployment works over Tailscale/LAN without a public hostname.

## Non-goals for the first milestone

- Public discovery/directory of peers.
- Sharing media libraries or local collections.
- Sharing admin APIs or schedules.
- Re-sharing a subscribed channel to another peer.
- Remote scheduling overrides.
- Catch-up/DVR from peers.
- NAT traversal beyond Tailscale/LAN.
- Central relay or SSAI.

## Terms

| Term | Meaning |
|---|---|
| Peer | Another ChannelLoom instance |
| Shared channel | A local channel a host exposes to peers |
| Grant | A scoped permission issued to a peer |
| Subscription | A local read-only channel sourced from a peer |
| Guide feed | Remote EPG entries for a shared channel |
| Live feed | Remote HLS playlist/segments |

## Architecture

```text
Host A (publisher)                         Host B (subscriber)
  local channel/guide                        peer directory
  peer API + shared channels                 subscription channels
  grant/token verification                   local guide cache
  HLS playlist/segments  ─── HTTPS ───►      signed proxy / player
                                             M3U + XMLTV + UI
```

- Tailscale or LAN direct HTTP is the default transport.
- The publisher exposes a versioned peer API.
- The subscriber polls the peer for channel list/guide and proxies HLS.
- The subscriber never receives publisher admin credentials.

## Identity and pairing

- Each host has a persistent peer ID and Ed25519/EdDSA key pair.
- `GET /.well-known/awesometv` returns:
  - peer ID
  - display name
  - protocol version
  - public key fingerprint
  - capabilities
- Pairing options:
  - one-time invite code displayed by publisher
  - subscriber submits code + its peer ID/public key
  - publisher approves and issues a scoped grant
  - optional manual approval in the publisher UI
- Grant token:
  - scoped to `channels:list`, `guide:read`, `stream:read`
  - expiring, revocable
  - stored hashed at the publisher, encrypted at the subscriber
- No transitive sharing: a subscriber cannot re-share a remote channel.

## Protocol (v1)

Publisher endpoints:

```text
GET  /.well-known/awesometv
POST /api/peer/v1/pair
GET  /api/peer/v1/channels
GET  /api/peer/v1/channels/{id}
GET  /api/peer/v1/channels/{id}/guide?from=...&to=...
GET  /api/peer/v1/channels/{id}/live.m3u8
GET  /api/peer/v1/channels/{id}/segments/{file}
GET  /api/peer/v1/health
```

- `Authorization: Bearer <grant>` for peer APIs.
- Live playlist returns short-lived segment URLs signed for the grant.
- Segment URLs are scoped to the grant and expire.
- All responses include protocol version and ETag/cache hints.
- The subscriber validates content types and path names; no arbitrary URLs from
  peer payloads are fetched (SSRF protection).

## Data model

### Publisher

```text
peer_identity(id, name, public_key, created_at)
peer_grants(id, peer_id, token_hash, scopes, expires_at, revoked_at)
peer_shared_channels(id, channel_id, slug, enabled, description)
peer_pairing_codes(code_hash, expires_at, used_at)
```

### Subscriber

```text
peers(id, name, base_url, peer_id, public_key, status, last_seen_at)
peer_subscriptions(
  id, peer_id, remote_channel_id, remote_slug,
  local_number, name, enabled, health, last_error
)
peer_guide_cache(subscription_id, from_ms, to_ms, payload, fetched_at)
peer_stream_tokens(subscription_id, token, expires_at)
```

Remote channels are marked as `source = remote_peer` in the channel model so the
local scheduler never tries to program them.

## Local behavior

- Subscribed channel appears in the local channel rail and guide.
- Channel number is assigned locally; remote ID stays stable.
- Local scheduler and collections cannot add remote channels.
- Guide is fetched and cached from the peer; stale guide shows the last known
  now/next with a stale marker.
- Live playback proxies the peer HLS through the local streaming proxy.
- If the peer is offline, the channel shows off-air/slate and a health error.
- M3U and XMLTV include subscribed channels.
- No remote library items appear in Library or Collections.

## UI

### Publisher

- **Peers** page:
  - identity/fingerprint
  - pairing code
  - pending/approved peers
  - grants and scopes
  - per-grant revoke
  - shared channel checkboxes
  - health of connected subscribers

### Subscriber

- **Peers** page:
  - add peer by URL
  - pairing flow
  - shared channel list
  - subscribe/unsubscribe
  - local channel number mapping
  - guide freshness and health
- Channels show a **Remote** badge and peer name.
- Guide shows remote EPG entries.
- Settings shows peer protocol version and connectivity.

## Security

- Peer API is separate from admin API.
- Pairing codes are one-time and short-lived.
- Grants are scoped, expiring, and revocable.
- Tokens are stored hashed at the publisher and encrypted at the subscriber.
- Rate limits per peer, grant, and IP.
- HLS segment URLs are signed and expire.
- Reject peer payload URLs outside the configured peer base.
- No library, schedule, settings, or backup data is exposed.
- Publisher controls which channels are shared.
- Public exposure, if enabled later, requires the admin/public route split and a
  dedicated public hostname.

## Phases

### P0 — Identity and manual peer add

- local peer identity/key pair
- well-known endpoint
- peers page and manual peer URL
- health/fingerprint display
- no pairing yet

Exit: two instances can identify each other and show fingerprints.

### P1 — Pairing and shared channels

- invite codes
- scoped grants and revocation
- publisher shared-channel selection
- subscriber channel list and subscription
- no playback yet

Exit: subscriber can pair, list shared channels, subscribe, and see access
revoked.

### P2 — Guide and live playback

- remote guide fetch/cache
- now/next and XMLTV/M3U integration
- signed segment URLs
- local proxy playback
- offline/slate behavior

Exit: a subscribed remote channel plays live and appears correctly in the guide
and M3U/XMLTV.

### P3 — Hardening and optional expansion

- multi-peer scaling, retries, health metrics
- local channel numbering conflicts
- optional catch-up/DVR if the publisher exposes it
- optional public discovery/relay
- optional mutual TLS / stronger identity

Exit: multiple peers stable; revocation and failure cases verified.

## Acceptance criteria

- Two ChannelLoom instances on a tailnet can pair.
- Publisher shares one channel; subscriber sees it in guide and M3U/XMLTV.
- Live playback works through the subscriber’s proxy.
- Revoking the grant stops the subscriber within the token expiry window.
- Peer offline produces off-air/slate, not a local scheduler failure.
- No admin endpoint or library data is reachable through the peer API.
- Guide and stream payloads are size-limited and rate-limited.
- Re-sharing a remote channel is impossible.

## Open decisions

1. Discovery: manual URL only, Tailscale peer discovery, or central directory.
2. Pairing: invite code, mutual approval, or both.
3. Share scope: live + guide only, or catch-up/DVR later.
4. Transport: direct HTTP, mutual TLS, or an optional relay.
5. Remote channel scheduling: strictly read-only, or allow local overrides.
6. Numbering: auto-assign or manual mapping.
7. Public exposure: tailnet-only first, or public hostname from day one.
8. Re-sharing: always forbidden (recommended), or an explicit publisher opt-in.

## Recommended first milestone

- Tailscale/LAN direct HTTP.
- Manual peer URL + one-time invite code.
- Publisher-selected channels.
- Read-only subscriptions.
- Live HLS proxy + guide/M3U/XMLTV.
- No catch-up, DVR, discovery, relay, or re-sharing.

This is enough to prove the feature without committing to public federation or
NAT traversal.
