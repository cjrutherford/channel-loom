import http.cookiejar, json, os, signal, socket, sqlite3, subprocess, tempfile, time, urllib.request
from pathlib import Path
def interrupted(signum, _frame):
 raise SystemExit(128 + signum)
for sig in (signal.SIGINT, signal.SIGTERM): signal.signal(sig, interrupted)
root=Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix='wave3-gate-') as t:
 p=Path(t); (p/'media').mkdir(); (p/'data').mkdir(); proc=None
 subprocess.run([str(root/'scripts/gen-wave1-fixtures.sh'),str(p/'media')],check=True,timeout=90)
 with socket.socket() as s:
  s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
 env={k:v for k,v in os.environ.items() if not k.startswith('AWESOMETV_')}
 env.update(AWESOMETV_BIND='127.0.0.1',AWESOMETV_PORT=str(port),AWESOMETV_DATA_DIR=str(p/'data'),AWESOMETV_MEDIA_ROOT=str(p/'media'),AWESOMETV_ADMIN_USER='fixture',AWESOMETV_ADMIN_PASSWORD='temporary-fixture-password',AWESOMETV_PRECACHE_ENABLED='false')
 base=f'http://127.0.0.1:{port}'; opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
 def launch(log):
  global proc
  proc=subprocess.Popen([str(root/'target/debug/awesometv-server')],cwd=p,env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  deadline=time.monotonic()+30
  while time.monotonic()<deadline:
   if proc.poll() is not None: raise RuntimeError(f'server exit {proc.returncode}')
   try:
    with opener.open(base+'/healthz',timeout=1) as r:
     if r.status==200:
      with open('/proc/net/tcp', encoding='ascii') as tcp:
       inodes={parts[9] for line in tcp if len(parts:=line.split())>9 and parts[1]==f'0100007F:{port:04X}' and parts[3]=='0A'}
      fds=Path(f'/proc/{proc.pid}/fd')
      if any(os.readlink(fd) in {f'socket:[{inode}]' for inode in inodes} for fd in fds.iterdir() if fd.exists()): return
   except OSError: pass
   time.sleep(.1)
  raise TimeoutError('healthz')
 def stop():
  global proc
  os.killpg(proc.pid,signal.SIGTERM)
  try: code=proc.wait(timeout=15)
  except subprocess.TimeoutExpired:
   os.killpg(proc.pid,signal.SIGKILL); proc.wait(timeout=5); raise
  print(f'SIGTERM exit={code}, pid={proc.pid}, process_group_gone=',end='')
  try: os.killpg(proc.pid,0); raise RuntimeError('process group remains')
  except ProcessLookupError: print('yes')
  if code != 0: raise RuntimeError('unclean exit')
  proc=None
 def post(path,body):
  req=urllib.request.Request(base+path,data=json.dumps(body).encode(),headers={'Content-Type':'application/json'})
  with opener.open(req,timeout=45) as r: return json.load(r)
 try:
  with (p/'first.log').open('wb') as log:
   launch(log); post('/api/v1/auth/login',{'username':'fixture','password':'temporary-fixture-password'})
   scan=post('/api/v1/scan',{'path':str(p/'media'),'kind':'other','limit':2}); ids=[x['id'] for x in scan['media']]; assert len(ids)==2
   with sqlite3.connect(p/'data/awesometv.db') as db: db.execute('UPDATE media_items SET duration_ms=3600000')
   col=post('/api/v1/collections',{'name':'Wave3 Fixture','media_ids':ids})
   result=post('/api/v1/channels/auto',{'collection_ids':[col['id']],'dry_run':False,'order':'sequential'})
   channel=result['created'][0]['id']; print('fixture scan=2 auto-create=1')
   stop()
  with sqlite3.connect(p/'data/awesometv.db') as db:
   shift=4*86400000
   db.execute('UPDATE channel_items SET start_ms=start_ms-?, end_ms=end_ms-? WHERE channel_id=?',(shift,shift,channel))
   db.execute('UPDATE generated_timeline_state SET boundary_ms=boundary_ms-? WHERE channel_id=?',(shift,channel))
   before=db.execute('SELECT boundary_ms FROM generated_timeline_state WHERE channel_id=?',(channel,)).fetchone()[0]
   count=db.execute('SELECT count(*) FROM channel_items WHERE channel_id=?',(channel,)).fetchone()[0]
   assert before < int(time.time()*1000)+5*86400000
  with (p/'restart.log').open('wb') as log:
   launch(log); deadline=time.monotonic()+30
   while time.monotonic()<deadline:
    with sqlite3.connect(p/'data/awesometv.db') as db:
     after=db.execute('SELECT boundary_ms FROM generated_timeline_state WHERE channel_id=?',(channel,)).fetchone()[0]
     count2=db.execute('SELECT count(*) FROM channel_items WHERE channel_id=?',(channel,)).fetchone()[0]
    if after>=int(time.time()*1000)+7*86400000: break
    time.sleep(.1)
   assert after>=int(time.time()*1000)+7*86400000 and count2>count
   stop()
  text=(p/'restart.log').read_text(); lines=[line for line in text.splitlines() if 'renewal status' in line and channel in line]
  assert lines and 'at_risk=false' in lines[-1] and 'last_error=None' in lines[-1], lines
  print(f'restart renewal rows={count}->{count2}, horizon >=7 days; status:',lines[-1])
 finally:
  if proc is not None:
   os.killpg(proc.pid,signal.SIGKILL); proc.wait(timeout=5)
