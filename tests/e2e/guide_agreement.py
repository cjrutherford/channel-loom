#!/usr/bin/env python3
"""Isolated live guide/SQLite/renewal check. Run after cargo build -p awesometv-server."""
import datetime as dt
import http.cookiejar
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
DAY = 86400000

def main():
    with tempfile.TemporaryDirectory(prefix='guide-agreement-') as tmp:
        p = Path(tmp)
        (p/'media').mkdir(); (p/'data').mkdir()
        subprocess.run([str(ROOT/'scripts/gen-wave1-fixtures.sh'), str(p/'media')], check=True, timeout=90)
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0)); port = s.getsockname()[1]
        env = {k: v for k, v in os.environ.items() if not k.startswith('AWESOMETV_')}
        env.update(AWESOMETV_BIND='127.0.0.1', AWESOMETV_PORT=str(port), AWESOMETV_DATA_DIR=str(p/'data'), AWESOMETV_MEDIA_ROOT=str(p/'media'), AWESOMETV_ADMIN_USER='fixture', AWESOMETV_ADMIN_PASSWORD='temporary-fixture-password', AWESOMETV_PRECACHE_ENABLED='false')
        base = f'http://127.0.0.1:{port}'
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        proc = None
        def launch():
            nonlocal proc
            proc = subprocess.Popen([str(ROOT/'target/debug/awesometv-server')], cwd=p, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            deadline = time.monotonic()+30
            while time.monotonic() < deadline:
                if proc.poll() is not None: raise RuntimeError(f'server exited {proc.returncode}')
                try:
                    with opener.open(base+'/healthz', timeout=1) as r:
                        if r.status == 200: return
                except OSError: time.sleep(.1)
            raise TimeoutError('healthz')
        def stop():
            nonlocal proc
            os.killpg(proc.pid, signal.SIGTERM)
            try: code = proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL); proc.wait(timeout=5); raise
            proc = None
            assert code == 0, code
        def get(path):
            with opener.open(base+path, timeout=15) as r: return r.read()
        def post(path, body):
            req = urllib.request.Request(base+path, data=json.dumps(body).encode(), headers={'Content-Type':'application/json'})
            with opener.open(req, timeout=45) as r: return json.load(r)
        def rows(channel):
            with sqlite3.connect(p/'data/awesometv.db') as db:
                return db.execute('SELECT COALESCE(m.title,ci.title),ci.start_ms,ci.end_ms FROM channel_items ci LEFT JOIN media_items m ON m.id=ci.media_id WHERE ci.channel_id=? ORDER BY ci.start_ms', (channel,)).fetchall()
        def check(channel, number, label):
            before = int(time.time()*1000)
            public_raw = get('/api/v1/public/guide?hours=24')
            xml_raw = get('/iptv/xmltv.xml')
            after = int(time.time()*1000)
            persisted = rows(channel)
            public = next(x for x in json.loads(public_raw) if x['number'] == number)
            xml = ET.fromstring(xml_raw)
            assert any(x.attrib['id'] == 'awesome.'+number for x in xml.findall('channel'))
            def parse(value):
                return int(dt.datetime.strptime(value, '%Y%m%d%H%M%S %z').timestamp()*1000)
            programmes = [(x.findtext('title'), parse(x.attrib['start']), parse(x.attrib['stop'])) for x in xml.findall('programme') if x.attrib['channel'] == 'awesome.'+number]
            guide = [(x['title'], x['start_ms'], x['end_ms']) for x in public['items']]
            # Ignore moving window edges; all intervals well inside both query windows are exact.
            expected = [x for x in persisted if x[1] >= after-3600000+5000 and x[2] <= before+DAY-5000]
            inside = lambda x: x[1] >= after-3600000+5000 and x[2] <= before+DAY-5000
            actual_guide = [x for x in guide if inside(x)]
            actual_xml = [x for x in programmes if inside(x)]
            assert expected and actual_guide == expected, (label, 'public', expected[:3], guide[:3])
            normalized = [(title, start // 1000 * 1000, end // 1000 * 1000) for title, start, end in expected]
            assert actual_xml == normalized, (label, 'xml', normalized[:3], programmes[:3])
            assert all(a[2] == b[1] for a,b in zip(actual_xml,actual_xml[1:])), 'normalized XMLTV gap/overlap'
            assert all(start < end for _,start,end in actual_xml), 'subsecond interval collapsed in XMLTV'
            assert all(a[2] == b[1] for a,b in zip(persisted,persisted[1:])), 'persisted gap/overlap'
            assert any(a <= before < b for _,a,b in expected), 'now not covered'
            assert len(expected) >= 3
            assert parse('20240101053000 +0530') == parse('20240101000000 +0000')
            assert parse('20231231190000 -0500') == parse('20240101000000 +0000')
            artifact = os.environ.get('GUIDE_EVIDENCE_DIR')
            if artifact:
                out = Path(artifact); out.mkdir(parents=True, exist_ok=True)
                (out/f'{label}-public.json').write_bytes(public_raw)
                (out/f'{label}-xmltv.xml').write_bytes(xml_raw)
                (out/f'{label}-timeline.json').write_text(json.dumps(persisted, indent=2))
            print(f'{label}: {len(expected)} intervals (public ms, XMLTV seconds); channel={number}; now={before}; timeline rows={len(persisted)}')
            return persisted
        try:
            launch()
            post('/api/v1/auth/login', {'username':'fixture','password':'temporary-fixture-password'})
            scan = post('/api/v1/scan', {'path':str(p/'media'),'kind':'other','limit':2})
            ids = [x['id'] for x in scan['media']]; assert len(ids) == 2
            with sqlite3.connect(p/'data/awesometv.db') as db: db.execute('UPDATE media_items SET duration_ms=3600000')
            col = post('/api/v1/collections', {'name':'Guide fixture','media_ids':ids})
            result = post('/api/v1/channels/auto', {'collection_ids':[col['id']],'dry_run':False,'order':'sequential'})
            channel = result['created'][0]['id']
            with sqlite3.connect(p/'data/awesometv.db') as db:
                number = db.execute('SELECT number FROM channels WHERE id=?',(channel,)).fetchone()[0]
                boundary = db.execute('SELECT boundary_ms FROM generated_timeline_state WHERE channel_id=?',(channel,)).fetchone()[0]
                target = (int(time.time())//3600+20)*3600000
                shift = boundary-target
                db.execute('UPDATE channel_items SET start_ms=start_ms-?,end_ms=end_ms-? WHERE channel_id=?',(shift,shift,channel))
                db.execute('UPDATE generated_timeline_state SET boundary_ms=boundary_ms-? WHERE channel_id=?',(shift,channel))
            original = check(channel, number, 'before')
            stop(); launch()
            deadline = time.monotonic()+30
            while time.monotonic() < deadline:
                current = rows(channel)
                if len(current)>len(original): break
                time.sleep(.1)
            assert len(current)>len(original), 'renewal did not append'
            assert current[:len(original)] == original, 'renewal altered published timeline'
            renewed = check(channel, number, 'after')
            assert renewed[len(original)][1] == original[-1][2]
            assert renewed[len(original)][1] < int(time.time()*1000)+DAY-5000, 'renewal outside guide window'
            print(f'renewal boundary={original[-1][2]}, rows={len(original)}->{len(renewed)}')
            # XMLTV uses whole seconds; public guide retains exact milliseconds.
            with sqlite3.connect(p/'data/awesometv.db') as db:
                db.execute('UPDATE channel_items SET start_ms=start_ms+123,end_ms=end_ms+123 WHERE channel_id=?', (channel,))
                db.execute('UPDATE generated_timeline_state SET boundary_ms=boundary_ms+123 WHERE channel_id=?', (channel,))
            check(channel, number, 'fractional')
            stop()
        finally:
            if proc is not None:
                os.killpg(proc.pid, signal.SIGKILL); proc.wait(timeout=5)

if __name__ == '__main__': main()
