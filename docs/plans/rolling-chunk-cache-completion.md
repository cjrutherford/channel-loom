# Rolling chunk cache: completion plan

Baseline commit: `425867d`. The feature is behind `AWESOMETV_PRECACHE_ROLLING_CHUNKS` (default off).

## Decisions (user, 2026-11-20)

- **Missing source:** if the source can't be found but its chunks are cached, air the cache until the end of that airing's slot, then drop the source.
- **Rollout:** turn it on for all channels at once, then run a short Chrome check.
- **Offline proof:** use isolated fixtures only. Never touch the production NAS or its mounts.
- **Acceptance:** multi-channel fill. The current and next programme must be covered on every eligible channel, including the source that fails today.

## Issue 1: verified identity is lost on restart

**Now:** `Precache::verified_sources` is an in-memory `HashMap<(media_id, path), SourceIdentity>`. After a restart with the source offline, `rolling_snapshot` skips the airing, and chunk expiry can then remove chunks for the airing that is on now.

**Fix:**
- Every chunk manifest already records `source_path`, `source_size` and `source_mtime`.
- At startup, after `ChunkIndex::open`, seed `verified_sources` from the manifests on disk. Map `source_path` to `media_id` through `media_items.path`.
- No new storage or schema change is needed.

**Policy:**
- A seeded identity is used only while its airing has not ended (`end_ms > now`). This is the "air to slot end" rule.
- Expiry keeps any chunk whose airing is still current.

**Tests:**
- Fix the stale assertion in `api::authenticated_rolling_coverage_survives_missing_origin`. The current policy keeps the airing, so the snapshot must be non-empty.
- Add a restart test: build the index, drop the `Precache`, delete the source, construct a fresh `Precache`, then check that the snapshot and coverage still show the current airing.
- Check that a finished airing with a missing source is not kept.

## Issue 2: the third source fails to remux

**Now:** `precache::tests::rolling_manager_background_publishes_three_channels_and_copies_music` fails twice. Two sources chunk; the third does not, and the error gives no reason.

**Steps:**
1. Capture the tail of ffmpeg's stderr (bounded, about 2 KB) in the chunk failure reason. Then rerun the test to get the real cause.
2. Fix the cause in `precache/chunks.rs`. Likely suspects:
   - the `RLIMIT_FSIZE` byte cap set too tight for the third file's bitrate;
   - the seek or offset for an airing with a non-zero `in_ms`;
   - a stream-mapping difference.
3. Make the reason visible on the coverage API, so production failures can be diagnosed.

## Issue 3: multi-channel fill gate

1. The full suite passes: `cargo test --bin awesometv-server`, 255 of 255, excluding ignored tests.
2. Run the isolated fixture fill with 3 or more channels plus one audio-only channel. Every eligible channel must have its current and next programme covered, and the audio-only channel must fall back to a whole-file copy.
3. Deploy with the flag on, using the existing image and override pattern.
4. Watch `GET /api/v1/precache/coverage` until current and next reach full coverage on all eligible channels.
5. Run a short Chrome check on Sci-Fi Theater plus two other channels: no stalls, and the playout label matches what is playing.

**Rollback:** remove the flag and recreate the container. Chunk files sit separately under `cache/chunks` and can stay in place.

## Out of scope (not asked for)

- A budget and eviction stress test against the whole-file cache.
- Proving the player is fed from chunks across a chunk boundary.
- Any real NAS offline drill.

## Constraints

- Never touch the NAS, its mounts, the protected inode 114980369, or job `be2684b5`.
- Never print `.agent/` snapshots or container env (they contain secrets).
- Do not commit until the gate passes and the user asks.

## Production findings (2026-11-20, evening)

- Done in code (uncommitted):
  - restart seeding of verified identities;
  - remux stderr in failure logs, and the keyframe probe fix;
  - packet-gap tolerance of 1 s, and source-tail tolerance of 2 s;
  - earliest-deadline-first planner;
  - escalating per-source backoff, from 60 s up to 30 min, with retry chunks halving down to 1 min;
  - a 10-minute lookahead plus the head of the next programme (user choice).
- Remux reads from the NAS ran at roughly 0.7–4 MB/s. One serial worker cannot keep all ten video channels ahead, even with the 10-minute window. In the last 10-minute run only 2 chunks were written.
- While the worker ran, the private publisher put out scheduled slate twice ("source unavailable", plus "source resolution slow"). Playback comes first, so the flag is back off in production.
- The gate is not met. Next step: the worker must yield to playback. It should not remux while a live source resolution is slow, and its own reads should be throttled. This is needed before any further fill attempt.
