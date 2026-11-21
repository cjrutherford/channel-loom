import { test, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// Supply a pinned local hls.js bundle; this test never contacts a deployment.
test('live playback survives transient playlist, fragment and offline failures', async ({ page, context }) => {
  test.setTimeout(120000);
  const bundle = process.env.HLS_JS_PATH;
  test.skip(!bundle, 'Set HLS_JS_PATH to a local hls.js bundle');
  const dir = mkdtempSync(join(tmpdir(), 'watch-network-'));
  try {
    execFileSync('python3', ['tests/e2e/channel-performance/marker-fixture.py', join(dir, 'source.mp4'), '90'], { timeout:60000 });
    execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-i',join(dir,'source.mp4'),'-c:v','libx264','-preset','ultrafast','-g','2','-sc_threshold','0','-f','hls','-hls_time','2','-hls_list_size','0','-hls_segment_filename',join(dir,'seg%03d.ts'),join(dir,'all.m3u8')], { timeout:60000 });
    const html = readFileSync('crates/awesometv-server/web/public.html','utf8');
    const hls = readFileSync(bundle!);
    let start = Date.now(), offline = false, failPlaylist = false, failFragment = false;
    let playlistFailures = 0, fragmentFailures = 0, offlineFailures = 0;
    const now = Date.now(); const channels = [{number:'1',name:'Fixture',now:{title:'Marker',start_ms:now-30000,end_ms:now+3600000}}];
    await page.addInitScript(() => { let H: any; (window as any).__players = []; Object.defineProperty(window,'Hls',{configurable:true,get:()=>H,set:v=>{ H = typeof v !== 'function' ? v : new Proxy(v,{construct(target,args){ const h=Reflect.construct(target,args); (window as any).__players.push(h); return h; }}); }}); });
    await page.route('**/*', async route => {
      const url = new URL(route.request().url());
      if (url.hostname === 'cdn.jsdelivr.net') return route.fulfill({contentType:'application/javascript',body:hls});
      if (url.pathname === '/') return route.fulfill({contentType:'text/html',body:html});
      if (url.pathname.endsWith('/channels')) return route.fulfill({json:channels});
      if (url.pathname.endsWith('/guide')) return route.fulfill({json:channels.map(c=>({...c,items:[c.now]}))});
      if (url.pathname.endsWith('/config')) return route.fulfill({json:{}});
      if (url.pathname.startsWith('/channel/')) {
        if (offline) { offlineFailures++; return route.abort('internetdisconnected'); }
        if (url.pathname.endsWith('.m3u8')) {
          if (failPlaylist) { failPlaylist=false; playlistFailures++; return route.abort('failed'); }
          const end = Math.min(44, 10 + Math.floor((Date.now()-start)/2000)); const first = Math.max(0,end-9);
          const lines = ['#EXTM3U','#EXT-X-VERSION:3','#EXT-X-TARGETDURATION:2',`#EXT-X-MEDIA-SEQUENCE:${first}`];
          for(let i=first;i<=end;i++) lines.push('#EXTINF:2.000,',`/channel/seg${String(i).padStart(3,'0')}.ts`);
          return route.fulfill({contentType:'application/vnd.apple.mpegurl',body:lines.join('\n')+'\n'});
        }
        if (failFragment) { failFragment=false; fragmentFailures++; return route.abort('failed'); }
        const file = url.pathname.split('/').pop()!; return route.fulfill({contentType:'video/mp2t',body:readFileSync(join(dir,file))});
      }
      return route.abort();
    });
    start = Date.now(); await page.goto('http://watch.fixture.test/');
    await page.waitForFunction(() => { const v=document.querySelector('video')!; return v.getVideoPlaybackQuality().totalVideoFrames > 2 && !v.paused; }, null, {timeout:30000});
    await page.evaluate(() => { const v=document.querySelector('video')!; let last=v.currentTime; (window as any).__rewinds=[]; v.addEventListener('timeupdate',()=>{ if (v.currentTime < last-10) (window as any).__rewinds.push([last,v.currentTime]); last=v.currentTime; }); });
    const sample = () => page.locator('#player').evaluate((v: HTMLVideoElement) => {
      const canvas=document.createElement('canvas'); canvas.width=320; canvas.height=240; const ctx=canvas.getContext('2d')!; ctx.drawImage(v,0,0,320,240);
      let marker=0; for(let i=0;i<13;i++){ const a=ctx.getImageData(i*24+4,16,1,1).data[0], b=ctx.getImageData(i*24+16,16,1,1).data[0]; if (Math.abs(a-b)<80) throw Error('ambiguous marker'); if(a>b) marker+=2**i; }
      return {marker,time:v.currentTime,frames:v.getVideoPlaybackQuality().totalVideoFrames};
    });
    const before=await sample(); failPlaylist=true; failFragment=true;
    await expect.poll(()=>playlistFailures,{timeout:12000}).toBe(1);
    await expect.poll(()=>fragmentFailures,{timeout:15000}).toBe(1);
    offline=true; await context.setOffline(true);
    await expect.poll(()=>offlineFailures,{timeout:12000}).toBeGreaterThan(0);
    const atRecovery=await sample();
    offline=false; await context.setOffline(false);
    await expect.poll(async()=> (await sample()).frames,{timeout:20000}).toBeGreaterThan(atRecovery.frames+3);
    const after=await sample(); expect(after.marker).toBeGreaterThanOrEqual(before.marker); expect(after.time).toBeGreaterThan(before.time);
    expect(await page.evaluate(() => (window as any).__players.length)).toBe(1);
    expect(await page.evaluate(() => (window as any).__rewinds)).toEqual([]);
    const lag = await page.evaluate(() => { const h=(window as any).__players[0], v=document.querySelector('video')!; return h.liveSyncPosition-v.currentTime; });
    expect(lag).toBeLessThan(10);
    await page.locator('#player').evaluate((v: HTMLVideoElement)=>v.pause());
    failPlaylist=true; await expect.poll(()=>playlistFailures,{timeout:12000}).toBe(2);
    expect(await page.locator('#player').evaluate((v: HTMLVideoElement)=>v.paused)).toBe(true);
  } finally { await context.setOffline(false); rmSync(dir,{recursive:true,force:true}); }
});
