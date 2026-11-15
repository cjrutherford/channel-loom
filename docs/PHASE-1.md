# Phase 1 — First vertical slice

Status: running on the Tailscale address; local scanning, one-channel HLS, M3U, XMLTV, and the broadcast-control UI are working. Phase 2 groundwork is underway.
Date: 2026-11-14

## What is implemented

- Rust `awesometv-server` (axum + SQLite) with:
  - local media scanning via `ffprobe` (movies/episodes/other videos)
  - stable media IDs from file paths
  - a seven-day channel timeline built by looping selected media
  - live HLS output through FFmpeg
  - M3U and XMLTV endpoints
  - basic-auth admin UI
  - Tailscale-only default bind
- SQLite catalog and channel tables (`media_items`, `channels`, `channel_items`).
- Web UI: scan a folder, select media, create a channel, watch it in the browser with HLS.js.

## Serve only on Tailscale

Default bind is the machine's Tailscale address:

```text
AWESOMETV_BIND=100.120.253.100
AWESOMETV_PORT=8409
AWESOMETV_PUBLIC_URL=http://100.120.253.100:8409
```

The server refuses to start if that address is unavailable. Docker Compose uses `network_mode: host` and the same bind, so it is not exposed on the LAN or public interfaces.

## Run locally

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export AWESOMETV_ADMIN_PASSWORD=change-me
export AWESOMETV_DATA_DIR="$PWD/data"
cargo run -p awesometv-server
```

Open `http://100.120.253.100:8409/` and sign in as `admin` with the configured password.

## Docker

```bash
cp .env.example .env
# edit .env and set AWESOMETV_ADMIN_PASSWORD
docker compose up -d --build
```

The compose file mounts `/mnt/valhalla/media` read-only at `/media`.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | health |
| `GET /` | admin UI (basic auth) |
| `POST /api/v1/scan` | scan a local path (basic auth) |
| `GET /api/v1/media` | catalog (basic auth) |
| `POST /api/v1/channels` | create a channel from media IDs (basic auth) |
| `GET /api/v1/channels` | list channels (basic auth) |
| `GET /api/v1/channels/{id}/now` | current item |
| `GET /api/v1/channels/{id}/guide?hours=12` | guide |
| `GET /api/v1/config` | public URL + Jellyfin URL |
| `GET /iptv/channels.m3u` | IPTV playlist |
| `GET /iptv/xmltv.xml` | XMLTV guide |
| `GET /channel/{number}.m3u8` | HLS playlist |
| `GET /channel/{number}/{segment}.ts` | HLS segment |

Jellyfin is configured as `https://jelly.christopherrutherford.net` via `JELLYFIN_URL`.

## Verified

- Server bound to `100.120.253.100:8409`; `/healthz` returned 200.
- UI returned 401 without credentials and 200 with basic auth.
- Scanned 5 real movies from `/mnt/valhalla/media/Movies` and 4 synthetic fixtures.
- Created a channel from three heterogeneous clips.
- HLS playlist returned 200; segments served as `video/mp2t`.
- `ffmpeg -i http://100.120.253.100:8409/channel/1.m3u8 -t 6 -f null -` exited 0.
- M3U and XMLTV returned valid shapes; XMLTV times are UTC and match the channel timeline.
- HLS segment probed as 1280x720 H.264/AAC.

## Known Phase 1 limits

- The engine runs one FFmpeg process for the current item and starts the next item on the next playlist request. Program boundaries are not seamless yet.
- Tune-in takes about 7 seconds for the first segment in the software path; needs prefetch/hardware tuning.
- Titles are filename-derived; NFO/metadata parsing is not wired yet.
- No Jellyfin source yet; only the base URL is configured. Plex/Emby sources are next.
- Collections, smart collections, and daypart scheduling are not exposed in the UI yet.
- Basic auth only; no sessions/OIDC.
- SQLite schema is intentionally small; migrations are inline for Phase 1.
- Docker image built successfully (`awesometv:phase1`) and ran with host networking on the Tailscale address; `/healthz` returned 200. Multi-arch image builds are not wired yet.
- The full server does not cross-check for ARM on this host yet: bundled SQLite needs `aarch64-linux-gnu-gcc`, and the host has no cross toolchain/sudo. Scheduler and spikes do cross-check. Multi-arch Docker builds should use buildx/QEMU or an ARM runner.
- HLS producer state is in memory; restarting the server restarts the current item.

