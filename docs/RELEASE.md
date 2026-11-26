# Release process

Releases are tag-driven. Pushing a `v*` tag runs
`.github/workflows/release.yml`, which publishes container images to GHCR and a
GitHub release with binaries.

## What a release publishes

| Output | Where |
|---|---|
| Container image, `linux/amd64` + `linux/arm64` | `ghcr.io/cjrutherford/channel-loom:X.Y.Z`, `:X.Y`, `:latest` |
| Server binary tarballs | `channelloom-server-X.Y.Z-linux-{amd64,arm64}.tar.gz` |
| Android TV APK, release-signed | `channelloom-android-tv-X.Y.Z.apk` |
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
5. Builds the release-signed APK and verifies its signature.
6. Publishes the GitHub release with generated notes, the assets, and `SHA256SUMS`.

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

### Android signing key

The release APK is signed with a key held in four repository secrets:

| Secret | Contents |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0` of the PKCS12 keystore |
| `ANDROID_KEYSTORE_PASSWORD` | keystore password |
| `ANDROID_KEY_ALIAS` | key alias (`channelloom`) |
| `ANDROID_KEY_PASSWORD` | key password (same as the keystore password for PKCS12) |

The release fails if they are missing. Keep an offline backup of the keystore
and its passwords. Android only installs an update signed with the same key, so
losing it means users must uninstall the app to move to a new one.

To build a signed APK locally, export the same values (with `CHANNELLOOM_` names)
and point `CHANNELLOOM_KEYSTORE` at the keystore file:

```bash
export CHANNELLOOM_KEYSTORE=/path/to/android-release.jks
export CHANNELLOOM_KEYSTORE_PASSWORD=... CHANNELLOOM_KEY_ALIAS=channelloom CHANNELLOOM_KEY_PASSWORD=...
cd android-tv && ./gradlew assembleRelease
```

Without `CHANNELLOOM_KEYSTORE`, `assembleRelease` produces an unsigned APK.
