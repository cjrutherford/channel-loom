# Release process

Releases are tag-driven. Pushing a tag that starts with `v` runs
`.github/workflows/release.yml` and publishes a GitHub release.

## What a release builds

| Asset | Source |
|---|---|
| `channelloom-server-linux-amd64` | `cargo build --release -p awesometv-server` |
| `channelloom-android-tv-debug.apk` | Android TV `assembleDebug` |
| Tauri Linux `.deb` | `cargo tauri build --bundles deb` |

The source archives are attached automatically by GitHub.

## Cut a release

```bash
git checkout main
git pull --ff-only
git tag -a v0.1.0 -m "ChannelLoom v0.1.0"
git push origin v0.1.0
```

The release workflow:

1. Builds the server, APK, and desktop bundle in parallel.
2. Downloads the artifacts into one release directory.
3. Publishes a GitHub release with generated notes and the asset list.

## Before tagging

- `main` CI is green.
- The version in `Cargo.toml` matches the tag.
- The live deployment has been smoke-tested.
- Release notes mention known limits:
  - Android APK is debug-signed.
  - Cast uses the Default Media Receiver until an App ID is registered.
  - Public streams are capped at the configured bitrate.

## Deploy a release

Run the published server binary inside the current runtime image, so FFmpeg and
the SQLite client match the container:

```bash
VERSION=v0.1.2
gh release download "$VERSION" \
  --pattern channelloom-server-linux-amd64 \
  --dir /tmp/channelloom-release --clobber
cid=$(docker create --name channelloom-deploy awesome-tv-awesometv:latest)
docker cp /tmp/channelloom-release/channelloom-server-linux-amd64 \
  "$cid":/usr/local/bin/awesometv-server
docker commit "$cid" "awesome-tv-awesometv:release-$VERSION"
docker rm "$cid"
docker tag "awesome-tv-awesometv:release-$VERSION" awesome-tv-awesometv:latest
docker compose up -d --force-recreate --no-build
```

Then verify `healthz`, a channel playlist, one segment, and the guide. Rebasing on
the runtime image means a release does not rebuild FFmpeg; rebuild the image from
the tag (`docker compose build`) when the runtime packages need to change.

## Verify

```bash
gh release view v0.1.2
gh release download v0.1.2 --dir /tmp/channelloom-release
```

Check that the server binary starts (it has no `--help` flag; run it with a test
data directory and port), the APK is installable, and the `.deb` installs on a
Linux desktop.
