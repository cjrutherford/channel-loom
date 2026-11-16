# Phase 0 — Foundations and spikes

Status: technical spikes complete on x86_64; ARM and live client tune-in pending
Date: 2026-11-14
Environment: Ubuntu 26.04, 16 cores, 60 GB RAM, AMD RX 6600 + Radeon iGPU, FFmpeg 8.0.1, Rust 1.98.1, Docker 29.1.2

## 1. Deliverables

| Deliverable | Location | Status |
|---|---|---|
| Scope ADR | `docs/adr/0001-scope.md` | done |
| Stack ADR | `docs/adr/0002-stack.md` | done |
| Scheduler ADR | `docs/adr/0003-scheduler.md` | done |
| Engine ADR | `docs/adr/0004-engine.md` | done |
| Clean-room ADR | `docs/adr/0005-clean-room.md` | done |
| Data model/API ADR | `docs/adr/0006-data-model.md` | done |
| Parity checklist | `docs/PARITY.md` | done |
| Synthetic media fixtures | `scripts/gen-fixtures.sh`, `testdata/generated/` | done |
| Scheduler crate | `crates/awesometv-scheduler` | done |
| HLS engine spike | `crates/awesometv-engine-spike` | done |
| HDHomeRun spike | `crates/awesometv-hdhr-spike` | done |
| Provenance log | `docs/clean-room/provenance.md` | done |

## 2. Scheduler spike

The scheduler is a pure library with injected time, date, seed, and state. Phase 0 covers weekly dayparts, sequential/shuffle sources, one/count/duration fill modes, tail filler, fallback coverage, overnight dayparts, and deterministic state.

Tests: **8/8 pass**, including 50 randomized invariant runs.

```bash
cargo test -p awesometv-scheduler
cargo run -p awesometv-scheduler --bin week
```

Sample week result:

- 1,631 timeline items
- 28.9% program coverage (the rest is fallback filler, as intended)
- 0 warnings
- Deterministic for the same seed/state
- No overlaps or negative durations across all tested plans

Known Phase 0 limits, to address in Phase 1/3:

- Fixed UTC offset, not IANA time zones or DST.
- No linked slots, multi-part grouping, mid-roll, EPG grouping, or classic/block/sequential compilers yet.
- Filler is tail/fallback only; pre/post/mid-roll are next.
- Cursor state is keyed by slot name; production needs stable plan IDs and versioned state.

## 3. HLS engine spike

Three heterogeneous clips were normalized into one 1280x720 H.264/AAC HLS channel:

- 1080p H.264/AAC
- 720p HEVC/AC3
- 480p MPEG-2/MP2, 4:3

Software encode (`libx264`, veryfast, CRF 20):

| Metric | Value |
|---|---|
| First segment | 351–451 ms |
| Total for 30 s of content | 1.29–1.66 s |
| Segments | 8 |
| Playlist duration | 29.9966 s |
| Program date time | present |
| Discontinuity | present at start |
| Segment format | 1280x720 H.264, AAC stereo |

VAAPI encode (`h264_vaapi`, QP 20, `/dev/dri/renderD128`):

| Metric | Value |
|---|---|
| First segment | 603 ms |
| Total | 2.23 s |
| Segments | 8 |
| Playlist duration | 29.9966 s |

The VAAPI run is slower on this short, simple fixture because upload/filter overhead dominates; it is not a capacity benchmark. Hardware decode was separately validated on real 10-bit HEVC and 4K HEVC content.

Validation:

- HLS served over HTTP and decoded with `ffmpeg -i http://.../index.m3u8 -f null -` (exit 0).
- HLS playlist and segments returned HTTP 200.
- Concatenated segments probed as MPEG-TS, H.264/AAC, 1280x720, 12.03 s.

Real-library HLS run (8 seconds from each of The Birdcage, Overboard 4K, and Dispatches 10-bit HEVC):

| Encode | First segment | Total | Segments | Notes |
|---|---|---|---|---|
| Software | 952 ms | 4.43 s | 7 | PGS subtitle warning only; video/audio correct |
| VAAPI `/dev/dri/renderD128` | 1,052 ms | 3.11 s | 7 | Faster overall on real 4K/10-bit inputs |

This spike uses one concat pipeline for convenience. Production moves to per-item FFmpeg processes plus a segment stitcher (ADR 0004).

## 4. HDHomeRun spike

The spike serves:

- `/discover.json`
- `/lineup.json`
- `/lineup_status.json`
- `/device.xml`
- `/auto/v1.ts` and `/tuner0/v1.ts`
- `/healthz`

Validation:

