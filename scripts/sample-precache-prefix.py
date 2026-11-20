#!/usr/bin/env python3
"""Bounded sample only; matching samples do not prove every prefix byte."""
import argparse
import hashlib
import json
import os
import subprocess
import sys

CHUNK = 1024 * 1024

def worker(source, partial, size, mtime, prefix):
    def identity():
        st = os.stat(source)
        return st.st_size, st.st_mtime_ns // 1_000_000
    if identity() != (size, mtime):
        raise ValueError('source identity mismatch before sampling')
    if prefix <= 0 or prefix > size or os.stat(partial).st_size < prefix:
        raise ValueError('invalid protected prefix length')
    length = min(CHUNK, prefix)
    offsets = sorted(set((0, max(0, (prefix - length) // 2), prefix - length)))
    samples = []
    with open(source, 'rb', buffering=0) as src, open(partial, 'rb', buffering=0) as dst:
        for offset in offsets:
            a, b = os.pread(src.fileno(), length, offset), os.pread(dst.fileno(), length, offset)
            if len(a) != length or len(b) != length:
                raise ValueError('short read')
            ha, hb = hashlib.sha256(a).hexdigest(), hashlib.sha256(b).hexdigest()
            samples.append({'offset': offset, 'length': length, 'source_sha256': ha, 'prefix_sha256': hb})
            if ha != hb:
                raise ValueError('sample mismatch at offset %d' % offset)
    if identity() != (size, mtime):
        raise ValueError('source identity changed during sampling')
    print(json.dumps({'result': 'sample_match_not_full_proof', 'prefix': prefix, 'samples': samples}))

def main():
    p = argparse.ArgumentParser()
    p.add_argument('marker'); p.add_argument('source'); p.add_argument('--timeout', type=float, default=30)
    p.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    a = p.parse_args()
    if a.timeout <= 0: p.error('timeout must be positive')
    if a.worker:
        try:
            with open(a.marker) as f: m = json.load(f)
            worker(a.source, os.path.join(os.path.dirname(a.marker), 'protected.part'), m['size'], m['mtime'], m['prefix'])
        except Exception as e:
            print(json.dumps({'result': 'inconclusive', 'reason': str(e)})); sys.exit(1)
    else:
        proc = subprocess.Popen([sys.executable, __file__, a.marker, a.source, '--worker'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            out, err = proc.communicate(timeout=a.timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            print(json.dumps({'result': 'inconclusive', 'reason': 'source I/O or sample timed out'})); sys.exit(2)
        print(out.strip() or json.dumps({'result': 'inconclusive', 'reason': err.strip() or 'worker failed'}))
        sys.exit(proc.returncode or 0)

if __name__ == '__main__': main()
