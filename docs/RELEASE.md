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

## Verify

```bash
gh release view v0.1.0
gh release download v0.1.0 --dir /tmp/channelloom-release
```

Check that the server binary runs `--help` or starts with a test data directory,
the APK is installable, and the `.deb` installs on a Linux desktop.
