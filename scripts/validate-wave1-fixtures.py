#!/usr/bin/env python3
"""Validate metadata and decoded visual/audio identities, not file hashes."""
import json
import pathlib
import subprocess
import sys


def run(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE)


def check(condition, message):
    if not condition:
        raise ValueError(message)


def validate(root):
    for name, title, dominant, frequency in [('a', 'Wave 1 fixture A', 0, 440), ('b', 'Wave 1 fixture B', 2, 880)]:
        path = str(root / f'fixture_{name}.mp4')
        probe = json.loads(run('ffprobe', '-v', 'error', '-show_format', '-show_streams', '-of', 'json', path))
        fmt = probe['format']
        check(abs(float(fmt['duration']) - 4.0) <= 0.04, f'{name}: duration {fmt["duration"]}')
        check(fmt.get('tags', {}).get('title') == title, f'{name}: title')
        streams = probe['streams']
        check(len(streams) == 2, f'{name}: stream count')
        video = next(s for s in streams if s['codec_type'] == 'video')
        audio = next(s for s in streams if s['codec_type'] == 'audio')
        check((video['codec_name'], video['width'], video['height'], video['r_frame_rate'], video['pix_fmt']) == ('h264', 160, 90, '25/1', 'yuv420p'), f'{name}: video metadata')
        check((audio['codec_name'], audio['sample_rate'], audio['channels']) == ('aac', '48000', 1), f'{name}: audio metadata')
        rgb = run('ffmpeg', '-v', 'error', '-i', path, '-frames:v', '1', '-vf', 'scale=1:1', '-f', 'rawvideo', '-pix_fmt', 'rgb24', '-')
        check(len(rgb) == 3 and rgb[dominant] > 180 and max(rgb[i] for i in range(3) if i != dominant) < 50, f'{name}: RGB {list(rgb)}')
        pcm = run('ffmpeg', '-v', 'error', '-i', path, '-ss', '0.5', '-t', '1', '-map', '0:a:0', '-f', 's16le', '-ac', '1', '-ar', '48000', '-')
        import array
        samples = array.array('h'); samples.frombytes(pcm)
        if sys.byteorder != 'little': samples.byteswap()
        check(len(samples) == 48000 and max(samples) > 500, f'{name}: audio samples')
        crossings = sum(a <= 0 < b for a, b in zip(samples, samples[1:]))
        check(abs(crossings - frequency) <= 2, f'{name}: frequency {crossings} Hz')
        print(f'{name}: duration={float(fmt["duration"]):.3f}s title={title!r} h264=160x90@25 aac=48000/mono RGB={list(rgb)} frequency={crossings}Hz')


if __name__ == '__main__':
    if len(sys.argv) != 2:
        sys.exit(f'usage: {sys.argv[0]} FIXTURE_DIR')
    try:
        validate(pathlib.Path(sys.argv[1]))
    except (ValueError, KeyError, StopIteration, subprocess.CalledProcessError, OSError) as error:
        sys.exit(f'validation failed: {error}')