## Next

1. Wire the Jellyfin source against `https://jelly.christopherrutherford.net` (API key required).
2. Replace the one-item producer with a prefetch/next-item pipeline and segment cache.
3. Add NFO parsing, poster artwork, and media-kind detection (movie vs episode).
4. Add collections and a daypart editor backed by `awesometv-scheduler`.
5. Add proper migrations, sessions, and a Docker image build in CI.


## Phase 2 groundwork started

- Canonical source roots are locked in `docs/SOURCES.md`: Movies + TV, plus music/images, ignoring `.1` duplicates and download staging.
- The UI is rebuilt as a broadcast control surface: sidebar, on-air monitor, channel rail, library table, guide, sources, and settings.
- Scans now carry media kinds (`movie`, `episode`, `music`, `image`, `other`).
- NFO metadata is used for titles before folder/filename fallback.
- `/api/v1/sources` and `/api/v1/stats` support the new UI.
- Jellyfin is configured as `https://jelly.christopherrutherford.net`; API sync is gated on `JELLYFIN_API_KEY`.


## Phase 2 Jellyfin sync (verified)

- Jellyfin API key is configured; authentication uses `Authorization: MediaBrowser Token=<key>`.
- Jellyfin reports 10,417 movie/episode items. The sync imported **10,322** (68 movies, 10,254 episodes) and skipped **95** live-TV recordings under `/config/data/livetv/recordings`, which are outside the selected library.
- Path mapping: `/data/movies` → `/media/Movies`, `/data/tvshows` → `/media/Tv`, `/data/music` → `/media/music`.
- Import uses Jellyfin names, series/season/episode numbers, durations, resolution, and codecs; no per-file ffprobe pass.
- UI now exposes a Jellyfin **Sync** button and reports imported/skipped/error counts.


## Phase 2 engine and client output (verified)

- A per-channel supervisor now keeps the HLS producer alive across item boundaries, so the next item starts without waiting for a client playlist request.
- HDHomeRun endpoints are served at `/discover.json`, `/lineup.json`, `/lineup_status.json`, `/device.xml`, `/auto/v{number}.ts`, and `/tuner/{n}/v{number}.ts`.
- `/auto/v1.ts` returned HTTP 200, `video/mp2t`; ffprobe reported H.264/AAC 720p with a ~32 second stream during a 30 second request.
- The Docker runtime ships FFmpeg 5.1, which does not support `-readrate_initial_burst`; the producer uses the supported `-readrate` option. Bundling a newer FFmpeg is still on the list for faster startup.
- `HEVC` PGS subtitle warnings from 2001's remux are benign for playout; subtitles are not burned in yet.


## Phase 2 collections and dayparts (implemented)

- Collections are named, reusable media sets with ordered items.
- Weekly programming blocks store a day mask, local start/end minutes, collection, and sequential/shuffle order.
- Saving blocks rebuilds the next seven days through `awesometv-scheduler` and replaces the channel timeline.
- The UI now has Collections and Programming views; the Library can create a collection from selected media, and Programming can add/remove blocks and rebuild.
- Verified: a six-episode Adventure Time collection with a full-day, seven-day block rebuilt to 847 timeline items and became the current channel lineup.


## Phase 2 sessions and backups (verified)

