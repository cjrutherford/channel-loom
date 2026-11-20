#!/usr/bin/env python3
"""Exercise one baseline/review image pair with disposable Docker volumes."""
import hashlib, http.cookiejar, json, pathlib, sqlite3, subprocess, sys, tempfile, time, urllib.request, uuid

root = pathlib.Path(__file__).resolve().parents[1]
if len(sys.argv) != 4:
    sys.exit('usage: container-persistence-proof.py BASE_IMAGE REVIEW_IMAGE EVIDENCE_DIR')
old, new, evidence_arg = sys.argv[1:]
evidence = pathlib.Path(evidence_arg).resolve()
evidence.mkdir(parents=True, exist_ok=False, mode=0o700)
media = evidence / 'media'
media.mkdir()
name = 'awesome-s19-' + uuid.uuid4().hex[:10]
containers, volumes = [], []
password = uuid.uuid4().hex

def run(*args, timeout=90):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout).stdout.strip()

def docker(*args):
    return run('docker', *args)

def create(label, image):
    volume, container = name + '-' + label, name + '-' + label
    docker('volume', 'create', volume); volumes.append(volume)
    docker('create', '--name', container, '-p', '127.0.0.1::8409',
           '-e', 'AWESOMETV_BIND=0.0.0.0', '-e', 'AWESOMETV_ADMIN_USER=admin',
           '-e', 'AWESOMETV_ADMIN_PASSWORD=' + password, '-e', 'AWESOMETV_PRECACHE_ENABLED=false',
           '-v', volume + ':/data', '-v', str(media) + ':/media:ro', image)
    containers.append(container)
    return container

def start(container):
    docker('start', container)
    port = docker('port', container, '8409/tcp').rsplit(':', 1)[1]
    base = 'http://127.0.0.1:' + port
    jar = http.cookiejar.CookieJar()
    client = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    def request(path, body=None, method=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(base + path, data=data, method=method, headers={'Content-Type': 'application/json'})
        with client.open(req, timeout=5) as response:
            payload = response.read()
            return (payload.decode() if path == '/healthz' else json.loads(payload)) if payload else None
    deadline = time.monotonic() + 60
    while True:
        try:
            request('/healthz'); break
        except (OSError, ValueError):
            if time.monotonic() > deadline or docker('inspect', '-f', '{{.State.Running}}', container) != 'true':
                raise RuntimeError('readiness failed: ' + container)
            time.sleep(.5)
    request('/api/v1/auth/login', {'username':'admin', 'password':password})
    return request

def snapshot(req, channel):
    settings = req('/api/v1/settings/language')
    media_items = req('/api/v1/media')
    channels = req('/api/v1/channels')
    guide = req('/api/v1/channels/' + channel + '/guide?hours=1')
    assert settings == {'audio_language':'fra', 'subtitle_mode':'burn', 'subtitle_language':'eng'}, settings
    assert len(media_items) == 2 and len(channels) == 1 and channels[0]['id'] == channel
    assert len(guide) > 0, 'empty guide'
    return {'settings':settings, 'media_ids':sorted(x['id'] for x in media_items),
            'channel':{k:channels[0][k] for k in ('id','number','name','start_ms','schedule_mode')},
            'guide':guide}

def schema(container, label):
    directory = evidence / label
    directory.mkdir()
    docker('cp', container + ':/data/.', str(directory))
    with sqlite3.connect('file:' + str(directory / 'awesometv.db') + '?mode=ro', uri=True) as db:
        return db.execute("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name").fetchall()

def check(actual, expected, label):
    assert actual['settings'] == expected['settings'] and actual['media_ids'] == expected['media_ids']
    assert actual['channel'] == expected['channel']
    # The moving guide window drops old entries; compare overlapping immutable entries.
    before = {(x['media_id'],x['start_ms'],x['end_ms'],x['title']) for x in expected['guide']}
    after = {(x['media_id'],x['start_ms'],x['end_ms'],x['title']) for x in actual['guide']}
    assert before & after, 'no matching guide entries: ' + label
    print(label + ': settings, media, channel and overlapping guide entries match', flush=True)

try:
    run(str(root / 'scripts/gen-wave1-fixtures.sh'), str(media), timeout=120)
    run(str(root / 'scripts/validate-wave1-fixtures.py'), str(media), timeout=120)
    baseline = create('baseline', old)
    req = start(baseline)
    scan = req('/api/v1/scan', {'path':'/media','kind':'other','limit':2})
    assert scan['scanned'] == 2
    channel = req('/api/v1/channels', {'number':'99','name':'Fixture','media_ids':[x['id'] for x in scan['media']]})['id']
    req('/api/v1/settings/language', {'audio_language':'fra','subtitle_mode':'burn','subtitle_language':'eng'}, 'PUT')
    expected = snapshot(req, channel)
    (evidence / 'baseline-snapshot.json').write_text(json.dumps(expected, indent=2))
    docker('stop', '-t', '20', baseline)
    backup = evidence / 'backup'; backup.mkdir()
    docker('cp', baseline + ':/data/.', str(backup))
    def digest():
        return {str(p.relative_to(backup)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(backup.rglob('*')) if p.is_file()}
    original = digest(); (evidence / 'backup-sha256.json').write_text(json.dumps(original, indent=2))
    check(snapshot(start(baseline), channel), expected, 'baseline restart')
    docker('stop', '-t', '20', baseline)
    upgraded = create('upgrade', new)
    docker('cp', str(backup) + '/.', upgraded + ':/data')
    check(snapshot(start(upgraded), channel), expected, 'upgrade')
    docker('stop', '-t', '20', upgraded)
    first_schema = schema(upgraded, 'upgrade-first-stop')
    check(snapshot(start(upgraded), channel), expected, 'upgrade restart')
    docker('stop', '-t', '20', upgraded)
    assert schema(upgraded, 'upgrade-second-stop') == first_schema, 'schema changed on second migration startup'
    print('upgrade restart: SQLite schema unchanged', flush=True)
    rollback = create('restore', old)
    docker('cp', str(backup) + '/.', rollback + ':/data')
    check(snapshot(start(rollback), channel), expected, 'restore baseline backup')
    assert digest() == original, 'backup modified'
    print('untouched backup checksum verified', flush=True)
finally:
    for container in containers:
        try:
            logs = subprocess.run(['docker', 'logs', container], capture_output=True, text=True, timeout=30)
            (evidence / (container + '.log')).write_text(logs.stdout + logs.stderr)
        except Exception: pass
        subprocess.run(['docker','rm','-f',container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
    for volume in volumes:
        subprocess.run(['docker','volume','rm',volume], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
