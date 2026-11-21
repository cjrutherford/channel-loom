#!/usr/bin/env python3
"""Encode complementary 13-bit source-second cells into actual video frames."""
import subprocess, sys
from pathlib import Path
out = Path(sys.argv[1]); duration = int(sys.argv[2]) if len(sys.argv) > 2 else 4200
if not 1 <= duration <= 8191: raise ValueError('duration must be 1..8191')
# Alternating complementary cells catch corrupt/ambiguous decodes. A one-second
# frame is held by the encoder; the encoded content, not player time, is read.
boxes = []
for bit in range(13):
    for inverse in range(2):
        expr = f"eq(mod(floor(t/{2**bit}),2),{1-inverse})"
        boxes.append(f"drawbox=x={(2*bit+inverse)*12}:y=0:w=12:h=48:color=white:t=fill:enable='{expr}'")
subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-f','lavfi','-i',f'color=black:s=320x240:r=1:d={duration}', '-vf', ','.join(boxes), '-c:v','libx264','-preset','ultrafast','-crf','12','-pix_fmt','yuv420p','-movflags','+faststart','-y',str(out)], check=True, timeout=600)