- Browser auth is now a session flow: `POST /api/v1/auth/login` sets an HttpOnly, SameSite=Lax `awesometv_session` cookie; the UI shows a sign-in screen and a Sign out control.
- Basic auth remains supported for API clients and automation.
- Sessions are in memory with a seven-day expiry; restarting the server requires signing in again.
- Backups use SQLite `VACUUM INTO` and are listed, downloadable, and restorable from Settings.
- Restore is staged as `data/restore.pending`; the next startup preserves the old database as `awesometv.pre-restore.<timestamp>.db` and applies the staged snapshot.
- Verified: login, cookie-authenticated stats, backup creation, download listing, restore staging, restart, and post-restore catalog integrity.


## Phase 3 first objectives

- Classic ordered sequences: one/multiple/duration modes, sequential/shuffle order, seven-day loop, and a Blocks/Classic switch.
- Weekly blocks now accept an optional filler collection. Filler is used for tail/gap filling through the scheduler; a short commercial library will produce tight breaks without adding long program items.
- Generated slate fallback covers gaps where the timeline has no current item.


## Phase 3 mid-roll breaks (verified)

- Classic items accept a break filler collection, a mid-roll interval in minutes, and a break length in seconds.
- Timelines now persist `in_ms`, `out_ms`, and `kind` per item. The engine passes the trimmed window to FFmpeg as `-ss` and `-t`.
- Classic rebuilding splits each program at the interval, inserts only as much filler as the break allows, trims a longer filler to the break length, and keeps program continuity with the next segment's in point.
- Weekly-block timelines use full-item in/out windows; the same persistence path serves both schedulers.
- Verified on Classic Adventure with a five-minute interval and 20-second breaks: 4,015 parts (2,409 program segments, 1,606 filler breaks), no gaps or overlaps, and filler `out_ms` exactly 20,000.
- Verified at runtime: HLS produced a GUNDAM bumper break, transitioned after ~19 seconds of break content, and started the next program segment with `-ss 304.805` and `-t 295.195` in the FFmpeg process.
- Unit tests cover segment splitting, filler trimming, no-filler behavior, and trim-window math.


## Phase 3 chapter-aware breaks (verified)

- `ffprobe -show_chapters` now runs during scans and stores chapter markers in `media_chapters` with a `chapters_probed` flag per media item.
- `POST /api/v1/media/probe-chapters` probes a collection or a batch of unprobed media. Collections have a **Chapters** action in the UI.
- Classic items have a **Snap breaks to chapters** option, enabled by default.
- Break placement walks the mid-roll interval and snaps each target to the nearest chapter start within one-third of the interval; otherwise it falls back to the fixed interval.
- Snap targets advance from the chosen chapter, so chapter drift does not accumulate across a long program.
- Verified on **Chaptered Adventure**: four episodes with 16 chapter markers. A ten-minute interval snapped breaks to the 671–673 second chapter starts instead of 600 seconds.
- Runtime verification: the post-break segment started with `in_ms=671754` and the FFmpeg process used `-ss 678.353 -t 27.715`.
- Unit tests cover chapter snapping, fixed-interval fallback, and segmented program boundaries.


## Phase 3 block templates (verified)

- Added reusable weekly block templates: `schedule_templates` and `schedule_template_blocks`.
- Templates store day masks, time windows, collection, optional filler collection, and order for each block.
- API: list, create, read, delete templates, plus apply-to-channel. Applying replaces the channel's weekly blocks, switches it to blocks mode, and rebuilds the seven-day timeline.
- Programming view has a **Block templates** panel: save the current blocks under a name, then Apply or Delete from any channel.
- Deleting a collection also removes template blocks that referenced it.
- Verified with two templates: **Adventure Time Original** (1 block) and **Movie Night Daily** (2 blocks). Applying Movie Night Daily to channel 1 replaced its blocks and rebuilt 75 timeline items.
- Channel 1 now runs a two-block daily movie schedule; templates remain available in the UI.


## Phase 3 scripted YAML schedules (verified)