- `discover.json` returns a stable DeviceID, BaseURL, LineupURL, and TunerCount.
- `lineup.json` returns GuideNumber, GuideName, and a stream URL.
- `/auto/v1.ts` returned HTTP 200, 3,456,192 bytes, `video/mp2t`.
- `ffprobe` on the served TS: MPEG-TS, H.264/AAC, 1280x720, 12.033 s.

Not yet validated against a real Plex/Emby/Channels instance. Jellyfin 12.1.0 is running at `http://192.168.1.153:8096`, but the API requires an admin key; tune-in is pending that key.

## 5. Real library probes

The share at `/mnt/valhalla/media` is reachable: 16 TB, 79% used; 77 movie folders, 122 TV show folders, 67 music artist folders. `commercials/` and `images/` are currently empty.

Sample probes:

| File | Result |
|---|---|
| `Movies/The Birdcage (1996)/...mkv` | HEVC Main, 1920x1040, AAC 5.1, 7,153 s, 1.66 GB (filename says H.264; it is HEVC) |
| `Movies/Overboard (2018)/...2160p...mkv` | HEVC Main, 3840x2160, E-AC-3 5.1, SubRip subtitle, 6,738 s, 11.75 GB |
| `Tv/Dispatches from Elsewhere/S01E01...mkv` | HEVC Main 10, 1920x1080, 10-bit, AAC 5.1, PGS subtitle, 3,100 s, 2.66 GB |
| `music/.../01-fall_out_boy-irresistible.flac` | FLAC audio |

These are useful fixtures for the scanner and pipeline: 10-bit HEVC, PGS subtitles, 4K, 5.1 audio, and mismatched filename/codec.

## 6. GPU validation

- VAAPI encode works on both `/dev/dri/renderD128` and `/dev/dri/renderD129`.
- VAAPI decode works for 1080p 10-bit HEVC and 4K HEVC.
- Software fallback works.
- QSV/NVENC/AMF/VideoToolbox/RKMPP are not available on this host and remain untested.

## 7. Phase 0 exit assessment

| Criterion | Status |
|---|---|
| ADRs and clean-room policy | met |
| Synthetic fixtures and test harness | met |
| Deterministic scheduler spike with property tests | met |
| HLS pipeline with heterogeneous clips | met |
| HDHomeRun protocol shape + TS serving | met |
| Real media probe | met |
| x86 hardware encode/decode | met |
| ARM cross-compile (scheduler/spikes) | met (`cargo check --target aarch64-unknown-linux-gnu`) |
| Real Plex/Jellyfin/Emby/Channels tune-in | **pending API key / servers** |
| ARM runtime/performance | **pending hardware** |

Recommendation: proceed to Phase 1 while scheduling the two outstanding validations. The architecture is accepted; the remaining gaps are access and hardware, not design failures.

## 8. Phase 1 starting point

1. Replace the single-process HLS spike with per-item producers and a segment stitcher.
2. Add IANA time zones/DST and linked slot state to the scheduler.
3. Add a catalog scanner with ffprobe/NFO parsing and a SQLite schema.
4. Build the first HTTP API and channel runtime.
5. Validate against Jellyfin with an admin API key; stand up Plex/Emby/Channels test instances.
6. Add an ARM runner (Raspberry Pi 5 or RK3588) to CI.

## 9. Reproduce

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# fixtures
scripts/gen-fixtures.sh testdata/generated --force

# scheduler
cargo test -p awesometv-scheduler
cargo run -p awesometv-scheduler --bin week

# HLS (software)
rm -rf target/hls-spike-sw
cargo run -p awesometv-engine-spike -- \
  --out target/hls-spike-sw \
  testdata/generated/clip_a_1080p_h264.mp4 \
  testdata/generated/clip_b_720p_hevc.mkv \
  testdata/generated/clip_c_480p_mpeg2.mpg

# HLS (VAAPI)
rm -rf target/hls-spike-vaapi
cargo run -p awesometv-engine-spike -- \
  --vaapi /dev/dri/renderD128 \
  --out target/hls-spike-vaapi \
  testdata/generated/clip_a_1080p_h264.mp4 \
  testdata/generated/clip_b_720p_hevc.mkv \
  testdata/generated/clip_c_480p_mpeg2.mpg

# HDHomeRun
cat target/hls-spike-sw/seg_0000{0,1,2}.ts > target/hls-spike-sw/channel.ts
cargo run -p awesometv-hdhr-spike -- \
  --port 18409 --ts target/hls-spike-sw/channel.ts
curl -s http://127.0.0.1:18409/discover.json
curl -s -o /tmp/channel.ts http://127.0.0.1:18409/auto/v1.ts
ffprobe /tmp/channel.ts
```
