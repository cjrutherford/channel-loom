#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const dns = require('node:dns').promises;
const crypto = require('node:crypto');
const { performance } = require('node:perf_hooks');
function config(env) {
  if (!env.EXTERNAL_BASE_URL) throw Error('EXTERNAL_BASE_URL is required');
  const url = new URL(env.EXTERNAL_BASE_URL);
  if (url.protocol !== 'https:' || url.username || url.password || url.pathname !== '/' || url.search || url.hash) throw Error('EXTERNAL_BASE_URL must be an HTTPS origin without credentials or path');
  if (!env.EXTERNAL_CHANNEL || !/^[\w.-]+$/.test(env.EXTERNAL_CHANNEL)) throw Error('EXTERNAL_CHANNEL is required (channel number)');
  const seconds = Number(env.EXTERNAL_SECONDS || 3600), budget = Number(env.EXTERNAL_LOG_BYTES || 100 * 1024 * 1024);
  if (!Number.isInteger(seconds) || seconds < 5 || seconds > 3600 || !Number.isSafeInteger(budget) || budget < 10000) throw Error('invalid seconds or log budget');
  return { origin: url.origin, channel: env.EXTERNAL_CHANNEL, seconds, budget, out: env.EXTERNAL_OUTPUT || `external-soak-${Date.now()}` };
}
function safeUri(uri) {
  return 'sha256:' + crypto.createHash('sha256').update(uri).digest('hex');
}
function parsePlaylist(body) {
  const lines = body.split(/\r?\n/); const seq = lines.find(x => x.startsWith('#EXT-X-MEDIA-SEQUENCE:'));
  const disc = lines.find(x => x.startsWith('#EXT-X-DISCONTINUITY-SEQUENCE:'));
  const sequence = seq ? Number(seq.split(':')[1]) : 0;
  let discontinuitySequence = disc ? Number(disc.split(':')[1]) : 0;
  const segmentTimeline = []; let duration = null, programDate = null, discontinuity = false, nextInferred = null;
  for (const line of lines) {
    if (line.startsWith('#EXTINF:')) { const value = Number.parseFloat(line.slice(8)); duration = Number.isFinite(value) ? value : null; }
    else if (line.startsWith('#EXT-X-PROGRAM-DATE-TIME:')) programDate = line.slice(25);
    else if (line === '#EXT-X-DISCONTINUITY') { discontinuity = true; discontinuitySequence++; nextInferred = null; }
    else if (line && !line.startsWith('#')) {
      const explicitMs = programDate === null ? NaN : Date.parse(programDate);
      const inferredPdt = programDate === null && nextInferred !== null ? new Date(nextInferred).toISOString() : null;
      segmentTimeline.push({ sequence: sequence + segmentTimeline.length, uri: safeUri(line), duration, explicitPdt: programDate, inferredPdt, discontinuity, discontinuitySequence });
      const start = Number.isFinite(explicitMs) ? explicitMs : (programDate === null ? nextInferred : null);
      nextInferred = start !== null && duration !== null ? start + duration * 1000 : null;
      duration = null; programDate = null; discontinuity = false;
    }
  }
  return { sequence, programDates: lines.filter(x => x.startsWith('#EXT-X-PROGRAM-DATE-TIME:')).map(x => x.slice(25)), segments: segmentTimeline.map(s => s.uri), segmentTimeline };
}
function classifyPlaylists(before, after) {
  const previous = new Map(before.segmentTimeline.map(s => [`${s.discontinuitySequence}:${s.sequence}:${s.uri}`, s]));
  const rewrites = [];
  for (const s of after.segmentTimeline) {
    const old = previous.get(`${s.discontinuitySequence}:${s.sequence}:${s.uri}`);
    const a = old && (old.explicitPdt || old.inferredPdt), b = s.explicitPdt || s.inferredPdt;
    if (a && b && Number.isFinite(Date.parse(a)) && Number.isFinite(Date.parse(b)) && Date.parse(a) !== Date.parse(b)) rewrites.push({ sequence:s.sequence, uri:s.uri, before:a, after:b, evidence: old.explicitPdt && s.explicitPdt ? 'explicit' : 'includes-inference' });
  }
  const last = p => { const s = p.segmentTimeline.at(-1); return s && (s.explicitPdt || s.inferredPdt); };
  const a = last(before), b = last(after);
  const lastPdtDecrease = !!(a && b && Date.parse(b) < Date.parse(a)); const sequenceDecrease = after.sequence < before.sequence; const discontinuityDecrease = !!(before.segmentTimeline.length && after.segmentTimeline.length && after.segmentTimeline[0].discontinuitySequence < before.segmentTimeline[0].discontinuitySequence);
  return { rewrites, lastPdtDecrease, sequenceDecrease, discontinuityDecrease, classification: sequenceDecrease ? 'sequence-decrease' : discontinuityDecrease ? 'discontinuity-decrease' : rewrites.some(r => r.evidence === 'explicit') ? 'same-segment-explicit-rewrite' : rewrites.length ? 'same-segment-inferred-change' : lastPdtDecrease ? 'window-last-pdt-decrease' : 'none' };
}
function redactedPlaylistBody(body, limit = 128 * 1024) {
  const redacted = body.split(/\r?\n/).map(line => {
    if (line && !line.startsWith('#')) return safeUri(line);
    if (line.startsWith('#') && !/^#(?:EXTM3U|EXTINF:|EXT-X-(?:VERSION:|TARGETDURATION:|MEDIA-SEQUENCE:|DISCONTINUITY(?:-SEQUENCE:)?$|PROGRAM-DATE-TIME:|ENDLIST$))/.test(line)) return '#REDACTED:' + safeUri(line);
    return line.replace(/URI="[^"]*"/gi, match => `URI="${safeUri(match.slice(5,-1))}"`).replace(/https?:\/\/[^\s,]+/gi, safeUri);
  }).join('\n');
  const bytes = Buffer.from(redacted); return { body: bytes.subarray(0, limit).toString('utf8'), truncated: bytes.length > limit, originalBytes: bytes.length };
}

function sanitize(value) {
  if (typeof value === 'string') return value.replace(/https?:\/\/[^\s"'<>]+/gi, m => safeUri(m));
  if (Array.isArray(value)) return value.map(sanitize);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k,v]) => [k, sanitize(v)]));
  return value;
}
function createLogger(fd, budget, channel, out, emit = console.log) {
  let bytes = 0, failure = null, state = null; const counts = {}, rolling = [];
  const live = /^(player-(waiting|stalled|playing|error)|stall-(start|end)|hls-(error|startLoad|recoverMediaError)|request-failed|frame-freeze|time-backward|playlist-anomaly|observer-error)$/;
  function log(type, data) {
    if (failure) return;
    const event = { at:new Date().toISOString(), channel, type, state, data:sanitize(data) };
    const line = JSON.stringify(event) + '\n';
    if (bytes + Buffer.byteLength(line) > budget) { failure = Error('event log budget exceeded; run incomplete'); emit(`observer-error ${failure.message}`); return; }
    try { fs.writeSync(fd,line); bytes += Buffer.byteLength(line); counts[type] = (counts[type] || 0) + 1; rolling.push(event); if (rolling.length > 300) rolling.shift(); if (live.test(type)) emit(JSON.stringify(event).slice(0,2000)); }
    catch (e) { failure = e; emit(`observer-error ${String(e)}`); }
  }
  function status(extra = {}) { fs.writeFileSync(path.join(out,'status.json'), JSON.stringify({updatedAt:new Date().toISOString(),channel,bytes,budget,counts,state,...extra},null,2)); }
  return { log, status, counts, rolling, setState(v) { state = sanitize(v); }, get bytes(){return bytes;}, get failure(){return failure;} };
}
function installBrowserHooks() {
      const emit = (type, data) => { const v = document.querySelector('video'); const state = v ? {currentTime:v.currentTime, paused:v.paused, readyState:v.readyState, buffered:Array.from({length:v.buffered.length},(_,i)=>[v.buffered.start(i),v.buffered.end(i)])} : null; void window.__externalSoakLog(type, { ...data, player:state }); };
      let H; Object.defineProperty(window, 'Hls', { configurable: true, get: () => H, set: value => {
        if (typeof value !== 'function') { H = value; return; }
        H = new Proxy(value, { construct(target, args) {
          const h = Reflect.construct(target, args); emit('hls-created', {});
          for (const key of ['ERROR', 'MANIFEST_LOADING', 'LEVEL_LOADING', 'FRAG_LOADING']) if (target.Events?.[key]) h.on(target.Events[key], (_event, d) => emit('hls-'+key.toLowerCase(), { type:d?.type, details:d?.details, fatal:d?.fatal, reason:d?.reason, url:d?.url, sn:d?.frag?.sn, responseCode:d?.response?.code, retry:d?.retry, retryCount:d?.retryCount, networkDetails:d?.networkDetails?.status }));
          for (const name of ['startLoad', 'recoverMediaError']) { const orig = h[name]; if (typeof orig === 'function') h[name] = function(...a) { emit('hls-'+name, { args:a }); return orig.apply(this,a); }; }
          return h;
        } });
      } });
      document.addEventListener('DOMContentLoaded', () => { const v = document.querySelector('video'); if (!v) return; let stalled = false; v.addEventListener('waiting', () => { if (!stalled) { stalled = true; emit('stall-start', {}); } }); v.addEventListener('playing', () => { if (stalled) { stalled = false; emit('stall-end', {}); } }); for (const name of ['loadstart','waiting','playing','stalled','error','seeking','seeked','pause','ended','emptied']) v.addEventListener(name, () => emit('player-'+name, { time:v.currentTime, error:v.error?.message, readyState:v.readyState })); });
}
function observeRequestFailures(page, log) { page.on('requestfailed', req => log('request-failed', { url:req.url(), failure:req.failure() })); }

