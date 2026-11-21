'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { config, parsePlaylist, classifyPlaylists, redactedPlaylistBody, safeUri } = require('./external-soak.cjs');
test('requires explicit HTTPS origin and channel', () => {
  assert.throws(() => config({}), /EXTERNAL_BASE_URL/);
  assert.throws(() => config({ EXTERNAL_BASE_URL: 'http://example.test', EXTERNAL_CHANNEL: '1' }), /HTTPS/);
  assert.throws(() => config({ EXTERNAL_BASE_URL: 'https://example.test' }), /EXTERNAL_CHANNEL/);
  assert.equal(config({ EXTERNAL_BASE_URL: 'https://example.test', EXTERNAL_CHANNEL: '7' }).seconds, 3600);
});
test('associates sparse PDT with segments, infers forward only within discontinuities', () => {
  const p = parsePlaylist('#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:12\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n#EXTINF:4,\na.ts?secret=x\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:04Z\n#EXTINF:4,\nb.ts\n#EXTINF:4,\nc.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\nd.ts');
  assert.deepEqual(p.segmentTimeline.map(s => [s.sequence,s.explicitPdt,s.inferredPdt,s.discontinuitySequence]), [[12,null,null,3],[13,'2026-01-01T00:00:04Z',null,3],[14,null,'2026-01-01T00:00:08.000Z',3],[15,null,null,4]]);
  assert.equal(p.segmentTimeline[0].uri, safeUri('a.ts?secret=x'));
});
test('distinguishes shrinking window from a same-segment timestamp rewrite', () => {
  const p = (seq, entries) => parsePlaylist('#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:'+seq+'\n'+entries.map(([uri,date]) => '#EXT-X-PROGRAM-DATE-TIME:'+date+'\n#EXTINF:4,\n'+uri).join('\n'));
  const before = p(10, Array.from({length:24}, (_,i) => [`s${i}`,new Date(Date.UTC(2026,0,1,0,0,i*4)).toISOString()]));
  const next = p(34, [['s24','2026-01-01T00:01:20Z']]);
  assert.equal(classifyPlaylists(before,next).classification, 'window-last-pdt-decrease');
  const rewritten = p(10, [['s0','2026-01-01T00:00:01Z']]);
  assert.equal(classifyPlaylists(before,rewritten).classification, 'same-segment-explicit-rewrite');
});
test('anomaly body preserves ordering with redacted URIs and an explicit size bound', () => {
  const b = redactedPlaylistBody('#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="https://example.test/key?token=secret"\n#EXTINF:4,\nseg.ts?token=secret');
  assert.ok(!b.body.includes('secret')); assert.match(b.body, /#EXTINF:4,\nsha256:/); assert.equal(b.truncated,false);
  assert.equal(redactedPlaylistBody('x'.repeat(30), 10).truncated,true);
});
test('injected network failure and stall are persisted and emitted immediately with redaction and bounded budget', () => {
  const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
  const { createLogger } = require('./external-soak.cjs');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'soak-logger-test-'));
  const fd = fs.openSync(path.join(dir, 'events.jsonl'), 'wx'); const emitted = [];
  try {
    const logger = createLogger(fd, 10000, '1', dir, line => emitted.push(line));
    logger.setState({currentTime:42, buffered:[[40,48]]});
    logger.log('stall-start', { url:'https://example.test/live?token=secret' });
    logger.log('request-failed', { url:'https://example.test/seg?token=secret', failure:'injected' });
    const lines = fs.readFileSync(path.join(dir,'events.jsonl'),'utf8');
    assert.equal(emitted.length, 2); assert.match(lines, /stall-start/); assert.match(lines, /request-failed/); assert.doesNotMatch(lines + emitted.join(''), /secret/);
    assert.equal(JSON.parse(lines.split('\n')[0]).state.currentTime, 42);
    logger.log('sample', {large:'x'.repeat(10001)}); assert.match(logger.failure.message, /budget exceeded/);
  } finally { fs.closeSync(fd); fs.rmSync(dir,{recursive:true}); }
});
test('browser injection emits stall and failed network request before browser closes', async () => {
  const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
  const { chromium } = require('@playwright/test');
  const { createLogger, installBrowserHooks, observeRequestFailures } = require('./external-soak.cjs');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'soak-browser-test-'));
  const fd = fs.openSync(path.join(dir, 'events.jsonl'), 'wx'), emitted = [];
  const logger = createLogger(fd, 100000, '1', dir, line => emitted.push(line));
  let browser;
  try {
    browser = await chromium.launch({headless:true}); const page = await browser.newPage();
    await page.exposeBinding('__externalSoakLog', (_source,type,data) => logger.log(type,data));
    await page.addInitScript(installBrowserHooks); observeRequestFailures(page, logger.log);
    await page.route('https://example.test/**', route => route.abort('failed'));
    await page.goto('data:text/html,<video></video>');
    await page.evaluate(() => { document.querySelector('video').dispatchEvent(new Event('waiting')); fetch('https://example.test/segment?token=secret').catch(() => {}); });
    await page.waitForEvent('requestfailed', {timeout:5000}).catch(() => {});
    await page.waitForFunction(() => true);
    const deadline = Date.now() + 5000;
    while ((!logger.counts['request-failed'] || !logger.counts['stall-start']) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    const lines = fs.readFileSync(path.join(dir,'events.jsonl'),'utf8');
    assert.equal(logger.counts['stall-start'], 1); assert.equal(logger.counts['request-failed'], 1);
    assert.ok(emitted.some(x => x.includes('stall-start'))); assert.ok(emitted.some(x => x.includes('request-failed'))); assert.doesNotMatch(lines + emitted.join(''), /secret/);
  } finally { if (browser) await browser.close(); fs.closeSync(fd); fs.rmSync(dir,{recursive:true}); }
});