- Added `serde_yaml` and a scripted schedule format covering classic items and weekly blocks.
- `GET /api/v1/channels/{id}/schedule.yaml` exports the active schedule with collection names.
- `POST /api/v1/channels/{id}/schedule.yaml` parses YAML, resolves collections by name or id, validates days/times/orders, applies the schedule, and rebuilds.
- Classic items support filler collections, mid-roll intervals, break lengths, and chapter snapping in YAML.
- Weekly blocks accept `daily` or day lists plus `HH:MM` start/end times.
- Programming has a **Scripted YAML** tab with Load current and Apply YAML.
- Verified round-trip export/import on Classic Adventure (2,496 items) and a two-block weekday/weekend schedule on Movie Night (410 items).
- Invalid YAML returns a 400 with the parser or validation message.

## Phase 3 channel graphics and stream profiles (verified)

- Channels now store a text bug: text, position (four corners), opacity, and font size.
- `GET/PUT /api/v1/channels/{id}/graphics` manages the overlay. Settings apply immediately by restarting the channel producer at its current position.
- Block templates capture graphics settings and reapply them with their blocks.
- Channels now store a stream profile: width/height, x264 preset, CRF, and audio bitrate.
- `GET/PUT /api/v1/channels/{id}/stream` manages the profile; the engine uses it for scale/pad, preset, CRF, and audio bitrate.
- Verified a 854x480 / fast / CRF 26 / 128 kbps profile: the FFmpeg process contained `scale=854:480`, `-preset fast`, `-crf 26`, and `-b:a 128k`; the HLS segment probed as 854x480.
- Verified the graphics overlay in a captured frame: `AWESOME TV` rendered bottom-right at the configured 55% opacity.
- Verified template graphics transfer: applying **Movie Night Branded** moved the `MOVIE NIGHT` top-right bug to Adventure Time, then applying **Adventure Time Original** cleared it again.
- Programming now has Channel graphics and Channel stream panels.


## Phase 3 image watermarks and lower-third decos (verified)

- Graphics settings now include an image watermark and a lower-third text band, alongside the existing corner text bug.
- Image watermarks are selected from catalog images and support position, opacity, and width percentage.
- Lower thirds support text, size, and opacity.
- Graphics are captured in block templates and reapplied with the blocks.
- Engine filtergraph composes lower third, corner text, and `movie=` image overlay in one `-vf` chain.
- Verified with a generated `atv-logo` catalog image: an HLS frame showed the ATV watermark top-left, `Classic Adventure` lower-left, and `AWESOME TV` bottom-right in the same frame.
- The FFmpeg process contained the expected `drawtext` and `movie=...overlay` filters at 1280x720.

## Phase 3 external playout JSON (verified)

- Added `GET /api/v1/channels/{id}/playout.json` to export the materialized timeline as media references, in-points, durations, and item types.
- Added `POST /api/v1/channels/{id}/playout.json` to import an external timeline. Items resolve by media id or exact title, support in/duration seconds and program/filler types, and can loop for seven days.
- Imported channels switch to `external` mode. Rebuild requests preserve the imported timeline instead of replacing it.
- Verified Classic Adventure export: 2,496 items with RFC3339 start and media references.
- Verified a looping two-item external playout: 15,120 items covering seven days, a live current item, and a rebuild that returned the same count.
- Verified invalid playout JSON returns 400, and an external channel was restored to its block template afterwards.
- Programming now has an External JSON tab with Load current and Apply JSON.


## Phase 3 manual break markers (verified)