async function main() {
  const c = config(process.env); fs.mkdirSync(c.out, { recursive: false });
  const fd = fs.openSync(path.join(c.out, 'events.jsonl'), 'wx'); let failure = null, browser;
  const observer = createLogger(fd, c.budget, c.channel, c.out); const { log, counts, rolling } = observer;
  let interrupted = false; const stop = () => { interrupted = true; }; process.on('SIGINT', stop); process.on('SIGTERM', stop);
  const playlistHistory = new Map(); let playlistOrder = Promise.resolve();
  const pending = new Set(); const track = p => { pending.add(p); p.catch(e => log('observer-error', String(e))).finally(() => pending.delete(p)); };
  let start, measurementElapsed = 0, samples = 0, firstFrames, lastFrames, result = 'incomplete', previousSample, frozen = false, lastHeartbeat = -60, startCounts = {};
  try {
    const addresses = await dns.lookup(new URL(c.origin).hostname, { all: true });
    log('provenance', { observerSha256:crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'), origin: c.origin, channel: c.channel, addresses, node: process.version, proxyEnvironmentPresent: Object.keys(process.env).filter(k => /^(https?_proxy|all_proxy|no_proxy)$/i.test(k)), note: 'DNS lookup is local resolver evidence, not necessarily the browser connection path; proxy values intentionally omitted' });
    const { chromium } = require('@playwright/test'); browser = await chromium.launch({ headless: true });
    log('browser', { version: browser.version() });
    const context = await browser.newContext({ ignoreHTTPSErrors: false, serviceWorkers: 'block' });
    await context.route('**/*', route => { const req = route.request(); if (req.isNavigationRequest() && new URL(req.url()).origin !== c.origin) { log('blocked-navigation', { url:req.url() }); return route.abort(); } return ['GET', 'HEAD'].includes(req.method()) ? route.continue() : (log('blocked-write', { method:req.method(), url:req.url() }), route.abort()); });
    const page = await context.newPage();
    await page.exposeBinding('__externalSoakLog', (_source, type, data) => log(type, data));
    await page.addInitScript(installBrowserHooks);
    page.on('console', msg => log('console', { level: msg.type(), text: msg.text() }));
    page.on('pageerror', e => log('page-error', String(e)));
    observeRequestFailures(page, log);
    page.on('response', response => { playlistOrder = playlistOrder.then(async () => {
      const req = response.request(); const headers = await response.allHeaders();
      const details = { url:response.url(), status:response.status(), method:req.method(), serverAddress:await response.serverAddr(), security:await response.securityDetails(), headers:Object.fromEntries(Object.entries(headers).filter(([k]) => ['server','via','x-cache','x-served-by','cf-ray','date','content-type','location'].includes(k))) };
      log('http-response', details);
      if (response.url().includes('.m3u8') && response.ok()) {
        const body = await response.text(), parsed = parsePlaylist(body);
        const url = new URL(response.url()); url.search = ''; url.hash = '';
        const key = url.href, history = playlistHistory.get(key) || { previous:null, awaitingAfter:false };
        const snapshot = { at:new Date().toISOString(), url:key, parsed, raw:redactedPlaylistBody(body) };
        log('playlist', { url:key, ...parsed });
        if (history.awaitingAfter) { log('playlist-anomaly-after', snapshot); history.awaitingAfter = false; }
        if (history.previous) {
          const comparison = classifyPlaylists(history.previous.parsed, parsed);
          if (comparison.classification !== 'none') { log('playlist-anomaly', { comparison, before:history.previous, current:snapshot }); history.awaitingAfter = true; }
        }
        history.previous = snapshot; playlistHistory.set(key, history);
      }
    }); track(playlistOrder); });
    const url = new URL('/', c.origin); url.searchParams.set('channel', c.channel);
    await page.goto(url.href, { waitUntil:'domcontentloaded', timeout:30000 });
    const card = page.locator(`button[data-number="${c.channel}"]`); await card.waitFor({ timeout:30000 }); await card.click();
    const video = page.locator('#player'); await video.waitFor({timeout:30000});
    await page.waitForFunction(() => { const v = document.querySelector('#player'); return v && !v.paused && v.getVideoPlaybackQuality().totalVideoFrames > 0; }, null, { timeout:120000 });
    start = performance.now(); startCounts = { ...counts }; log('measurement-start', { seconds:c.seconds });
    console.log(`playback-start ${new Date().toISOString()} channel=${c.channel} output=${c.out}`);
    while (!failure && !observer.failure && !interrupted && performance.now() - start < c.seconds * 1000) {
      const sample = await video.evaluate(v => { const q = v.getVideoPlaybackQuality(); return { currentTime:v.currentTime, frames:q.totalVideoFrames, droppedFrames:q.droppedVideoFrames, paused:v.paused, readyState:v.readyState, currentSrc:v.currentSrc, buffered:Array.from({length:v.buffered.length}, (_,i) => [v.buffered.start(i),v.buffered.end(i)]) }; });
      samples++; if (firstFrames === undefined) firstFrames = sample.frames; lastFrames = sample.frames;
      sample.liveEdge = sample.buffered.length ? sample.buffered.at(-1)[1] : null; sample.liveEdgeLag = sample.liveEdge === null ? null : sample.liveEdge - sample.currentTime; observer.setState(sample); log('sample', sample); if (previousSample) { if (sample.currentTime < previousSample.currentTime - 0.1) log('time-backward', {previous:previousSample.currentTime,current:sample.currentTime}); const nowFrozen = !sample.paused && sample.frames === previousSample.frames; if (nowFrozen && !frozen) log('frame-freeze', {previousFrames:previousSample.frames}); if (!nowFrozen && frozen) log('frame-resume', {}); frozen = nowFrozen; } previousSample = sample; const elapsed = Math.floor((performance.now()-start)/1000); if (elapsed - lastHeartbeat >= 60) { lastHeartbeat = elapsed; observer.status({elapsedSeconds:elapsed, phase:'running'}); console.log(`heartbeat ${new Date().toISOString()} elapsed=${elapsed}s channel=${c.channel} events=${Object.values(counts).reduce((a,b)=>a+b,0)}`); }
      await page.waitForTimeout(1000);
    }
    measurementElapsed = (performance.now()-start)/1000;
    if (observer.failure) throw observer.failure; if (failure) throw failure; if (interrupted) throw Error('interrupted');
    if (measurementElapsed < c.seconds || samples < 2 || !(lastFrames > firstFrames)) throw Error('insufficient elapsed time or decoded frame progression');
    result = 'observation complete';
  } catch (e) { failure = e; }
  finally {
    if (browser) await browser.close().catch(() => {});
    await Promise.allSettled([...pending]);
    if (failure) result = 'incomplete';
    const summary = { result, elapsedSeconds:measurementElapsed || (start ? (performance.now()-start)/1000 : 0), samples, firstFrames, lastFrames, error:failure ? String(failure) : null, logBytes:observer.bytes, logBudget:c.budget, counts, measurementCounts:Object.fromEntries(Object.entries(counts).map(([k,v]) => [k, v - (startCounts[k] || 0)])), completedHour:result === 'observation complete' && c.seconds === 3600, uninterrupted:result === 'observation complete' && !['stall-start','frame-freeze','time-backward'].some(k => (counts[k] || 0) > (startCounts[k] || 0)), contentReplay:'unknown', limitations:['No remote server logs or server process inspection', 'No decoded source identity or replay oracle; time, frames, playlist sequence and PDT cannot prove absence of repeated content', 'HLS hooks depend on compatible page and hls.js construction; absence of events is not proof of no reconnect', 'DNS lookup and HTTP/TLS peer evidence do not independently prove every proxy hop'] };
    observer.status({phase:'finished', summary}); fs.writeFileSync(path.join(c.out,'rolling-events.json'), JSON.stringify(rolling,null,2)); fs.writeFileSync(path.join(c.out,'summary.json'), JSON.stringify(summary,null,2)); fs.closeSync(fd);
    console.log(JSON.stringify(summary)); if (failure) process.exitCode = 1;
  }
}
module.exports = { config, parsePlaylist, classifyPlaylists, redactedPlaylistBody, safeUri, createLogger, sanitize, installBrowserHooks, observeRequestFailures };
if (require.main === module) main().catch(e => { console.error(e); process.exitCode = 1; });
