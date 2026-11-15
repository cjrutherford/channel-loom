# ADR 0004 — Streaming engine

Status: accepted
Date: 2026-11-14

## Context

ErsatzTV's engine history shows the hard parts: heterogeneous sources, hardware quirks, fast tune-in, transitions, subtitles, overlays, and failure isolation. The engine must work on x86 GPU and low-power ARM.

## Decision

Build our own engine as a per-channel runtime with per-item FFmpeg processes and a segment cache.

- One runtime per active channel; start on demand; idle stop after a heartbeat timeout.
- Resolve the current/next timeline item, then produce segments ahead of the live edge.
- Per-item FFmpeg process for fault isolation; normalize to the channel profile unless copy mode applies.
- Write to a staging directory and publish segments atomically into a rolling HLS window.
- Emit `EXT-X-PROGRAM-DATE-TIME`; insert `EXT-X-DISCONTINUITY` when encoding parameters change.
- Prefetch the next item before the boundary; cache segments by content/profile/graphics/subtitle/in-out.
- Serve HLS and a MPEG-TS gateway for HDHomeRun/Plex; enforce tuner count and heartbeat.
- On item failure, play fallback (filler/slate/error card/black+silence) for that item's window; never take the channel offline.
- Capability-probe hardware and fall back to software at each step.
- Write a diagnostic bundle for every failed item.

## Consequences

- The first spike validates a single-process concat pipeline; production moves to per-item processes and a stitcher.
- HLS is the canonical output; MPEG-TS is a gateway, not a second encoder.
- Copy mode is per item, not per channel.
- Cache and work-ahead need quotas, invalidation, and metrics.
- Client compatibility must be tested against real Plex/Jellyfin/Emby/Channels/Kodi/TiviMate installations.