- Classic items accept explicit `break_markers` in seconds; YAML also accepts `mm:ss` and `hh:mm:ss` strings.
- Manual markers take precedence over interval/chapter placement for that item and are sorted, de-duplicated, and clipped to the media duration.
- Markers require a filler collection and a positive break length; invalid combinations are rejected at save time.
- Classic editor has a "Manual break markers" field and shows "manual breaks at ..." on the sequence row.
- YAML export/import round-trips markers as seconds.
- Verified with markers `[300, "10:00"]`: program segments ended at 300 and 600 seconds with 20-second fillers between them, producing 2,430 program and 1,620 filler parts.
- Verified YAML export produced `break_markers: [300, 600]`, and invalid marker-only YAML without filler returned 400.
- Restored Classic Adventure to its ten-minute chapter-snapped schedule afterward.


## Phase 3 music and image playout (verified)

- The producer detects media kind from the catalog and uses different FFmpeg paths:
  - video: video + audio encode
  - music: audio-only HLS with `-vn` and an explicit audio map
  - image: `-loop 1 -framerate 30` video-only HLS with `-an`
- Verified a FLAC track: the HLS segment probed as **AAC audio only** and the process used `-vn`.
- Verified an image item: the HLS segment probed as **H.264 1280x720 video only** and the process used `-loop 1`.
- Verified an animated GIF watermark overlays and animates on top of a static image source.

## Phase 3 stream selection, subtitles, HDR, and VAAPI (verified)

- Channel stream profiles now include audio language, subtitle mode/language, HDR mode, and encoder.
- The engine probes each media item for audio/subtitle streams and HDR transfer metadata.
- Audio selection maps the preferred language, falling back to the first audio track.
- Subtitle burn-in extracts a text subtitle stream into a cached SRT in the background, shifts cue timestamps to the trim window, and renders it through libass.
- HDR tonemapping uses zscale/tonemap and is applied when the source is HDR and the channel requests tonemap.
- VAAPI encoding uses `/dev/dri/renderD128`, uploads software-filtered frames, and encodes with `h264_vaapi`.
- Verified:
  - Stargate Universe subtitles burned into an HLS frame (`Move, move! ...`).
  - 2001: A Space Odyssey HDR path produced the zscale/tonemap filter and selected the intended audio stream.
  - VAAPI HLS segment probed as H.264 1280x720 with `h264_vaapi` in the process.
- Docker Compose now passes `/dev/dri` into the container.

## Phase 3 alternate schedules and animated decos (verified)

- Added date-range schedule overrides with priority. Overrides store YAML and are mixed per day with the base schedule.
- Verified a two-day blocks override on Movie Night: both override days rendered Adventure Time while later days stayed on the base movie schedule.
- Verified a one-day classic override on Adventure Time: the override day rendered movies, the next day returned to the base schedule.
- Deleting an override restored the base schedule.
- Animated decos use a looped second input and `filter_complex` overlay; verified an animated GIF watermark produced changing frames over a static image source.
- Added Emby source configuration and sync endpoint mirroring Jellyfin; no live Emby instance is configured, so the endpoint currently returns a clear 400.


## Phase 3 channel lineup buildout

- Expanded the catalog by scanning 300 music tracks and grouping all 114 episode shows.
- Replaced the small demo collections: Movie Night now holds all 76 movies and Adventure Time all 278 episodes.
- Created over 70 collections covering animation, sci-fi, sitcoms, drama, reality, Gundam, family movies, and music.
- Built 12 live channels with classic schedules:
  - Movie Night (4 movies/cycle, 30-minute chapter breaks, HDR tonemap)
  - Animation Station (nine Western animation series, 2 episodes each)
  - Adventure Time (dedicated)
  - Sci-Fi Theater (twelve sci-fi series)
  - Doctor Who (dedicated, 20-minute chapter breaks)
  - Sitcom Central (twelve sitcoms)
  - One Piece (dedicated)
  - Reality Rundown (RuPaul franchise and reality shows)
  - Drama After Dark (fourteen dramas)
  - Family Movie Night (2 movies/cycle)
  - Music Mix (12 tracks/cycle, audio-only)
  - Gundam Channel (thirteen Gundam series)
