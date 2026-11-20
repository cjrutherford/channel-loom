# Fresh deployment attempt — 2026-11-19

Outcome: **rolled back; fresh deployment blocked by playback runtime failures**. The running service is the previous image, not the new build. No application source was changed during this deployment attempt.

The local Compose service is `awesome-tv/awesometv`, container `awesometv`, host network, Tailscale address `100.120.253.100:8409`. DNS for `atv.christopherrutherford.net` resolves to `45.79.204.77`; the repository's SWAG config identifies its upstream as this machine. Before and after deployment, local and public channel and guide API responses matched each other. Data is bind-mounted at `data:/data` and media at `/mnt/valhalla/media:/media:ro`.

- Previous and restored image: `sha256:855a960806117dbb133bd28d4d4e84934f23e6b5de430fe961fc6db88c6ea3e3`, retained as `awesometv:rollback-20261119`.
- Fresh image: `sha256:c675199826925ac34698006d661287574e6b642c3aef5325bee081a0b0e90dad`, tagged `awesometv:deployment-20261119-jv2ckk`. Release Docker build passed without compiler warnings. Base revision `f486be4fafdf3c8711385852ab6fcf72b988fb64`; copied build inputs, including untracked source, have manifest SHA-256 `c5803e5adf295fae1b626e31ce96eafe463df7230af359b46da5e6ee09b9ef87`. Image labels record revision, manifest and snapshot time. The full snapshot, file manifest and build log are in the protected evidence directory below.
- Evidence: `/home/cjrutherford/.t3/soak-evidence/awesome-tv/deployment-20261119-jV2CkK`. An online SQLite backup passed `integrity_check`; database, secrets directory and Compose environment copy are owner-only under `protected/`. Rollback image override is `rollback.yml`; new image override is `deploy.yml`. The current database was not restored, preserving writes made during the attempt.
- At 13:02:51 UTC the fresh image started; local and public `/healthz` returned 200, public channels and guide each returned 12 entries, SQLite integrity was `ok`, and counts remained 12 channels, 10,633 media items and 6,990 channel items. Before/after API payloads were not byte-identical; their contents are retained for inspection and should not be described as identical across time.
- The fresh runtime logged repeated `private channel runtime failed: a character literal was not valid`, followed by publisher backoff on channels 1 and 4. It was rolled back at 13:03:19 UTC. A likely upgrade incompatibility is the new `Publisher::resume` strict RFC 3339 parser in `engine/private_publisher.rs`: existing FFmpeg playlists contain offsets such as `+0000`, while the existing engine normalizes these to `+00:00` before parsing. The precise failing line was not instrumented, so this is a strong hypothesis, not a proven stack trace. Raw runtime log is protected in the evidence directory.
- After rollback, local and public health returned 200. In the public Watch browser, video was unpaused and readyState 4; over six seconds currentTime advanced from 83.442 to 89.442 seconds and decoded frames from 2743 to 2923 at 1280 pixels wide. The browser tab was closed. This proves short playback on the restored image only, not the fresh image. No hour soak was run.

To restore the retained image if needed, from the repository root run:

```sh
docker compose -f docker-compose.yml -f /home/cjrutherford/.t3/soak-evidence/awesome-tv/deployment-20261119-jV2CkK/rollback.yml up -d --no-deps --no-build awesometv
```

The SQLite backup and protected key/config copies are a point-in-time recovery set; restoring them would discard subsequent writes and requires stopping the service and a deliberate recovery procedure. Next: fix and test legacy playlist timestamp compatibility, rebuild from the intended tree, redeploy, verify public build identity and actual playback, then run the longer soak.

## Follow-up investigation

The retained `rollback-playlist.m3u8`, captured after rollback, contains `2026-11-19T13:03:26.253+0000`. The saved runtime log reports `a character literal was not valid` but does not identify the input timestamp, so the exact timestamp consumed in the failed fresh run cannot be established from that log. Current old-image channel playlists also use `+0000` and legacy `seg_...ts` filenames, and omit `#EXT-X-DISCONTINUITY-SEQUENCE`. The new `Publisher::resume` requires `segment_<uuid>.ts` and that tag, so normalizing the timestamp alone would expose further incompatibilities. The retained rollback playlist is a public playlist whose segment paths include a channel prefix; it is not a direct copy of the publisher's on-disk input. No redeployment or source fix was attempted in this follow-up. The previous image remains running. A safe migration must cover the actual on-disk legacy playlist format and continuity as well as timestamps, with a representative retained fixture and runtime checks before replacement.


## Legacy resume adapter (local verification)

The resume adapter accepts FFmpeg numeric UTC offsets and RFC 3339, validates legacy segment basenames, and retains advertised filenames and dates. Missing discontinuity sequence uses the HLS zero default; a new encoder starts at an explicit discontinuity. The first append preserves the entire retained window, and later appends gradually reduce an oversized window without deleting legacy files. Unknown earlier discontinuity history is not reconstructed. The regression embeds the captured rollback HTTP playlist with only its `1/` channel URI prefix removed to represent disk names; the capture is not itself an on-disk snapshot. No image, deployment, service restart, or live playback validation was performed in this slice.

Next deployment scope: independently inspect the actual retained on-disk playlists and files, then build the intended image, deploy with rollback available, verify build identity and uninterrupted public playback/sequence/PDT across restart, and run the requested soak.
