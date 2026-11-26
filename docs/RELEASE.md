# Release process

Releases are tag-driven. Pushing a `v*` tag runs
`.github/workflows/release.yml`, which publishes container images to GHCR and a
GitHub release with binaries.

## What a release publishes

| Output | Where |
|---|---|
| Container image, `linux/amd64` + `linux/arm64` | `ghcr.io/cjrutherford/channel-loom:X.Y.Z`, `:X.Y`, `:latest` |
| Server binary tarballs | `channelloom-server-X.Y.Z-linux-{amd64,arm64}.tar.gz` |
| Android TV APK (debug-signed) | `channelloom-android-tv-X.Y.Z-debug.apk` |
| Linux desktop bundle | Tauri `.deb` |
| Checksums | `SHA256SUMS` |

Images carry an SBOM, provenance, and a GitHub build attestation. Every push to
`main` also publishes `:edge` and `:sha-<commit>`.

A tag with a suffix (`v0.2.0-rc.1`) publishes a prerelease and does not move
`latest` or `X.Y`.

## Cut a release

1. Bump the version in all four places. The workflow refuses a tag that doesn't match them:
   - `Cargo.toml` (`[workspace.package] version`)
   - `desktop/src-tauri/Cargo.toml`
   - `desktop/src-tauri/tauri.conf.json`
   - `android-tv/app/build.gradle` (`versionName`; also increment `versionCode`)
2. Run `cargo check` so `Cargo.lock` picks up the new version. Merge to `main` and wait for green CI.
3. Tag and push:

   ```bash
   git checkout main && git pull --ff-only
   git tag -a v0.1.4 -m "ChannelLoom v0.1.4"
   git push origin v0.1.4
   ```

The workflow then:

1. Checks the tag against the versions above.
2. Runs the full CI suite: fmt, clippy, tests, browser smoke, multi-arch image build and smoke test, APK, and `.deb`.
3. Pushes the multi-arch image to GHCR, attests it, and smoke-tests the pushed tag.
4. Builds the server tarballs from the image's build stage (Debian bookworm, glibc 2.36).
5. Publishes the GitHub release with generated notes, the assets, and `SHA256SUMS`.

## Verify

```bash
gh release view v0.1.4
docker pull ghcr.io/cjrutherford/channel-loom:0.1.4
gh attestation verify oci://ghcr.io/cjrutherford/channel-loom:0.1.4 --repo cjrutherford/channel-loom
```

A running server reports its build at `GET /api/v1/public/version`.

## Upgrade a deployment

```bash
docker compose pull
docker compose up -d
```

Set `CHANNELLOOM_VERSION` in `.env` to pin a release instead of following
`latest`. The database migrates on startup.

## One-time setup

The first push creates the GHCR package as private. Make it public so anonymous
pulls work: GitHub → Packages → `channel-loom` → Package settings → Change
visibility.
