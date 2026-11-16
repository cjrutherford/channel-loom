#!/usr/bin/env bash
# Generate deterministic synthetic media fixtures for Phase 0 spikes and tests.
# Usage: scripts/gen-fixtures.sh [output-dir] [--force]
set -euo pipefail

out="${1:-testdata/generated}"
force="${2:-}"

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is required" >&2
  exit 1
fi

if [[ -d "$out" && "$force" != "--force" ]]; then
  echo "fixtures already exist at $out (use --force to regenerate)"
  exit 0
fi

mkdir -p "$out"
rm -f "$out"/*

FF=(ffmpeg -hide_banner -loglevel error -y)

echo "generating 1080p H.264/AAC clip with subtitles and chapters"
"${FF[@]}" \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30000/1001:duration=12" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=12" \
  -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p \
  -c:a aac -b:a 192k -ac 2 -shortest \
  "$out/clip_a_1080p_h264.mp4"

cat > "$out/clip_a.srt" <<'SRT'
1
00:00:01,000 --> 00:00:04,000
Fixture subtitle line one

2
00:00:05,000 --> 00:00:08,500
Fixture subtitle line two
SRT

cat > "$out/clip_a.chapters.txt" <<'CHAPTERS'
;FFMETADATA1
title=Fixture A

[CHAPTER]
TIMEBASE=1/1000
START=0
END=4000
title=Opening

[CHAPTER]
TIMEBASE=1/1000
START=4000
END=8000
title=Middle

[CHAPTER]
TIMEBASE=1/1000
START=8000
END=12000
title=Ending
CHAPTERS

"${FF[@]}" -i "$out/clip_a_1080p_h264.mp4" -i "$out/clip_a.chapters.txt" \
  -map 0 -map_metadata 1 -c copy "$out/clip_a_1080p_h264_chapters.mp4"
mv "$out/clip_a_1080p_h264_chapters.mp4" "$out/clip_a_1080p_h264.mp4"

echo "generating 720p HEVC/AC3 clip"
"${FF[@]}" \
  -f lavfi -i "testsrc2=size=1280x720:rate=25:duration=10" \
  -f lavfi -i "sine=frequency=330:sample_rate=48000:duration=10" \
  -c:v libx265 -preset ultrafast -crf 28 -pix_fmt yuv420p \
  -c:a ac3 -b:a 192k -ac 2 -shortest \
  "$out/clip_b_720p_hevc.mkv"

echo "generating 480p MPEG-2/MP2 4:3 clip"
"${FF[@]}" \
  -f lavfi -i "testsrc2=size=720x480:rate=30000/1001:duration=8" \
  -f lavfi -i "sine=frequency=220:sample_rate=48000:duration=8" \
  -vf "setsar=4/3" -aspect 4:3 \
  -c:v mpeg2video -q:v 4 -pix_fmt yuv420p \
  -c:a mp2 -b:a 192k -ac 2 -shortest \
  "$out/clip_c_480p_mpeg2.mpg"

echo "generating 1080p H.264 with 5.1 E-AC-3 clip"
"${FF[@]}" \
  -f lavfi -i "testsrc2=size=1920x1080:rate=24000/1001:duration=8" \
  -f lavfi -i "sine=frequency=550:sample_rate=48000:duration=8" \
  -c:v libx264 -preset veryfast -crf 22 -pix_fmt yuv420p \
  -c:a eac3 -b:a 384k -ac 6 -shortest \
  "$out/clip_d_1080p_h264_51.mkv"

echo "generating still image and audio fixtures"
"${FF[@]}" -f lavfi -i "color=c=0x2dd4bf:size=1280x720:duration=1" -frames:v 1 "$out/slate.png"
"${FF[@]}" -f lavfi -i "sine=frequency=880:sample_rate=48000:duration=6" -c:a flac "$out/tone.flac"

cat > "$out/manifest.json" <<JSON
{
  "clips": [
    {"id": "a", "path": "clip_a_1080p_h264.mp4", "codec": "h264", "resolution": "1920x1080", "fps": "30000/1001", "audio": "aac"},
    {"id": "b", "path": "clip_b_720p_hevc.mkv", "codec": "hevc", "resolution": "1280x720", "fps": "25", "audio": "ac3"},
    {"id": "c", "path": "clip_c_480p_mpeg2.mpg", "codec": "mpeg2video", "resolution": "720x480", "fps": "30000/1001", "audio": "mp2"},
    {"id": "d", "path": "clip_d_1080p_h264_51.mkv", "codec": "h264", "resolution": "1920x1080", "fps": "24000/1001", "audio": "eac3"}
  ]
}
JSON

echo "fixtures written to $out"
ls -lh "$out"
