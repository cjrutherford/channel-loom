# Cloud providers and P2P channels: sequencing

Two separate tracks share infrastructure but deliver different capabilities.

| Track | Question | Output |
|---|---|---|
| Cloud providers | What media files may this host index and play? | Remote files become local library items |
| P2P channels | What channels and guides may another host share? | Remote channels become local read-only channels |

## Shared infrastructure

Both tracks depend on:

- signed, expiring streaming URLs
- Range-capable byte proxy
- secret encryption / key management
- rate limiting and metrics
- public/admin route separation when exposed publicly
- request timeouts/retries
- health status in the UI

## Dependency graph

```text
C0 credentials ──► C1 cloud core hardening ──► C2 OAuth/webhooks ──► C3 adapters ──► C4 acceptance
                         │
                         ├──► P0 peer identity
                         │
                         └──► P1 pairing/grants ──► P2 guide/live ──► P3 hardening
```

C2 and P1/P2 both need the admin/public split if public exposure is enabled.
Tailnet-only P2P does not.

## Recommended order

### Now

1. C0: create a real Google service account and test folder.
2. Validate Google list, probe, playback, and cache against the real API.
3. C1: harden the shared core and finish source edit/cache/sync behavior.
4. Decide whether the next deployment has a public hostname.

### Then

5. If public hostname exists: C2 OAuth and webhooks, plus admin/public split.
6. If not: continue C1 and start P0/P1 P2P over Tailscale.
7. C3 adapters in parallel once the trait and conformance tests are stable.
8. P2 guide/live after signed proxy and route separation are stable.
9. C4 and P3 acceptance.

## Milestones

| Milestone | Exit criteria |
|---|---|
| Cloud C0 | Real service account can list/download a test folder |
| Cloud C1 | Source lifecycle, background scans, delta reconciliation, cache pins, source badges |
| Cloud C2 | Browser OAuth and signed webhooks work behind a public HTTPS origin |
| Cloud C3 | Google, Dropbox, OneDrive, S3, WebDAV pass the same conformance suite |
| Cloud C4 | Real-provider smoke tests, security review, docs, performance |
| P2P P0 | Two hosts identify each other and show fingerprints |
| P2P P1 | Pairing, scoped grants, shared channel selection, subscribe/revoke |
| P2P P2 | Remote guide, M3U/XMLTV, live HLS proxy, offline/slate behavior |
| P2P P3 | Multi-peer stability, health, optional catch-up/DVR/discovery |

## Decisions needed before implementation continues

### Cloud

- Public hostname and OAuth redirect URI.
- Real Google service account and test folder.
- Whether to create Dropbox/OneDrive apps now or after Google validation.
- S3/WebDAV test endpoints.
- Cache budget and secret-key backup policy.
- Admin/public route split timing.

### P2P

- Discovery: manual URL, tailnet discovery, or directory.
- Pairing: invite code, mutual approval, or both.
- Share scope: live/guide only or catch-up/DVR.
- Transport: direct HTTP, mutual TLS, or relay.
- Remote channel scheduling: strictly read-only or local overrides.
- Numbering: automatic or manual.
- Public exposure timing.
- Re-sharing: forbid (recommended) or publisher opt-in.

## What to accomplish before moving forward

1. Validate the Google service-account path against real Google Drive.
2. Decide the public hostname, or explicitly defer public OAuth/webhooks.
3. Choose whether cloud adapters or P2P pairing is the next implementation
   milestone.
4. Define the P2P trust/share defaults above.
5. Provision test assets: Google test folder, MinIO, local WebDAV, Dropbox and
   OneDrive apps when their adapters begin.
6. Decide cache budget and secret-key recovery procedure.