- Added per-channel text bugs; Doctor Who retains the ATV watermark and lower third.
- Movie channels use HDR tonemapping; all channels default to 720p software encoding.
- Added Holiday Movies override on Movie Night for December 24–26.
- Fixed channel ordering to sort numerically (1..12) instead of lexicographically.
- Verified all 12 channels are live, expose 12 M3U/XMLTV entries, and rebuild with zero warnings.


## Phase 4 stream tuning and audio-language verification

- Added per-channel `segment_seconds`, `prebuffer_seconds`, and `cache_segments` stream settings.
- The engine now uses these values for HLS segment length, initial read-rate burst, keyframe cadence, and playlist window.
- Tune-in waits for enough segments to cover the prebuffer instead of returning an empty playlist.
- Added producer metrics at `/api/v1/metrics` and Prometheus text at `/metrics`; Settings shows startup latency, segment counts, and HLS disk usage.
- Fixed the supervisor to re-read channel settings from the database before each producer start so audio/stream changes apply immediately.
- Audio selection now:
  - normalizes ISO codes and English language names (`eng`, `en`, `English`, `en-US`)
  - prefers the default matching track
  - skips commentary tracks
  - falls back to the first non-commentary track when the requested language is absent
- All 12 channels are configured to prefer **English** audio.
- Verified with `Doctor Who — S19E08`, whose first audio track is German and second is English: the producer mapped `0:2` (English), not the first track.
- Verified HLS with 2-second segments, 6-second prebuffer, and an 8-segment cache: the playlist, FFmpeg args, startup metrics, and Prometheus output all reflected the settings. Channel 5 was restored to 4-second segments and defaults afterward.


## Phase 4 global language defaults

- Added persisted application language settings:
  - default audio language
  - default subtitle mode
  - default subtitle language
- Environment defaults seed the database on first start (`AWESOMETV_AUDIO_LANGUAGE`, `AWESOMETV_SUBTITLE_MODE`, `AWESOMETV_SUBTITLE_LANGUAGE`).
- Channels inherit global language settings when their own audio/subtitle fields are empty or set to `inherit`.
- Per-channel overrides still win.
- Settings view has a **Language defaults** panel; saving defaults restarts producers so changes apply immediately.
- The channel stream editor now exposes `Inherit global` for subtitles and shows inherited values in placeholders.
- Verified global switching on `Doctor Who`:
  - global English + inherited channel -> `-map 0:2` (English)
  - global German + inherited channel -> `-map 0:1` (German)
  - restored global English -> `-map 0:2`
- Verified subtitle inheritance: with global mode `burn` and language `eng`, an inherited channel produced a `subtitles=` filter for the English text track.
- All 12 channels are set to inherit; global defaults are English audio and subtitles off.


## Phase 4 catch-up and prewarm

- Catch-up (`POST /api/v1/channels/{id}/catchup`) starts a separate HLS session for the current program from the media start.
  - Sessions are private UUID paths under `/catchup/{session}.m3u8` and `/catchup/{session}/{segment}.ts`.
  - A new catch-up for the same channel stops the previous session.
  - Expired sessions are pruned and their directories removed.
  - When the producer finishes, the playlist gains `#EXT-X-ENDLIST`.
- Verified on `Doctor Who`: catch-up started at media offset 0 rather than the live mid-roll offset, mapped English audio, and served a valid HLS playlist and segments.
- Verified an image catch-up completed and returned `#EXT-X-ENDLIST`.
- Added per-channel `prewarm` setting:
  - prewarms opted-in channels at startup
  - prewarms immediately when enabled in stream settings
  - surfaced in producer metrics and Settings stream health
- Verified Channel 5 prewarmed immediately and survived a container restart without an HLS request; the setting was disabled again after testing.
- Added a **Restart program** button on the On Air screen.


## Phase 4 10-foot TV mode

- Added a dedicated **TV Mode** view for televisions and remote controls.
- Layout:
  - large 16:9 player
  - large now-playing title and channel/time overlay
  - live clock
  - horizontally scrolling channel strip with now/next/progress per channel
