#!/usr/bin/env python3
"""Provision isolated channel and execute opt-in browser soak; bounded cleanup."""
import hashlib, http.cookiejar, json, os, platform, signal, socket, sqlite3, subprocess, sys, tempfile, time, urllib.request
import threading
from pathlib import Path
ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT/'tests/e2e'))
from stream_agreement import owns_listener

def command(args, timeout=30):
    return subprocess.check_output(args, cwd=ROOT, timeout=timeout, stderr=subprocess.STDOUT).decode().strip()
def digest(path):
    h=hashlib.sha256()
    with open(path,'rb') as f:
        for chunk in iter(lambda: f.read(1024*1024), b''): h.update(chunk)
    return h.hexdigest()
def stop(sig, frame): raise SystemExit(128+sig)
for sig in (signal.SIGINT, signal.SIGTERM): signal.signal(sig, stop)

def main():
    seconds=int(os.environ.get('SOAK_SECONDS','3600'))
    if not 5 <= seconds <= 3600: raise ValueError('SOAK_SECONDS must be 5..3600')
    budget=float(os.environ.get('SOAK_MAX_STALL_SECONDS','0'))
    if not 0 <= budget <= 3600: raise ValueError('invalid stall budget')
    binary=Path(os.environ.get('E2E_SERVER_BINARY',ROOT/'target/debug/awesometv-server')).resolve()
    out=Path(os.environ.get('SOAK_ARTIFACT_DIR',ROOT/'test-results'/f'channel-soak-{time.time_ns()}')).resolve()
    out.mkdir(parents=True,exist_ok=True)
    # Shell redirection may create run.log before the harness starts.
    if any(entry.name != 'run.log' or not entry.is_file() for entry in out.iterdir()):
        raise RuntimeError(f'artifact directory must be empty apart from run.log: {out}')
    print(f'Artifacts: {out}',flush=True)
    # Capture a bounded tail rather than allow an hour of unbounded producer logs.
    with tempfile.TemporaryDirectory(prefix='channel-soak-') as tmp:
        p=Path(tmp); media=p/'media'; data=p/'data'; media.mkdir(); data.mkdir()
        server=None; browser=None; row=None; get=None
        meta={'seconds':seconds,'acceptance_eligible':seconds==3600,'stall_budget_seconds':budget,'binary':str(binary),'binary_sha256':digest(binary),'git_head':command(['git','rev-parse','HEAD']),'git_status':command(['git','status','--porcelain=v1'])[:20000], 'platform':platform.platform(),'node':command(['node','--version']),'ffmpeg':command(['ffmpeg','-version']).splitlines()[0], 'cache':'fresh isolated server/database; OS cache uncontrolled'}
        meta['build_provenance_limit']='binary hash identifies executable; source-to-binary correspondence is not independently established'
        (out/'provenance.json').write_text(json.dumps(meta,indent=2))
        try:
            subprocess.run([sys.executable,str(HERE/'marker-fixture.py'),str(media/'marker.mp4'),'4200'],check=True,timeout=600)
            meta['fixture_sha256_before']=digest(media/'marker.mp4')
            with socket.socket() as s: s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
            base=f'http://127.0.0.1:{port}'
            env={k:v for k,v in os.environ.items() if not k.startswith('AWESOMETV_')}
            env.update(AWESOMETV_BIND='127.0.0.1',AWESOMETV_PORT=str(port),AWESOMETV_PUBLIC_URL=base,AWESOMETV_DATA_DIR=str(data),AWESOMETV_MEDIA_ROOT=str(media),AWESOMETV_ADMIN_USER='fixture',AWESOMETV_ADMIN_PASSWORD='temporary-fixture-password',AWESOMETV_PRECACHE_ENABLED='false')
            log_tail=bytearray(); log_total=[0]; log_threads=[]
            def launch():
                proc=subprocess.Popen([str(binary)],cwd=p,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,start_new_session=True)
                def drain():
                    for chunk in iter(lambda: proc.stdout.read(8192), b''):
                        log_total[0]+=len(chunk); log_tail.extend(chunk)
                        if len(log_tail)>1000000: del log_tail[:-1000000]
                thread=threading.Thread(target=drain,daemon=True); thread.start(); log_threads.append(thread)
                return proc
            server=launch()
            opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
            deadline=time.monotonic()+30
            while time.monotonic()<deadline:
                if server.poll() is not None: raise RuntimeError('server exited before readiness')
                try:
                    if owns_listener(server.pid,port) and opener.open(base+'/healthz',timeout=1).status==200: break
                except OSError: pass
                time.sleep(.2)
            else: raise TimeoutError('server readiness/ownership')
            def get(path):
                with opener.open(base+path,timeout=25) as r: return json.load(r)
            def post(path,value,method='POST'):
                req=urllib.request.Request(base+path,data=json.dumps(value).encode(),headers={'Content-Type':'application/json'},method=method)
                with opener.open(req,timeout=30) as r: return json.load(r)
            post('/api/v1/auth/login',{'username':'fixture','password':'temporary-fixture-password'})
            scan=post('/api/v1/scan',{'path':str(media),'kind':'other','limit':2}); ids=[x['id'] for x in scan['media']]
            if len(ids)!=1: raise RuntimeError(f'expected one marker media: {ids}')
            col=post('/api/v1/collections',{'name':'Marker','media_ids':ids})
            channel=post('/api/v1/channels/auto',{'collection_ids':[col['id']],'start_number':99,'dry_run':False,'order':'sequential','items_per_cycle':1})['created'][0]['id']
            settings=get(f'/api/v1/channels/{channel}/stream'); settings.update(width=320,height=240,preset='ultrafast',segment_seconds=2,prebuffer_seconds=0,cache_segments=30)
            post(f'/api/v1/channels/{channel}/stream',settings,'PUT')
            meta['stream_settings']=settings
            # Anchor the unique active row near its start in the disposable DB.
            os.killpg(server.pid,signal.SIGTERM); server.wait(timeout=15)
            now=int(time.time()*1000)
            with sqlite3.connect(data/'awesometv.db') as db:
                current=db.execute('SELECT start_ms FROM channel_items WHERE channel_id=? AND start_ms<=? AND end_ms>?',(channel,now,now)).fetchall()
                if len(current)!=1: raise RuntimeError('cannot anchor unique broadcast')
                shift=(now-30000)-current[0][0]
                db.execute('UPDATE channel_items SET start_ms=start_ms+?,end_ms=end_ms+? WHERE channel_id=?',(shift,shift,channel))
                db.execute('UPDATE generated_timeline_state SET boundary_ms=boundary_ms+? WHERE channel_id=?',(shift,channel))
            server=launch(); deadline=time.monotonic()+30
            while time.monotonic()<deadline:
                if server.poll() is not None: raise RuntimeError('server exited after schedule anchor')
                try:
                    if owns_listener(server.pid,port) and opener.open(base+'/healthz',timeout=1).status==200: break
                except OSError: pass
                time.sleep(.2)
            else: raise TimeoutError('restart readiness/ownership')
            def rows():
                with sqlite3.connect(data/'awesometv.db') as db: return db.execute('SELECT position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind FROM channel_items WHERE channel_id=? ORDER BY position',(channel,)).fetchall()
            before=rows(); guide=get('/api/v1/public/guide?hours=24'); now=int(time.time()*1000)
            active=[r for r in before if r[3]<=now<r[4]]
            if len(active)!=1 or active[0][4]-now < (seconds+180)*1000: raise RuntimeError(f'no single persisted broadcast covers run: {active}')
            row=active[0]; items=next(c for c in guide if c['number']=='99')['items']
            if not any((x['title'],x['start_ms'],x['end_ms'])==(row[2],row[3],row[4]) for x in items): raise RuntimeError('guide does not match persisted row')
            (out/'guide-before.json').write_text(json.dumps({'rows':before,'guide':guide},indent=2))
            childenv=os.environ.copy(); childenv.update(E2E_BASE_URL=base,SOAK_SECONDS=str(seconds),SOAK_ROW=json.dumps(row),SOAK_OUTPUT=str(out),SOAK_MAX_STALL_SECONDS=str(budget),E2E_CHANNEL_SOAK='1')
            browser=subprocess.Popen([str(ROOT/'node_modules/.bin/playwright'),'test','tests/e2e/channel-performance/channel-soak.spec.ts','--workers=1','--retries=0','--reporter=line','--output',str(p/'playwright-output')],cwd=ROOT,env=childenv,start_new_session=True)
            try: code=browser.wait(timeout=seconds+300)
            except subprocess.TimeoutExpired: raise TimeoutError('outer browser deadline exceeded')
            if code: raise RuntimeError(f'browser exited {code}')
            after=rows(); (out/'guide-after.json').write_text(json.dumps({'rows':after,'guide':get('/api/v1/public/guide?hours=24')},indent=2))
            if row not in after: raise RuntimeError('persisted broadcast changed during run')
            meta['fixture_sha256_after']=digest(media/'marker.mp4')
            if meta['fixture_sha256_before']!=meta['fixture_sha256_after']: raise RuntimeError('fixture changed during run')
            meta['result']='PASS' if seconds==3600 else 'SHORT CHECK ONLY'
            print(meta['result'],flush=True)
        finally:
            if browser and browser.poll() is None:
                os.killpg(browser.pid,signal.SIGTERM)
                try: browser.wait(timeout=10)
                except subprocess.TimeoutExpired: os.killpg(browser.pid,signal.SIGKILL); browser.wait(timeout=5)
            if row is not None and server and server.poll() is None:
                try:
                    (out/'guide-after.json').write_text(json.dumps({'rows':rows(),'guide':get('/api/v1/public/guide?hours=24')},indent=2))
                except Exception as error: meta['after_guide_error']=str(error)
            if (media/'marker.mp4').exists(): meta['fixture_sha256_after']=digest(media/'marker.mp4')
            if server and server.poll() is None:
                os.killpg(server.pid,signal.SIGTERM)
                try: server.wait(timeout=10)
                except subprocess.TimeoutExpired: os.killpg(server.pid,signal.SIGKILL); server.wait(timeout=5)
            if server:
                for thread in log_threads: thread.join(timeout=2)
                (out/'server-tail.log').write_bytes(log_tail)
                meta['server_log_truncated']=log_total[0]>1000000
            (out/'provenance.json').write_text(json.dumps(meta,indent=2))
if __name__=='__main__': main()
