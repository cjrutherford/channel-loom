# AMD VAAPI validation — 2026-11-19

## Outcome and deployment decision

The existing deployment already maps `/dev/dri` and is encoding video with AMD VAAPI. No configuration or pipeline fix has been established by these tests, so no image was rebuilt or deployed. The requested fresh-image regression, backup and deployment gate remains incomplete. The running service has separately logged `item boundary differs from published cursor`; that pre-existing publisher failure is outside the proven GPU fix and makes a blind redeployment unsafe. No changes were made to the live service, database, nginx, or container configuration.

The protected test artifacts are in `/home/cjrutherford/.t3/soak-evidence/awesome-tv/amd-vaapi-eNfsOsUR` (directory mode 0700). Paths and logs in that directory may reveal media names and must remain private. No full media library or large source was copied. Tests ran sequentially in disposable containers against the current image; live service activity was not controlled.

## Device and actual pipeline

Running image: `sha256:f192a1025dcc32505749f6286410370c7fea9049b5daf6b9a2797e7783140032`, container user 1000:1000. Docker already maps `/dev/dri` read/write; render node 128 reports AMD vendor 1002, device 73ff and host driver `amdgpu`. Container FFmpeg is 8.0.1-3ubuntu2. The observed live producer command used `-vaapi_device /dev/dri/renderD128`, CPU decode, scale, pad, fps and drawtext, `format=nv12,hwupload`, and `h264_vaapi`, with HLS output and AAC audio. This proves neither hardware decoding nor GPU filtering. Slate uses software encoding. The workspace also contains the hardware upload suffix for static and animated watermark paths.

Channel 1's log shows `No filtered frames for output stream` immediately before `A hardware frames reference is required` and no output packets. In the same image, an isolated seek beyond EOF reproduced that sequence despite `format=nv12,hwupload`; it exited 234. This establishes an empty video stream can cause the reported encoder message, not the cause of the historical empty stream. The surrounding log includes an Opus parsing error, but the exact historical input, seek and causal chain were not recovered. A synthetic upload/encode succeeded. Suppressing the error or switching to software would not fix the underlying absence of frames.

## Short source and encoder experiments

Current channel 1 source sampled as 1918x1080 SDR HEVC 8-bit; channel 10 as 3840x2160 HEVC 10-bit HDR10 (BT.2020/PQ). The channel 1 comparison used an existing ready local cache copy of the same source, the same 60-second seek, eight-second output, 1280x720 scale/pad, 30000/1001 fps, AAC 160k, four-second HLS segments, and two threads; one process at a time. Sizes match and the first 8 MiB hashes match; the full 1.75 GB source was not rehashed. VAAPI used QP 22; libx264 used CRF 22 and veryfast, which are not equal quality/rate-control settings. Times include Docker startup, input open/probe and encoding; first closed segment means the first completed `.ts` appeared, not public publication. Each first segment had 120 video frames and decoded successfully.

| Source | Encoder | First closed segment | Total |
| --- | --- | ---: | ---: |
| NAS | VAAPI | 3.97 s | 4.52 s |
| NAS | libx264 | 23.65 s | 24.45 s |
| local cache | VAAPI | 4.27 s | 4.77 s |
| local cache | libx264 | 4.82 s | 5.62 s |

These are single observations, not a controlled benchmark. During the NAS VAAPI run a separate bounded read sampled the same NAS file and was interrupted; other live workloads continued throughout. Cache warmth, network contention, different codec rate control, and process startup confound comparisons. Total-time ratios are not encoder-only speed, and publication pacing was not measured. A separate sequential 8 MiB read from the start of each file took 9.153 s from NAS and 0.005 s from local ext4 storage, with matching hashes. This points to a possible source-read delay under observed conditions, but does not characterize sustained reads or establish the cause of publisher lag. Earlier tests using a separately extracted local clip are in the protected artifacts and are not matched-input evidence.

A separate four-second channel 10 test used the application's CPU scale and zscale/Hable tone-map sequence followed by NV12 upload and VAAPI encoding. It completed in 12.24 s including input/probe and produced 119 decodable 1280x720 H.264 frames tagged BT.709. This does not establish real-time HDR throughput, visual correctness across all HDR sources, subtitle/watermark behavior, or publication timing.

## Live observations and remaining work

Local and public DNS `/healthz` returned HTTP 200. In the public browser, channel 1 advanced from 74.64 to 86.64 seconds and decoded frames advanced from 139 to 499 over 12 seconds. This was a short observation, not a soak. The container remained on the original image, running with zero restarts; disposable test containers were removed. A recent live log also contained the publisher cursor error, so this playback sample does not prove continuity across restarts or item boundaries.

Before a production change, identify the exact historical no-frame source/seek and separately diagnose the cursor failure. Prove the actual application-built producer and private publication path on representative SDR and HDR inputs, including subtitles, watermark and slate; measure source access, encoding and publication separately with controlled repetitions. Then build the intended dirty snapshot warning-free with provenance, validate it in isolation, prepare a consistent database backup and protected key/config backup and rollback image, and only deploy if the regression gate passes. No long-run or twelve-channel capacity claim follows from this short test.
