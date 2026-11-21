import { test, expect } from '@playwright/test';
import { writeFileSync, mkdirSync } from 'node:fs';
test.use({ trace:'off', screenshot:'off', video:'off' });
const { checkContinuity } = require('./continuity.cjs');

test('isolated decoded marker playback soak', async ({ page, browser }, info) => {
  test.skip(process.env.E2E_CHANNEL_SOAK !== '1', 'only through the isolated soak harness');
  const seconds = Number(process.env.SOAK_SECONDS || 3600);
  test.setTimeout((seconds + 240) * 1000);
  const row = JSON.parse(process.env.SOAK_ROW || 'null');
  if (!row || !Number.isFinite(seconds)) throw new Error('missing persisted broadcast row');
  const identity = JSON.stringify([row[0], row[1], row[3], row[4], row[5], row[6]]);
  const out = process.env.SOAK_OUTPUT!; mkdirSync(out, { recursive: true });
  const events: any[] = []; let dropped = 0; let playlist = ''; let responseErrors = 0;
  const add = (kind: string, data: any) => { events.push({ at: new Date().toISOString(), kind, data }); if (events.length > 600) { events.shift(); dropped++; } };
  page.on('requestfailed', r => { if (!r.failure()?.errorText.includes('ERR_ABORTED')) responseErrors++; add('requestfailed', {url:r.url(), error:r.failure()?.errorText}); });
  page.on('response', async r => {
    if (r.url().includes('.m3u8')) {
      try { const body = await r.text(); playlist = body.slice(-32000); add('playlist', {url:r.url(), status:r.status(), lines:playlist.split('\n').filter(x => x.startsWith('#EXT-X-MEDIA-SEQUENCE') || x.startsWith('#EXT-X-DISCONTINUITY') || x.startsWith('#EXT-X-PROGRAM-DATE-TIME') || (x && !x.startsWith('#'))).slice(-30)}); } catch (e) { add('playlist-read-error', String(e)); }
    }
    if (r.status() >= 400) { responseErrors++; add('http-error', {url:r.url(), status:r.status()}); }
  });
  let summary: any = { identity, row, browser: browser.version(), seconds, stallBudgetSeconds: Number(process.env.SOAK_MAX_STALL_SECONDS || 0), instrumentationGaps: ['hls.js internal reconnect events not instrumented; network requests and failures are recorded', 'server log retained as bounded tail'], result: 'failed or interrupted' };
  try {
    await page.addInitScript(() => {
      const w=window as any; w.__mseEvents=[]; w.__mseDropped=0;
      if (!window.SourceBuffer) { w.__mseUnavailable=true; return; }
      for (const name of ['appendBuffer','remove'] as const) {
        const original=(SourceBuffer.prototype as any)[name];
        (SourceBuffer.prototype as any)[name]=function(...args: any[]) {
          w.__mseEvents.push({name, at:Date.now(), bytes:name==='appendBuffer'?args[0]?.byteLength:undefined, range:name==='remove'?args:undefined});
          if(w.__mseEvents.length>300) { w.__mseEvents.shift(); w.__mseDropped++; }
          return original.apply(this,args);
        };
      }
    });
    await page.goto('/', { waitUntil:'domcontentloaded' });
    await page.locator('#rail button[data-number="99"]').click({ timeout:30000 });
    await page.locator('#player').evaluate((v: HTMLVideoElement) => {
      const w = window as any; w.__soakEvents = [];
      for (const name of ['waiting','playing','stalled','error','seeking','seeked','pause']) v.addEventListener(name, () => { w.__soakEvents.push({name, at:Date.now(), time:v.currentTime}); if (w.__soakEvents.length > 300) w.__soakEvents.shift(); });
    });
    await page.waitForFunction(() => { const v = document.querySelector('#player') as HTMLVideoElement; return v && v.readyState >= 2 && v.getVideoPlaybackQuality().totalVideoFrames > 0; }, null, { timeout:90000 });
    await page.evaluate(() => { (window as any).__soakEvents = []; });
    const start = performance.now(); let previous: any; let firstMarker: number | undefined; let lastFrames = -1; let lastFrameAt = start; let stallSeconds = 0; let lastMarkerChange = start; let samples = 0; let heartbeat = start; let lastSampleAt = start; let waitingAt: number | undefined; let rebufferSeconds = 0; let rebufferCount = 0;
    while ((performance.now()-start)/1000 < seconds) {
      const sample: any = await page.locator('#player').evaluate((v: HTMLVideoElement) => {
        const canvas = document.createElement('canvas'); canvas.width = 320; canvas.height = 240;
        const ctx = canvas.getContext('2d', {willReadFrequently:true})!; ctx.drawImage(v,0,0,320,240);
        const values:number[] = []; for(let i=0;i<26;i++) { const d=ctx.getImageData(i*12+4,16,4,16).data; let sum=0; for(let j=0;j<d.length;j+=4) sum+=(d[j]+d[j+1]+d[j+2])/3; values.push(sum/(d.length/4)); }
        let marker=0; for(let i=0;i<13;i++) { const a=values[2*i], b=values[2*i+1]; if (Math.abs(a-b)<80 || Math.max(a,b)<150 || Math.min(a,b)>90) throw new Error(`ambiguous marker bit ${i}: ${a}/${b}`); if (a>b) marker+=2**i; }
        const q=v.getVideoPlaybackQuality(); const w=window as any; const ev=w.__soakEvents.splice(0);
        return {marker, currentTime:v.currentTime, frames:q.totalVideoFrames, droppedFrames:q.droppedVideoFrames, paused:v.paused, readyState:v.readyState, events:ev, mseEvents:w.__mseEvents.splice(0), mseDropped:w.__mseDropped, mseUnavailable:!!w.__mseUnavailable};
      });
      const now=performance.now(); const delta=(now-lastSampleAt)/1000; lastSampleAt=now;
      if (delta > 4) throw new Error(`missing marker coverage for ${delta.toFixed(1)} seconds`);
      samples++; sample.elapsed=(now-start)/1000; add('sample', sample);
      if (sample.mseUnavailable || sample.mseDropped) throw new Error('MSE instrumentation unavailable or overflowed');
      for (const event of sample.events) {
        if (['seeking','error','pause'].includes(event.name)) throw new Error(`unexpected player event: ${JSON.stringify(event)}`);
        if (event.name === 'waiting' && waitingAt === undefined) { waitingAt=event.at; rebufferCount++; }
        if (event.name === 'playing' && waitingAt !== undefined) { rebufferSeconds+=(event.at-waitingAt)/1000; waitingAt=undefined; }
      }
      const scheduledSecond=(Date.now()-row[3]+row[5])/1000;
      if (sample.marker < row[5]/1000 || sample.marker >= row[6]/1000 || scheduledSecond-sample.marker > 60 || sample.marker-scheduledSecond > 2) throw new Error(`marker outside persisted schedule mapping: ${sample.marker}, scheduled ${scheduledSecond}`);
      if (firstMarker === undefined) firstMarker=sample.marker;
      if (previous) { const issue=checkContinuity(previous,{identity,sourceSeconds:sample.marker},1); if (issue) throw new Error(issue); }
      if (sample.marker !== previous?.sourceSeconds) lastMarkerChange=now;
      if (now-lastMarkerChange > 5000) throw new Error('decoded marker stopped advancing for five seconds');
      if (sample.frames !== lastFrames) { lastFrames=sample.frames; lastFrameAt=now; }
      else if (now-lastFrameAt > 2000) { stallSeconds += delta; add('freeze', {seconds:delta}); }
      if (stallSeconds > summary.stallBudgetSeconds) throw new Error(`freeze budget exceeded: ${stallSeconds.toFixed(2)}s`);
      previous={identity,sourceSeconds:sample.marker};
      if (now-heartbeat>=60000) { console.log(`soak heartbeat ${new Date().toISOString()} elapsed=${sample.elapsed.toFixed(0)} marker=${sample.marker} frames=${sample.frames}`); heartbeat=now; }
      await page.waitForTimeout(1000);
    }
    if (waitingAt !== undefined) rebufferSeconds+=(Date.now()-waitingAt)/1000;
    summary={...summary, elapsedSeconds:(performance.now()-start)/1000, samples, stallSeconds, rebufferCount, rebufferSeconds, responseErrors, droppedEvents:dropped, firstMarker, lastMarker:previous?.sourceSeconds};
    expect(responseErrors, 'network or HTTP failures').toBe(0);
    expect(rebufferSeconds, 'rebuffer budget').toBeLessThanOrEqual(summary.stallBudgetSeconds);
    expect(summary.elapsedSeconds).toBeGreaterThanOrEqual(seconds);
    expect(playlist, 'HLS playlist evidence').toContain('#EXT-X-MEDIA-SEQUENCE');
    expect(previous.sourceSeconds-firstMarker!, 'decoded source progression').toBeGreaterThanOrEqual(Math.max(0,seconds-5));
    summary.result=seconds===3600?'PASS':'SHORT CHECK ONLY';
  } catch (e) {
    summary.error=String(e); await page.screenshot({path: `${out}/failure.png`}).catch(() => {}); throw e;
  } finally {
    summary.droppedEvents=dropped; writeFileSync(`${out}/summary.json`,JSON.stringify(summary,null,2));
    writeFileSync(`${out}/rolling-events.json`,JSON.stringify(events,null,2)); writeFileSync(`${out}/playlist.m3u8`,playlist);
  }
});
