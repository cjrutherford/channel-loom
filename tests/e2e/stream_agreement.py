#!/usr/bin/env python3
"""Isolated live decoded-marker agreement; run after building awesometv-server.

STREAM_EVIDENCE_DIR optionally retains captures, timeline, guide and logs.
"""
import collections
import datetime as dt
import http.cookiejar
import json
import os
from pathlib import Path
import shutil
import re
import threading
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]

def stop_signal(signum, _frame):
    raise SystemExit(128 + signum)

for sig in (signal.SIGINT, signal.SIGTERM):
    signal.signal(sig, stop_signal)

def run_cmd(args, timeout=45, **kwargs):
    return subprocess.run(args, check=True, timeout=timeout, **kwargs)

def owns_listener(pid, port):
    with open('/proc/net/tcp', encoding='ascii') as tcp:
        inodes = {parts[9] for line in tcp if len(parts := line.split()) > 9
                  and parts[1] == f'0100007F:{port:04X}' and parts[3] == '0A'}
    for fd in Path(f'/proc/{pid}/fd').iterdir():
        try:
            if os.readlink(fd) in {f'socket:[{inode}]' for inode in inodes}: return True
        except FileNotFoundError: pass
    return False

def parse_playlist_entries(playlist):
    entries = []
    date = duration = None
    for line in playlist.splitlines():
        if line.startswith('#EXT-X-PROGRAM-DATE-TIME:'):
            date = dt.datetime.fromisoformat(line.split(':', 1)[1].replace('Z', '+00:00')).timestamp() * 1000
        elif line.startswith('#EXTINF:'):
            duration = float(line.split(':', 1)[1].split(',', 1)[0])
        elif line and not line.startswith('#'):
            entries.append({'segment': line, 'pdt_ms': date, 'duration_seconds': duration})
            date = duration = None
    return entries

