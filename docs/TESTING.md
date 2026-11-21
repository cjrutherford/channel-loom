# End-to-end testing

The browser suite uses Playwright and lives in `tests/e2e/`.

## Requirements

- A populated ChannelLoom instance (channels, media, guide).
- Chromium or Google Chrome.
- Studio credentials for authenticated tests.

## Run

```bash
corepack pnpm install
E2E_BASE_URL=https://atv.christopherrutherford.net \
E2E_CHANNEL=chrome \
E2E_USER=admin \
E2E_PASSWORD='...' \
corepack pnpm e2e
```

Use `corepack pnpm e2e:headed` to watch the browser. Public runs can set
`E2E_RETRIES=1` to retry a transient network failure once.

Without `E2E_PASSWORD`, the public Watch tests still run and the Studio tests
are skipped.

## Coverage

- Watch loads channels, guide, player, and channel switching.
- The buffer meter appears on `waiting` and disappears on `playing`.
- Public APIs: health, channels, guide, config, HDHomeRun, M3U, XMLTV, PWA
  manifest, service worker, icons, and Cast receiver.
- HLS playlist for channel 1.
- Studio login and every Studio view.
- Studio buffer meter.
- Auto-channel preview returns plans without creating channels.
- Authenticated APIs, role, audit, token, provider, and metrics endpoints.
- Anonymous access to protected APIs returns `401`.

## Long playback check

`tests/e2e/playback.spec.ts` samples the player clock for a longer window and
fails if it ever moves backwards. It is opt-in because it runs for minutes:

```bash
E2E_BASE_URL=https://atv.christopherrutherford.net \
E2E_CHANNEL=chrome \
E2E_PLAYBACK_SECONDS=180 \
corepack pnpm e2e:playback
```

Run it after changing HLS output, the producer supervisor, or the precache
policy. To provoke restarts during the run, kill the channel producer from
another shell:

```bash
pkill -f '^ffmpeg .*data/hls/1/'
```

## Manual playback checks

The automated suite does not assert live playback continuity because it depends
on producer timing. When changing HLS output or the producer supervisor, run a
browser session for several minutes while restarting a producer, then confirm
`player.currentTime` never decreases:

```bash
# in one shell, watch the player
node /tmp/playback-probe.mjs
# in another, force a producer restart
pkill -f '^ffmpeg .*data/hls/1/'
curl -s http://127.0.0.1:8409/channel/1.m3u8 | grep -E 'MEDIA-SEQUENCE|^1/'
```

`MEDIA-SEQUENCE` must increase or stay equal, never reset, and the first segment
of the new playlist must have a later MPEG-TS `start_time` than the last segment
of the old playlist.

## Notes

- Tests run serially because they share the live database and producers.
- Channel switching and HLS startup may take several seconds on a slow source
  mount.
- The suite does not create or delete channels; auto-build is preview-only.
- The isolated fixture smoke also runs in CI; the broader live suite remains separate.

## Disposable synthetic media fixture

This fixture uses no external media. It requires `ffmpeg` with `libx264` and
AAC encoders, `ffprobe`, Python 3, curl, and a built `awesometv-server`. The
separate `scripts/gen-wave1-fixtures.sh` does not change the older, larger
`scripts/gen-fixtures.sh` fixtures. It requires an empty output directory and
never deletes its contents. It makes two four-second 160×90 H.264/AAC MP4s:
red with a 440 Hz tone and blue with an 880 Hz tone. Encoder parameters and
container titles are fixed in the script. The validator probes duration, title,
stream metadata, decoded color and tone; it does not promise byte-identical
encoding across ffmpeg versions. Run from the repository root:

```bash
fixture_root=$(mktemp -d)
printf "fixture_root=%s\n" "$fixture_root"
mkdir "$fixture_root/media" "$fixture_root/data"
scripts/gen-wave1-fixtures.sh "$fixture_root/media"
scripts/validate-wave1-fixtures.py "$fixture_root/media"
ffmpeg -version | head -1
```

Start an isolated server in a separate terminal using the same `fixture_root`
(or export it first). Build beforehand with `cargo build -p awesometv-server`.
Choose an unused port and a local-only disposable password:

```bash
export fixture_root  # set this to the path printed by mktemp in both terminals
AWESOMETV_BIND=127.0.0.1 AWESOMETV_PORT=18409 \
AWESOMETV_DATA_DIR="$fixture_root/data" \
AWESOMETV_MEDIA_ROOT="$fixture_root/media" \
AWESOMETV_ADMIN_USER=admin AWESOMETV_ADMIN_PASSWORD=fixture-password \
AWESOMETV_PRECACHE_ENABLED=false target/debug/awesometv-server
```

