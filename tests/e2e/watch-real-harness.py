#!/usr/bin/env python3
"""Run the fixture smoke against a disposable local server and database."""
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
ARTIFACTS = ROOT / 'test-results'
server = None

def stop_signal(signum, _frame):
    raise SystemExit(128 + signum)

for sig in (signal.SIGINT, signal.SIGTERM):
    signal.signal(sig, stop_signal)

def owns_listener(pid, port):
    # Confirm the listener belongs to this server before issuing any API writes.
    with open('/proc/net/tcp', encoding='ascii') as tcp:
        inodes = {parts[9] for line in tcp if len(parts := line.split()) > 9
                  and parts[1] == f'0100007F:{port:04X}' and parts[3] == '0A'}
    for fd in Path(f'/proc/{pid}/fd').iterdir():
        try:
            if os.readlink(fd) in {f'socket:[{inode}]' for inode in inodes}:
                return True
        except FileNotFoundError:
            pass
    return False

def run():
    global server
    binary = Path(os.environ.get('E2E_SERVER_BINARY', ROOT / 'target/debug/awesometv-server')).resolve()
    with tempfile.TemporaryDirectory(prefix='awesometv-e2e-') as tmp:
        root = Path(tmp)
        media, data = root / 'media', root / 'data'
        media.mkdir(); data.mkdir()
        log = root / 'server.log'
        success = False
        try:
            subprocess.run([str(ROOT / 'scripts/gen-wave1-fixtures.sh'), str(media)], check=True, timeout=90)
            subprocess.run([str(ROOT / 'scripts/validate-wave1-fixtures.py'), str(media)], check=True, timeout=90)
            # The socket is released before launch; a bind race causes startup failure, never reuse.
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                port = sock.getsockname()[1]
            base = f'http://127.0.0.1:{port}'
            password = secrets.token_urlsafe(32)
            env = {k: v for k, v in os.environ.items() if not k.startswith('AWESOMETV_')}
            env.update(AWESOMETV_BIND='127.0.0.1', AWESOMETV_PORT=str(port),
                       AWESOMETV_DATA_DIR=str(data), AWESOMETV_MEDIA_ROOT=str(media),
                       AWESOMETV_ADMIN_USER='fixture', AWESOMETV_ADMIN_PASSWORD=password,
                       AWESOMETV_PRECACHE_ENABLED='false')
            with log.open('wb') as output:
                server = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=output,
                                          stderr=subprocess.STDOUT, start_new_session=True)
                opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
                deadline = time.monotonic() + 30
                while True:
                    if server.poll() is not None:
                        raise RuntimeError(f'server exited during startup: {server.returncode}')
                    try:
                        if owns_listener(server.pid, port):
                            with opener.open(base + '/healthz', timeout=1) as response:
                                if response.status == 200:
                                    break
                    except OSError:
                        pass
                    if time.monotonic() >= deadline:
                        raise TimeoutError('server readiness exceeded 30 seconds')
                    time.sleep(.2)
                def post(path, body):
                    req = urllib.request.Request(base + path, data=json.dumps(body).encode(),
                                                 headers={'Content-Type': 'application/json'})
                    with opener.open(req, timeout=30) as response:
                        return json.load(response)
                post('/api/v1/auth/login', {'username': 'fixture', 'password': password})
                scan = post('/api/v1/scan', {'path': str(media), 'kind': 'other', 'limit': 2})
                if scan['scanned'] != 2 or len(scan['media']) != 2:
                    raise RuntimeError('fixture scan did not return both files')
                post('/api/v1/channels', {'number': '99', 'name': 'Fixture',
                                          'media_ids': [item['id'] for item in scan['media']]})
                post('/api/v1/channels', {'number': '98', 'name': 'Second fixture', 'media_ids': [item['id'] for item in scan['media']]})
                env.update(E2E_ISOLATED='1', E2E_BASE_URL=base, E2E_RETRIES='0', E2E_CHANNEL='chrome')
                
                subprocess.run([str(ROOT / 'node_modules/.bin/playwright'), 'test', 'watch-real.spec.ts', '--reporter=line'],
                               cwd=ROOT, env=env, check=True, timeout=210)
                success = True
        finally:
            if server is not None:
                try:
                    os.killpg(server.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(server.pid, signal.SIGKILL)
                    server.wait(timeout=5)
                server = None
            if not success and log.exists():
                ARTIFACTS.mkdir(exist_ok=True)
                shutil.copyfile(log, ARTIFACTS / 'isolated-server.log')
                print(f'Server log retained at {ARTIFACTS / "isolated-server.log"}', file=sys.stderr)
            print('Temporary server and data cleaned up', flush=True)

if __name__ == '__main__':
    try:
        run()
    except (Exception, KeyboardInterrupt) as exc:
        print(f'Isolated smoke failed: {exc}', file=sys.stderr)
        sys.exit(1)