def main():
    with tempfile.TemporaryDirectory(prefix='stream-agreement-') as tmp:
        p = Path(tmp); media = p/'media'; data = p/'data'
        media.mkdir(); data.mkdir()
        # Source-media timecode: six binary, one-second bits survive transcode.
        bits=','.join(f"drawbox=x={i*40}:y=0:w=40:h=30:color=white:t=fill:enable='mod(floor(t/{2**i}),2)'" for i in range(6))
        for name, color in [('red', 'red'), ('blue', 'blue')]:
            run_cmd(['ffmpeg','-hide_banner','-loglevel','error','-f','lavfi','-i',f'color={color}:s=320x180:r=30:d=60','-f','lavfi','-i','sine=frequency=440:duration=60','-vf',f'drawbox=x=0:y=0:w=240:h=30:color=black:t=fill,{bits}','-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-c:a','aac','-shortest',str(media/f'{name}.mp4')], timeout=60)
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0)); port = s.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        env = {k:v for k,v in os.environ.items() if not k.startswith('AWESOMETV_')}
        env.update(AWESOMETV_BIND='127.0.0.1', AWESOMETV_PORT=str(port), AWESOMETV_PUBLIC_URL=base, AWESOMETV_DATA_DIR=str(data), AWESOMETV_MEDIA_ROOT=str(media), AWESOMETV_ADMIN_USER='fixture', AWESOMETV_ADMIN_PASSWORD='temporary-fixture-password', AWESOMETV_PRECACHE_ENABLED='false')
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        server = capture = None
        receive = []
        delayed_receive = []
        pending = collections.deque()
        receive_done = threading.Event()
        capture_errors = []
        try:
            with (p/'server.log').open('wb') as log:
                server = subprocess.Popen([os.environ.get('E2E_SERVER_BINARY', str(ROOT/'target/debug/awesometv-server'))], cwd=p, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                deadline = time.monotonic()+30
                while True:
                    if server.poll() is not None: raise RuntimeError(f'server exited {server.returncode}')
                    try:
                        if owns_listener(server.pid, port):
                            with opener.open(base+'/healthz', timeout=1) as r:
                                if r.status == 200: break
                    except OSError: pass
                    if time.monotonic()>deadline: raise TimeoutError('server readiness')
                    time.sleep(.2)
                def get(path):
                    with opener.open(base+path, timeout=25) as r: return r.read()
                def post(path, value, method='POST'):
                    req = urllib.request.Request(base+path, data=json.dumps(value).encode(), headers={'Content-Type':'application/json'}, method=method)
                    with opener.open(req, timeout=30) as r: return json.load(r)
                post('/api/v1/auth/login', {'username':'fixture','password':'temporary-fixture-password'})
                scan=post('/api/v1/scan', {'path':str(media),'kind':'other','limit':2})
                ids={Path(x['path']).stem:x['id'] for x in scan['media']}
                assert set(ids)=={'red','blue'}, ids
                # Generated ownership and continuation are created by the real API.
                with sqlite3.connect(data/'awesometv.db') as db: db.execute('UPDATE media_items SET duration_ms=60000')
                col=post('/api/v1/collections', {'name':'Markers','media_ids':[ids['red'],ids['blue']]})
                channel=post('/api/v1/channels/auto', {'collection_ids':[col['id']],'start_number':99,'dry_run':False,'order':'sequential','items_per_cycle':2})['created'][0]['id']
                settings=json.loads(get(f'/api/v1/channels/{channel}/stream'))
                settings.update(width=320,height=240,preset='ultrafast',segment_seconds=2,prebuffer_seconds=0,cache_segments=30)
                post(f'/api/v1/channels/{channel}/stream',settings,'PUT')
                # Stop before aging the disposable persisted epoch; restart triggers
                # the real startup renewal with the saved generator cursor intact.
                os.killpg(server.pid,signal.SIGTERM); server.wait(timeout=15)
                boundary=int(time.time()*1000)+30000
                with sqlite3.connect(data/'awesometv.db') as db:
                    old_boundary=db.execute('SELECT boundary_ms FROM generated_timeline_state WHERE channel_id=?',(channel,)).fetchone()[0]
                    shift=old_boundary-boundary
                    db.execute('UPDATE channel_items SET start_ms=start_ms-?,end_ms=end_ms-? WHERE channel_id=?',(shift,shift,channel))
                    db.execute('UPDATE generated_timeline_state SET boundary_ms=boundary_ms-? WHERE channel_id=?',(shift,channel))
                    original=db.execute('SELECT position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind FROM channel_items WHERE channel_id=? ORDER BY position',(channel,)).fetchall()
                server=subprocess.Popen([os.environ.get('E2E_SERVER_BINARY', str(ROOT/'target/debug/awesometv-server'))], cwd=p, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                deadline=time.monotonic()+30
                while time.monotonic()<deadline:
                    if server.poll() is not None: raise RuntimeError(f'server exited {server.returncode}')
                    try:
                        if owns_listener(server.pid,port):
                            with opener.open(base+'/healthz',timeout=1) as r:
                                if r.status==200: break
                    except OSError: pass
                    time.sleep(.1)
                else: raise TimeoutError('restart readiness')
                deadline=time.monotonic()+25
                while time.monotonic()<deadline:
                    with sqlite3.connect(data/'awesometv.db') as db:
                        renewed=db.execute('SELECT position,media_id,title,start_ms,end_ms,in_ms,out_ms,kind FROM channel_items WHERE channel_id=? ORDER BY position',(channel,)).fetchall()
                    if len(renewed)>len(original): break
                    time.sleep(.1)
                (p/'original-rows.json').write_text(json.dumps(original))
                (p/'renewed-rows.json').write_text(json.dumps(renewed))
                assert len(renewed)>len(original), 'startup renewal did not append'
                assert renewed[:len(original)]==original, 'renewal changed preserved rows'
                before,after=original[-1],renewed[len(original)]
                assert before[4]==after[3]==boundary and before[1]!=after[1], (before,after)
                colors={v:k for k,v in ids.items()}
                first,second=colors[before[1]],colors[after[1]]
                guide=json.loads(get('/api/v1/public/guide?hours=24'))
                actual=next(c for c in guide if c['number']=='99')['items']
                guide_rows=[(x['title'],x['start_ms'],x['end_ms']) for x in actual]
                for row in (before,after):
                    assert (row[2],row[3],row[4]) in guide_rows, (row,guide_rows)
                # Also check persisted/guide intervals at the next UTC date
                # boundary. This is schedule evidence, not live midnight playback.
                midnight=(int(time.time()*1000)//86400000+1)*86400000
                rollover=[r for r in renewed if r[3]<midnight+60000 and r[4]>midnight-60000]
                if midnight < int(time.time()*1000)+86400000-120000:
                    assert rollover and any(r[3]<midnight<r[4] or r[3]==midnight for r in rollover)
                    for row in rollover:
                        assert (row[2],row[3],row[4]) in guide_rows, ('UTC rollover',row)
                    rollover_result='guide matches persisted intervals around next UTC midnight'
                else:
                    rollover_result='skipped: next midnight too close to guide window edge'
                (p/'timeline.json').write_text(json.dumps({'original_count':len(original),'renewed_count':len(renewed),'preserved_tail':before,'appended_head':after,'epoch_shift_ms':shift,'next_utc_midnight_ms':midnight,'rollover_rows':rollover,'rollover_result':rollover_result},indent=2))
                (p/'guide.json').write_text(json.dumps(guide,indent=2))
                # Record receive times against a paired wall/monotonic clock. A
                # packet's receive time is an upper bound, not its air time.
                mono_before=time.monotonic_ns()/1e6; clock_wall=time.time_ns()/1e6; mono_after=time.monotonic_ns()/1e6
                clock_mono=(mono_before+mono_after)/2
                def receive_ts():
                    try:
                        with urllib.request.urlopen(base+'/auto/v99.ts',timeout=15) as response, (p/'capture.ts').open('wb') as out:
                            while time.monotonic()*1000 < clock_mono+50000:
                                chunk=response.read1(1316)
                                if not chunk: break
                                out.write(chunk)
                                stamp=time.monotonic_ns()/1e6
                                receive.append((out.tell(),stamp))
                                pending.append((stamp,chunk))
                    except Exception as e: capture_errors.append(repr(e))
                # An actual client-side FIFO holds each received chunk for 15 s.
                # Its capture follows the same byte stream and exercises the gate.
                delay_ms=15000
                def delayed_delivery():
                    with (p/'delayed-capture.ts').open('wb') as out:
                        while not receive_done.is_set() or pending:
                            if pending and time.monotonic_ns()/1e6 >= pending[0][0]+delay_ms:
                                _, chunk = pending.popleft()
                                out.write(chunk)
                                delayed_receive.append((out.tell(),time.monotonic_ns()/1e6))
                            else: time.sleep(.005)
                delayed=threading.Thread(target=delayed_delivery,daemon=True); delayed.start()
                capture=threading.Thread(target=receive_ts,daemon=True); capture.start()
                segments={}; playlist_trace=[]; deadline=time.monotonic()+48
                while time.monotonic()<deadline:
                    playlist=get('/channel/99.m3u8').decode(); (p/'playlist.m3u8').write_text(playlist)
                    entries=parse_playlist_entries(playlist)
                    playlist_trace.append({'wall_ms': time.time_ns()/1e6, 'monotonic_ms': time.monotonic_ns()/1e6, 'entries': entries})
                    (p/'playlist-trace.json').write_text(json.dumps(playlist_trace,indent=2))
                    for entry in entries:
                        line, date = entry['segment'], entry['pdt_ms']
                        if date is not None and line not in segments:
                            try: blob=get('/channel/'+line)
                            except OSError: continue
                            file=p/f'segment-{len(segments):03}.ts'; file.write_bytes(blob)
                            segments[line]=(date,file.name)
                    if time.time()*1000>boundary+17000: break
                    time.sleep(1)
                capture.join(timeout=18)
                assert not capture.is_alive(), 'TS capture did not finish'
                receive_done.set(); delayed.join(timeout=20)
                assert not delayed.is_alive(), 'delayed delivery did not finish'
                (p/'clock-correlation.json').write_text(json.dumps({'wall_ms':clock_wall,'monotonic_ms':clock_mono,'uncertainty_ms':(mono_after-mono_before)/2}))
                (p/'ts-receive.json').write_text(json.dumps(receive))
                (p/'ts-delayed-receive.json').write_text(json.dumps(delayed_receive))
                assert receive and not capture_errors, capture_errors
                (p/'segments.json').write_text(json.dumps(segments,indent=2))
                def markers(file):
                    result=run_cmd(['ffmpeg','-hide_banner','-loglevel','error','-i',str(file),'-vf','fps=1,scale=1:1','-pix_fmt','rgb24','-f','rawvideo','-'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    raw=result.stdout
                    return ['red' if raw[i]>raw[i+2]*2 else 'blue' if raw[i+2]>raw[i]*2 else 'unknown' for i in range(0,len(raw)-2,3)]
                observed=[]
                for date, name in segments.values():
                    for i, marker in enumerate(markers(p/name)): observed.append((round(date+i*1000),marker))
                # showinfo gives the sampled decoded frames' own PTS; packet
                # positions then associate the transition with a receive event.
                decoded=run_cmd(['ffmpeg','-hide_banner','-loglevel','info','-copyts','-i',str(p/'capture.ts'),'-vf','fps=1,scale=1:1,showinfo','-pix_fmt','rgb24','-f','rawvideo','-'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                raw=decoded.stdout
                ts=['red' if raw[i]>raw[i+2]*2 else 'blue' if raw[i+2]>raw[i]*2 else 'unknown' for i in range(0,len(raw)-2,3)]
                pts=[float(x) for x in re.findall(rb'pts_time:([0-9.]+)',decoded.stderr)]
                packets=json.loads(run_cmd(['ffprobe','-v','error','-select_streams','v:0','-show_packets','-show_entries','packet=pts_time,pos,size','-of','json',str(p/'capture.ts')],stdout=subprocess.PIPE).stdout)['packets']
                (p/'decoded.json').write_text(json.dumps({'hls':observed,'ts':ts,'sample_pts_seconds':pts},indent=2))
                assert observed and all(m in (first,second) for _,m in observed)
                transition=next(t for t,m in observed if m==second)
                assert all(m==first for t,m in observed if t<transition) and all(m==second for t,m in observed if t>=transition)
                assert abs(transition-boundary)<=6000, (transition,boundary)
                assert first in ts and second in ts and len(ts)==len(pts), (ts,pts)
                split=ts.index(second)
                assert all(x==first for x in ts[:split]) and all(x==second for x in ts[split:]), ts
                packet=next(x for x in packets if 'pts_time' in x and 'pos' in x and float(x['pts_time'])>=pts[split])
                pos=int(packet['pos'])
                # ffprobe's size is elementary-stream bytes, not TS byte span.
                # The following video packet's TS position bounds completion.
                next_pos=min(int(x['pos']) for x in packets if 'pos' in x and int(x['pos'])>pos)
                received=next(ms for end,ms in receive if end>=next_pos+188)
                receive_wall=clock_wall+(received-clock_mono)
                startup_ms=receive[0][1]-clock_mono
                # Decode the independent source timecode on preceding frames.
                # This measures media age relative to the persisted schedule,
                # including producer and transport, not first-byte startup.
                stamped=run_cmd(['ffmpeg','-hide_banner','-loglevel','error','-copyts','-i',str(p/'capture.ts'),'-vf','fps=1,scale=32:18','-pix_fmt','rgb24','-f','rawvideo','-'],stdout=subprocess.PIPE,stderr=subprocess.PIPE).stdout
                frame_bytes=32*18*3
                assert len(stamped)//frame_bytes == len(ts)
                def marker_second(frame):
                    return sum((1<<i) for i in range(6) if frame[(3*32+(i*4+2))*3]>150)
                def receipt_at(frame_pts, trace):
                    pkt=next(x for x in packets if 'pts_time' in x and 'pos' in x and float(x['pts_time'])>=frame_pts)
                    nxt=min(int(x['pos']) for x in packets if 'pos' in x and int(x['pos'])>int(pkt['pos']))
                    return clock_wall+(next(ms for end,ms in trace if end>=nxt+188)-clock_mono)
                calibration=[]
                for i in range(split):
                    second_code=marker_second(stamped[i*frame_bytes:(i+1)*frame_bytes])
                    media_wall=before[3]+(second_code*1000-before[5])
                    # Reserve the last five seconds for the transition; neither
                    # the transition frame nor its receipt sets the tolerance.
                    frame_received=receipt_at(pts[i],receive)
                    startup_end=clock_wall+(receive[0][1]-clock_mono)+3000
                    if media_wall < boundary-5000 and frame_received >= startup_end:
                        calibration.append({'index':i,'source_second':second_code,'media_wall_ms':media_wall,'receipt_ms':frame_received,'age_ms':frame_received-media_wall})
                assert len(calibration)>=5, calibration
                assert all(0 <= x['source_second'] < 60 for x in calibration), calibration
                assert all(0 <= b['source_second']-a['source_second'] <= 2 for a,b in zip(calibration,calibration[1:])), calibration
                # Predeclared: measured maximum plus 2 s for one-second source
                # quantisation and decoded-frame sampling. 8 s is a hard ceiling,
                # not tuned to the transition result.
                tolerance=max(x['age_ms'] for x in calibration)+2000
                delayed_wall=receipt_at(pts[split],delayed_receive)
                last_old_second=marker_second(stamped[(split-1)*frame_bytes:split*frame_bytes])
                first_new_second=marker_second(stamped[split*frame_bytes:(split+1)*frame_bytes])
                result={'wall_anchor_ms':clock_wall,'monotonic_anchor_ms':clock_mono,'startup_first_byte_ms':startup_ms,'transition_pts_seconds':pts[split],'packet_pos':pos,'next_video_packet_pos':next_pos,'receive_wall_estimate_ms':receive_wall,'boundary_ms':boundary,'receive_minus_boundary_ms':receive_wall-boundary,'calibration':calibration,'calibration_max_age_ms':max(x['age_ms'] for x in calibration),'sampling_allowance_ms':2000,'tolerance_ms':tolerance,'hard_ceiling_ms':8000,'last_old_source_second':last_old_second,'first_new_source_second':first_new_second,'control_delay_ms':delay_ms,'control_receive_minus_boundary_ms':delayed_wall-boundary,'clock_pairing_uncertainty_ms':(mono_after-mono_before)/2}
                (p/'ts-timing.json').write_text(json.dumps(result,indent=2))
                (p/'ts-receive.json').write_text(json.dumps(receive))
                (p/'ts-delayed-receive.json').write_text(json.dumps(delayed_receive))
                assert not -1000<=delayed_wall-boundary<=tolerance, result
                assert last_old_second >= (before[6]-1000)//1000 and first_new_second <= after[5]//1000+1, result
                assert 0 < tolerance <= 8000, result
                assert -1000<=receive_wall-boundary<=tolerance, result
                print(f'PASS HLS error={transition-boundary} ms; TS age={receive_wall-boundary:.0f} ms; calibration max={result["calibration_max_age_ms"]:.0f} ms; tolerance={tolerance:.0f} ms; delayed control={delayed_wall-boundary:.0f} ms; startup={startup_ms:.0f} ms',flush=True)
        finally:
            if server is not None and server.poll() is None:
                os.killpg(server.pid,signal.SIGTERM)
                try: server.wait(timeout=10)
                except subprocess.TimeoutExpired: os.killpg(server.pid,signal.SIGKILL); server.wait(timeout=5)
            dest=os.environ.get('STREAM_EVIDENCE_DIR')
            if dest:
                out=Path(dest); out.mkdir(parents=True,exist_ok=True)
                for f in p.iterdir():
                    if f.is_file(): shutil.copy2(f,out/f.name)
                if (data/'logs').exists(): shutil.copytree(data/'logs',out/'producer-logs',dirs_exist_ok=True)
                print(f'Evidence: {out}',flush=True)

if __name__=='__main__': main()