After `/healthz` responds, in the first terminal authenticate, scan, and create
one channel in scan order (the API returns media IDs derived from paths):

```bash
base=http://127.0.0.1:18409
curl -fsS -c "$fixture_root/cookies" -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"fixture-password"}' "$base/api/v1/auth/login"
python3 - "$fixture_root/media" > "$fixture_root/scan-request.json" <<'PY'
import json, sys
print(json.dumps({'path': sys.argv[1], 'kind': 'other', 'limit': 2}))
PY
curl -fsS -b "$fixture_root/cookies" -H 'Content-Type: application/json' \
  --data-binary @"$fixture_root/scan-request.json" "$base/api/v1/scan" > "$fixture_root/scan.json"
python3 - "$fixture_root/scan.json" > "$fixture_root/channel.json" <<'PY'
import json, sys
scan = json.load(open(sys.argv[1]))
assert scan['scanned'] == 2
print(json.dumps({'number': '99', 'name': 'Fixture',
                  'media_ids': [item['id'] for item in scan['media']]}))
PY
curl -fsS -b "$fixture_root/cookies" -H 'Content-Type: application/json' \
  --data-binary @"$fixture_root/channel.json" "$base/api/v1/channels"
```

Stop the server before `rm -rf -- "$fixture_root"`; only remove the temporary
path you created. Scan order is sorted by path. The initial channel creation
sets up a timeline; it does not establish renewal, restart continuity, or live
playback guarantees. See `docs/reviews/schedule-renewal-semantics.md` for the
unresolved scheduling contract. Keep generated media and credentials out of git.

### Fixture verification record

On 2026-11-18, from the repository root, ffmpeg reported
`ffmpeg version 8.0.1-3ubuntu2`. Two independent empty directories each
passed `scripts/gen-wave1-fixtures.sh "$dir"` followed by
`scripts/validate-wave1-fixtures.py "$dir"`. Both runs reported 4.000 s,
titles `Wave 1 fixture A` and `Wave 1 fixture B`, H.264 160×90 at 25 fps,
AAC 48000 Hz mono, RGB `[254, 0, 0]` and `[0, 0, 255]`, and 439 and
879 positive zero crossings per second respectively. The one-Hz difference
from the nominal tones reflects the counting window.

With `cargo build --offline -p awesometv-server`, the start and seed flow
above was also exercised on port 18419 in a disposable directory: scan
returned two items, each 4000 ms with H.264/AAC; channel creation returned
number 99 and name `Fixture`. This checks ingest and channel creation, not
playback or renewal.


## Isolated CI browser smoke

From the repository root, install Node 22, pnpm 10, Rust 1.98.1, ffmpeg
(with libx264 and AAC), ffprobe, and Python 3. Run the CI-equivalent steps:

```bash
pnpm install --frozen-lockfile
pnpm exec playwright install --with-deps chromium
cargo build -p awesometv-server
python3 scripts/run-isolated-e2e.py
```

The lifecycle creates a private temporary media and data directory, generates and
validates both fixtures, starts a server on a loopback ephemeral port with a
random run-only password, scans the media and creates channel 99. Playwright
checks that the generated `fixture a` title appears in its guide. Startup and
test execution have deadlines. The server process group and temporary directory
are removed on success and failure; failure retains the server log in
`test-results/isolated-server.log` and Playwright retains a trace and screenshot.
CI uploads these artifacts on failure. The log may contain local paths; treat
failure artifacts as internal. The smoke requires a real server and fixture,
and does not certify playback or renewal. Existing live tests still use their
normal configuration; the fixture smoke is skipped outside this lifecycle.

The lifecycle currently requires Linux `/proc` to verify that the temporary
server owns the selected listening socket before it sends any seed requests.
An occupied-port race therefore fails instead of seeding another process.

### Isolated smoke verification record

On 2026-11-18, with Node 22.22.1, Playwright 1.65.1, Rust 1.98.1 and
ffmpeg 8.0.1 on Linux, `cargo build -p awesometv-server` and
`python3 scripts/run-isolated-e2e.py` succeeded: two fixtures validated and
one browser test passed from a newly created database. Setting
`E2E_SERVER_BINARY=/bin/false` made startup fail and retained the server log;
an unseeded temporary server made the guide assertion fail. Forcing a missing
browser made the lifecycle fail while retaining its server log and Playwright
trace. Each run reported temporary server and data cleanup. GitHub Actions
itself has not been run as part of this local record.
