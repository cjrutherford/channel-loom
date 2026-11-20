import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const html = readFileSync('crates/awesometv-server/web/public.html', 'utf8');
const base = 1700000000000;
const channels = ['a', 'b'].map((id, i) => ({ id, number: String(i + 1), name: `Channel ${id}`, now: { title: 'Scheduled show', start_ms: base, end_ms: base + 100000 } }));
const segments = [
  { sequence: '10', start_ms: base, end_ms: base + 10000, source: 'media', title: 'Marker A', media_id: 'a' },
  { sequence: '11', start_ms: base + 10000, end_ms: base + 20000, source: 'slate' },
  { sequence: '12', start_ms: base + 20000, end_ms: base + 30000, source: 'media', title: 'Marker B', media_id: 'b' },
];

test('playhead identity follows bounded published fragments and ignores stale selections', async ({ page }) => {
  await page.addInitScript(({ base }) => {
    class FakeHls {
      static Events = { LEVEL_UPDATED: 'level', MANIFEST_PARSED: 'manifest', ERROR: 'error' };
      static ErrorTypes = { NETWORK_ERROR: 'network', MEDIA_ERROR: 'media' };
      static isSupported() { return true; }
      currentLevel = 0;
      levels = [{ details: { fragments: [0, 1, 2].map(i => ({ sn: 10 + i, start: i * 10, duration: 10, programDateTime: base + i * 10000 })) } }];
      on() {} loadSource() {} attachMedia() {} destroy() {}
    }
    (window as any).Hls = FakeHls;
  }, { base });
  let ready = false;
  let releaseOld: (() => void) | undefined;
  let oldPending = false;
  await page.route('**/*', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path.endsWith('/channels')) return route.fulfill({ json: channels });
    if (path.endsWith('/guide')) return route.fulfill({ json: channels.map(c => ({ ...c, items: [c.now] })) });
    if (path.endsWith('/config')) return route.fulfill({ json: {} });
    if (path.endsWith('/a/playout')) {
      if (oldPending) await new Promise<void>(resolve => { releaseOld = resolve; });
      return route.fulfill({ json: { channel_id: 'a', generation: 'gen-a', state: ready ? 'ready' : 'preparing', segments } }).catch(() => {});
    }
    if (path.endsWith('/b/playout')) return route.fulfill({ json: { channel_id: 'b', generation: 'gen-b', state: 'ready', segments: [] } });
    return route.abort();
  });
  await page.goto('http://watch.fixture.test/');
  await expect(page.locator('#nowTitle')).toHaveText('Preparing');
  ready = true;
  await expect(page.locator('#nowTitle')).toHaveText('Marker A');
  await expect(page.locator('#nextTitle')).toContainText('Scheduled: Scheduled show');
  const setTime = async (time: number) => page.locator('#player').evaluate((v: HTMLVideoElement, t) => { Object.defineProperty(v, 'currentTime', { configurable: true, value: t }); v.dispatchEvent(new Event('timeupdate')); }, time);
  await setTime(11); await expect(page.locator('#nowTitle')).toHaveText('Standby');
  await setTime(21); await expect(page.locator('#nowTitle')).toHaveText('Marker B');
  await setTime(2); await expect(page.locator('#nowTitle')).toHaveText('Marker A');
  await page.evaluate(() => { (0, eval)('state').hls.levels[0].details.fragments[0].programDateTime += 1000; });
  await setTime(2); await expect(page.locator('#nowTitle')).toHaveText('Playback identity unknown');
  await page.evaluate(() => { (0, eval)('state').hls.levels[0].details.fragments[0].programDateTime -= 1000; });
  await setTime(2); await expect(page.locator('#nowTitle')).toHaveText('Marker A');
  await page.locator('#player').evaluate((v: HTMLVideoElement) => v.pause());
  await expect.poll(() => page.locator('#player').evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
  oldPending = true;
  await expect.poll(() => !!releaseOld, { timeout: 6000 }).toBe(true);
  await expect.poll(() => page.locator('#player').evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
  await page.locator('#rail').getByRole('button', { name: 'Watch Channel b', exact: true }).click();
  await expect(page.locator('#nowTitle')).toHaveText('Playback identity unknown');
  releaseOld?.();
  await expect(page.locator('#channelName')).toHaveText('Channel b');
  await expect(page.locator('#nowTitle')).toHaveText('Playback identity unknown');
});
