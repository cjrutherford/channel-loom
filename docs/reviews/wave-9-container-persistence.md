# Wave 9 slice 19 — container persistence and upgrade

Status: passed for the bounded test pair described below. The test pair is the pre-review repository checkpoint `f486be4fafdf3c8711385852ab6fcf72b988fb64` and the current review source snapshot; this is not a general supported-release compatibility claim. The local v0.1.2 image has unverified source provenance and is not used. Rollback means restoring an untouched stopped pre-upgrade backup to the baseline image, not opening the migrated database with older code.

The baseline context was produced by `git archive f486be4` in `/tmp/awesome-slice19-20261118/baseline`. The review context was captured using `git ls-files -co --exclude-standard` for the Dockerfile, Cargo manifests and crate sources in `/tmp/awesome-slice19-20261118/review`; it excludes .git, target, node_modules, ignored secrets and artifacts. The sorted per-file SHA-256 manifest is `/tmp/awesome-slice19-20261118/review-manifest.sha256`, SHA-256 `c0441d6312df8d4d8935b350974f70fd2d9f5c273a1702f9816a1e039eea8285`; the tracked binary diff restricted to these source paths has SHA-256 `0ff5efc15bbf75975f27c9efbedf12f2599bcc362b9dfafd0dac9b0c59007f54`. Four untracked Rust sources are included by the manifest. The snapshot predates the container harness and this document, which do not enter the image.

Baseline build command: `timeout 900s docker build --progress=plain -t awesome-slice19-baseline:local /tmp/awesome-slice19-20261118/baseline`, log `/tmp/awesome-slice19-20261118/baseline-build.log`. The harness creates and removes only uniquely named containers and volumes and uses a loopback ephemeral port. Existing checkout, deployment and resources are preserved.

## Result — 2026-11-18

**Passed for this tested pair.** Both Docker builds exited 0 using the repository Dockerfile. Baseline image: `sha256:cf8e92d40bd96d68608bb2f7f8913f0aa1dd180e46e9177245a5bdac40a39762`; review image: `sha256:90e270f9b94fc6e3cf5f4c4a1fc765ffcc988d2caaba4ccf70600c477242ee2f`. Review build log: `/tmp/awesome-slice19-20261118/review-build.log` (11 compiler warnings). The base image references are resolved in the build logs; neither image is claimed to be a previously published release.

The following exited 0 (complete output in `/tmp/awesome-slice19-20261118/proof.log`):

```sh
timeout 300s python3 scripts/container-persistence-proof.py \
  sha256:cf8e92d40bd96d68608bb2f7f8913f0aa1dd180e46e9177245a5bdac40a39762 \
  sha256:90e270f9b94fc6e3cf5f4c4a1fc765ffcc988d2caaba4ccf70600c477242ee2f \
  /tmp/awesome-slice19-20261118/proof
```

The harness generated and validated two local four-second media fixtures; started the baseline on an ephemeral loopback port; logged in; scanned both media; created channel 99; and changed language settings. It stopped the server before copying the data volume to a backup, restarted the same baseline volume, copied that backup into a separate review-image volume, and started the review image twice. Settings, both media IDs, channel identity and overlapping guide entries matched at each step; the review database schema was identical across its two startups. It then copied the original backup to a third volume and restored it under the baseline image, repeating the application checks and verifying the backup checksums remained unchanged.

An independent read-only SQLite comparison of the stopped backup and the second stopped review database found all baseline columns and rows identical for `app_settings` (3 rows), `media_items` (2), `channels` (1), and `channel_items` (151,200). The initial API guide contained 901 entries. The review schema had 54 non-internal objects versus 21 at baseline: 33 added objects and no changed baseline object definitions. This demonstrates an actual migration and repeat startup for this fixture; it does not prove every possible input, upgrade origin, or failure mode.

The evidence directory contains the backup, per-file hashes, snapshots and container logs. It is owner-only because the disposable database includes a generated cloud key. The harness removed its three uniquely named containers and volumes; a Docker name-filter check found none remaining. The two local test images and private evidence remain for inspection. The harness now creates its evidence directory owner-only; that permission hardening was applied after this run and the existing directory was secured. Playback was not repeated; earlier playback evidence remains separate. No failed-upgrade injection, hosted CI, production deployment, or general rollback/downgrade compatibility is claimed.

Review recheck: the manifest hash matches the recorded value; `sha256sum -c` against the current checkout reports only `crates/awesometv-server/web/public.html` changed among recorded image inputs. A fresh read-only comparison of the stopped backup and second upgraded database again matched 3 settings, 2 media, 1 channel and 151,200 channel-item rows. Docker name-filter checks found no `awesome-s19-` containers or volumes. The recorded snapshot, not the subsequent Watch HTML, is the tested review image. The plan’s supported-version origin, failed-upgrade injection, and guide/playback upgrade evidence remain unproven; the snapshot pair is accepted only as a bounded prerequisite for slice 20.
