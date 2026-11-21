#!/usr/bin/env python3
"""Serve an isolated, disposable AwesomeTV fixture for manual IPTV client checks."""
import argparse
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=300, choices=range(10, 1801), metavar='10..1800')
    parser.add_argument('--evidence-dir', type=Path, required=True, help='new directory for public feed snapshots and server log')
    args = parser.parse_args()
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    out = args.evidence_dir.resolve()
    binary = ROOT / 'target/debug/awesometv-server'
    if not binary.is_file():
        raise RuntimeError(f'build the server first: {binary} missing')
    with tempfile.TemporaryDirectory(prefix='awesometv-client-') as tmp:
        root = Path(tmp)
        (root/'media').mkdir(); (root/'data').mkdir()
        subprocess.run([str(ROOT/'scripts/gen-wave1-fixtures.sh'), str(root/'media')], check=True, timeout=90)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        env = {k:v for k,v in os.environ.items() if not k.startswith('AWESOMETV_')}
        password = secrets.token_urlsafe(32)
        env.update(AWESOMETV_BIND='127.0.0.1', AWESOMETV_PORT=str(port), AWESOMETV_PUBLIC_URL=base,
                   AWESOMETV_DATA_DIR=str(root/'data'), AWESOMETV_MEDIA_ROOT=str(root/'media'),
                   AWESOMETV_ADMIN_USER='fixture', AWESOMETV_ADMIN_PASSWORD=password, AWESOMETV_PRECACHE_ENABLED='false')
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        def post(path, body):
            req = urllib.request.Request(base+path, json.dumps(body).encode(), {'Content-Type':'application/json'})
            with opener.open(req, timeout=30) as response: return json.load(response)
        def fetch(path):
            with opener.open(base+path, timeout=10) as response: return response.read()
        with (out/'server.log').open('wb') as log:
            proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                # Verify listener ownership before any writes; prevent port-race access to another service.
                import sys
                sys.path.insert(0, str(ROOT/'scripts'))
                from importlib.machinery import SourceFileLoader
                owns = SourceFileLoader('isolated_runner', str(ROOT/'scripts/run-isolated-e2e.py')).load_module().owns_listener
                deadline = time.monotonic()+30
                while time.monotonic() < deadline:
                    if proc.poll() is not None: raise RuntimeError(f'server exited {proc.returncode}')
                    try:
                        if owns(proc.pid, port) and fetch('/healthz'): break
                    except (OSError, FileNotFoundError): pass
                    time.sleep(.2)
                else: raise TimeoutError('owned server readiness')
                post('/api/v1/auth/login', {'username':'fixture', 'password':password})
                scan = post('/api/v1/scan', {'path':str(root/'media'), 'kind':'other', 'limit':2})
                ids = [item['id'] for item in scan['media']]
                if len(ids) != 2: raise RuntimeError('expected two scanned fixtures')
                collection = post('/api/v1/collections', {'name':'Compatibility fixture', 'media_ids':ids})
                result = post('/api/v1/channels/auto', {'collection_ids':[collection['id']], 'dry_run':False, 'order':'sequential'})
                if not result['created']: raise RuntimeError('no channel created')
                for path, name in [('/iptv/channels.m3u','channels.m3u'),('/iptv/xmltv.xml','xmltv.xml')]:
                    (out/name).write_bytes(fetch(path))
                print(f'M3U: {base}/iptv/channels.m3u\nXMLTV: {base}/iptv/xmltv.xml\nEvidence: {out}\nExpires in {args.seconds}s; Ctrl-C stops it.', flush=True)
                end = time.monotonic()+args.seconds
                while time.monotonic() < end:
                    if proc.poll() is not None: raise RuntimeError(f'server exited {proc.returncode}')
                    time.sleep(min(1, end-time.monotonic()))
            finally:
                try: os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError: pass
                try: proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL); proc.wait(timeout=5)

if __name__ == '__main__':
    def terminate(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    try: main()
    except KeyboardInterrupt: print('\nStopped; temporary media and database removed.')
