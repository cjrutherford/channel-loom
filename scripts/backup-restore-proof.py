#!/usr/bin/env python3
"""Disposable local backup/restore drill. Never touches configured data or credentials."""
import hashlib, http.cookiejar, json, os, pathlib, secrets, shutil, signal, socket, sqlite3, subprocess, tempfile, threading, time, urllib.error, urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
BINARY = pathlib.Path(os.environ.get('BACKUP_PROOF_BINARY', ROOT / 'target/debug/awesometv-server')).resolve()
password = secrets.token_urlsafe(24)
processes = []

def start(data, media, log):
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
    env = {k:v for k,v in os.environ.items() if not k.startswith('AWESOMETV_')}
    env.update(AWESOMETV_BIND='127.0.0.1', AWESOMETV_PORT=str(port), AWESOMETV_DATA_DIR=str(data), AWESOMETV_MEDIA_ROOT=str(media), AWESOMETV_ADMIN_USER='fixture', AWESOMETV_ADMIN_PASSWORD=password, AWESOMETV_PRECACHE_ENABLED='false')
    out = open(log, 'ab'); proc = subprocess.Popen([str(BINARY)], cwd=media.parent, env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True); processes.append((proc,out))
    base = f'http://127.0.0.1:{port}'
    client = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
    def req(path, body=None, method=None, raw=False):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(base+path, data=data, method=method, headers={'Content-Type':'application/json'})
        with client.open(request, timeout=30) as response:
            blob = response.read(); return blob if raw else json.loads(blob)
    deadline = time.monotonic()+30
    while True:
        if proc.poll() is not None: raise RuntimeError('server exited; see '+str(log))
        try:
            # Refuse to send writes to an unrelated listener.
            inodes = {x[9] for l in open('/proc/net/tcp') if len(x:=l.split())>9 and x[1]==f'0100007F:{port:04X}' and x[3]=='0A'}
            owned = any(os.readlink(fd) in {f'socket:[{i}]' for i in inodes} for fd in pathlib.Path(f'/proc/{proc.pid}/fd').iterdir() if fd.exists())
            if owned: req('/healthz',raw=True); break
        except (OSError, urllib.error.URLError): pass
        if time.monotonic()>deadline: raise TimeoutError('readiness')
        time.sleep(.1)
    req('/api/v1/auth/login', {'username':'fixture','password':password})
    return proc, req

def stop(proc):
    os.killpg(proc.pid, signal.SIGTERM); proc.wait(timeout=25)

def snap(req, channel):
    return {'settings': req('/api/v1/settings/language'), 'media': req('/api/v1/media'), 'channels': req('/api/v1/channels'), 'guide': req('/api/v1/channels/'+channel+'/guide?hours=1')}

with tempfile.TemporaryDirectory(prefix='awesome-backup-proof-') as temporary:
    root = pathlib.Path(temporary); media = root/'media'; media.mkdir(); source=root/'source'; dest=root/'dest'
    try:
        subprocess.run([str(ROOT/'scripts/gen-wave1-fixtures.sh'), str(media)], check=True, timeout=90, stdout=subprocess.DEVNULL)
        proc, req = start(source, media, root/'source.log')
        scan = req('/api/v1/scan', {'path':str(media),'kind':'other','limit':2}); assert scan['scanned']==2
        channel = req('/api/v1/channels', {'number':'99','name':'Fixture','media_ids':[m['id'] for m in scan['media']]})['id']
        req('/api/v1/settings/language', {'audio_language':'fra','subtitle_mode':'burn','subtitle_language':'eng'}, 'PUT')
        expected = snap(req, channel); assert expected['guide']
        db = sqlite3.connect(source/'awesometv.db'); db.execute('PRAGMA journal_mode=WAL'); db.execute('CREATE TABLE backup_write_probe (n INTEGER)'); db.commit()
        running = threading.Event(); running.set(); counts=[]
        def writer():
            conn=sqlite3.connect(source/'awesometv.db'); n=0
            while running.is_set():
                conn.execute('INSERT INTO backup_write_probe VALUES (?)',(n,)); conn.commit(); n+=1; time.sleep(.005)
            counts.append(n); conn.close()
        thread=threading.Thread(target=writer); thread.start(); time.sleep(.1)
        try: backup = req('/api/v1/backups', {})
        finally: running.clear(); thread.join(timeout=30)
        assert not thread.is_alive() and counts[0]>0
        path=source/'backups'/backup['name']; digest=hashlib.sha256(path.read_bytes()).hexdigest()
        with sqlite3.connect(f'file:{path}?mode=ro',uri=True) as snapshot:
            assert snapshot.execute('PRAGMA integrity_check').fetchone()==('ok',)
            assert snapshot.execute('select count(*) from backup_write_probe').fetchone()[0]>0
        db.close(); stop(proc)
        proc, restored = start(dest, media, root/'dest.log')
        (dest/'backups').mkdir(exist_ok=True); shutil.copyfile(path, dest/'backups'/backup['name'])
        (dest/'backups'/'truncated.sqlite').write_bytes(path.read_bytes()[:100])
        try: restored('/api/v1/restore', {'name':'truncated.sqlite'}); raise AssertionError('truncated accepted')
        except urllib.error.HTTPError: pass
        assert not (dest/'restore.pending').exists()
        restored('/api/v1/restore', {'name':backup['name']}); stop(proc)
        proc, restored = start(dest, media, root/'dest.log'); actual=snap(restored,channel)
        assert actual['settings']==expected['settings']
        assert {x['id'] for x in actual['media']}=={x['id'] for x in expected['media']}
        assert any(x['id']==channel for x in actual['channels'])
        assert {(x['media_id'],x['start_ms'],x['end_ms']) for x in actual['guide']} & {(x['media_id'],x['start_ms'],x['end_ms']) for x in expected['guide']}
        assert all(pathlib.Path(m['path']).is_file() for m in actual['media'])
        print('WAL snapshot integrity, concurrent writes, fresh restore, settings, library, channel, guide and media paths: PASS')
        print('snapshot sha256:',digest,'writer commits:',counts[0])
        # Download and decode an actual HLS segment, not just the playlist.
        deadline=time.monotonic()+45; segment=None
        while time.monotonic()<deadline:
            playlist=restored('/channel/99.m3u8',raw=True).decode()
            lines=[x for x in playlist.splitlines() if x and not x.startswith('#')]
            if lines:
                try: segment=restored('/channel/'+lines[-1],raw=True); break
                except urllib.error.HTTPError: pass
            time.sleep(1)
        assert segment, 'no HLS segment'; segment_file=root/'segment.ts'; segment_file.write_bytes(segment)
        subprocess.run(['ffmpeg','-v','error','-i',str(segment_file),'-frames:v','1','-f','null','-'],check=True,timeout=30,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        print('restored channel HLS segment decoded: PASS')
    finally:
        for proc,out in processes:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try: proc.wait(timeout=25)
                except subprocess.TimeoutExpired: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
            out.close()