- Keyboard controls:
  - arrow keys move through channels and auto-tune
  - Enter re-tunes the selected channel
- Tunes HLS directly into the TV player; leaving TV mode stops the hidden player.
- Verified at 1600x1000: live video rendered, the overlay showed the current program, and the channel strip exposed the lineup with next-program information.


## Phase 4 installable app shell and PWA

- Added a web app manifest, service worker, 192/512/maskable icons, Apple touch icon, app shortcuts, and install prompts.
- Exposed the app over Tailscale Serve HTTPS at `https://nirvana.tail5a4a0.ts.net:8447/` so browsers can install it.
- Service worker caches only the app shell and icons; API/HLS/catch-up/DVR traffic is never cached.
- Added an **Install app** action and a Settings **Native apps** panel.
- Verified all manifest/icon/service-worker assets return 200 over HTTPS and the service worker registers (`READY`).
- Verified catch-up playlist URLs are relative so they work on both the HTTP and HTTPS origins.
- Added `docs/NATIVE.md` with install steps, TV mode controls, and native player endpoints.
- QSV/NVENC remain deferred: the current host passes VAAPI but rejects QSV and NVENC device initialization.


## Phase 4 rolling DVR buffer

- Added `POST /api/v1/channels/{id}/dvr` with a configurable rolling window
  (`window_seconds`, default 3600, clamped 30–14400) and `DELETE` to stop.
- The DVR supervisor follows the materialized channel timeline from the current
  item, starts one producer per item, appends segments to a session playlist,
  marks item boundaries with `#EXT-X-DISCONTINUITY`, and prunes segments beyond
  the requested window while advancing the HLS media sequence.
- Public playback:
  - `/dvr/{channel}.m3u8`
  - `/dvr/{channel}/{segment}.ts`
- Added a **DVR** button on On Air; starting live, catch-up, or TV mode stops the
  DVR session to avoid duplicate encoding.
- Verified with a 30-second window over a 20-second image item followed by a
  20-second episode item:
  - multiple segments from both items
  - `#EXT-X-DISCONTINUITY` at the item boundary
  - media sequence advanced after pruning
  - segment fetch returned H.264/AAC video
  - `DELETE` stopped the session and removed it
- Verified a 60-second DVR session on Doctor Who after the final build.

## Phase 4 native wrappers

- Android TV:
  - native full-screen WebView shell with Leanback launcher and banner
  - D-pad/Enter forwarding into the web app
  - HTML5 fullscreen video handling
  - cleartext LAN support and Tailscale HTTPS default
  - debug APK built successfully and verified with `aapt` for leanback launchability
- Tauri desktop:
  - Tauri v2 scaffold with external URL loading and `AWESOMETV_URL` override
  - build instructions in `desktop/README.md`
  - not compiled on this host because `libwebkit2gtk-4.1-dev` is unavailable
- PWA remains the installable desktop/mobile fallback over HTTPS.


## Cloud source core and Google Drive reference adapter

- Added encrypted cloud-source storage, roots, cloud media metadata, signed
  proxy URLs, and cloud source CRUD.
- Added Google Drive access-token, service-account, and refresh-token credential
  handling; folder listing and Range downloads.
- Added scan-time ffprobe via the signed proxy and cloud media ingestion into the
  normal library (`cloud://` paths).
- Added background cache-on-play with LRU eviction and environment budgets.
- Engine, catch-up, and DVR resolve cloud media to cached files or proxy URLs.
- Sources UI supports adding a Google Drive source, policy selection, folder
  browsing, adding roots, scanning, and deleting the source.
- Verified against a mock Google Drive API with a real MP4: 4 items scanned and
  probed, playback proxied, then cached locally.
- Interactive OAuth, provider webhooks, the public viewer split, and other
  providers remain deferred.
