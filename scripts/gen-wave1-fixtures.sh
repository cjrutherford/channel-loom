#!/usr/bin/env bash
# Generate only into a new or empty caller-owned directory.
set -euo pipefail
[[ $# == 1 ]] || { echo "usage: $0 EMPTY_OUTPUT_DIR" >&2; exit 2; }
out=$1
command -v ffmpeg >/dev/null || { echo 'ffmpeg required' >&2; exit 1; }
mkdir -p "$out"
[[ -z $(find "$out" -mindepth 1 -maxdepth 1 -print -quit) ]] || { echo 'output directory must be empty' >&2; exit 1; }
for spec in 'a red 440' 'b blue 880'; do
  read -r name color frequency <<< "$spec"
  ffmpeg -hide_banner -loglevel error -nostdin -n \
    -f lavfi -i "color=c=$color:s=160x90:r=25:d=4" \
    -f lavfi -i "sine=frequency=$frequency:sample_rate=48000:duration=4" \
    -map 0:v:0 -map 1:a:0 -map_metadata -1 -t 4 \
    -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p -threads 1 \
    -c:a aac -b:a 96k -ar 48000 -ac 1 \
    -metadata "title=Wave 1 fixture ${name^^}" -movflags +faststart \
    "$out/fixture_${name}.mp4"
done
