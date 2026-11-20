import { test, expect } from '@playwright/test';
import { writeFileSync } from 'node:fs';

test('sequential cold and warm channel startup measurements', async ({ browser }, testInfo) => {
  test.skip(process.env.E2E_CHANNEL_PERFORMANCE !== '1', 'Set E2E_CHANNEL_PERFORMANCE=1');
  test.setTimeout(20 * 60_000);
  const baseURL = process.env.E2E_BASE_URL || 'http://127.0.0.1:8409';
  const results: any[] = [];
  const limit = process.env.E2E_LOAD_THRESHOLD_MS ? Number(process.env.E2E_LOAD_THRESHOLD_MS) : undefined;
  if (limit !== undefined && (!Number.isFinite(limit) || limit <= 0)) throw new Error('Invalid E2E_LOAD_THRESHOLD_MS');
  const context = await browser.newContext({ baseURL, serviceWorkers: 'block' });
  const page = await context.newPage();
  try {
    await page.goto('/');
    await page.locator('#rail button[data-number]').first().waitFor();
    const numbers = await page.locator('#rail button[data-number]').evaluateAll(es => es.map(e => (e as HTMLElement).dataset.number!));
    expect(numbers.length).toBeGreaterThan(0);
    for (const phase of ['cold', 'warm']) for (const number of numbers) {
      // A fresh browser context cannot make a server-side cache cold; cold means first visit in this run.
      const row: any = { phase, number };
      results.push(row);
      const started = Date.now();
      let requestAt: number | undefined;
      const onRequest = (r: import('@playwright/test').Request) => { if (r.url().includes(`/channel/${encodeURIComponent(number)}.m3u8`) && requestAt === undefined) requestAt = Date.now() - started; };
      page.on('request', onRequest);
      const playlist = page.waitForResponse(r => r.url().includes(`/channel/${encodeURIComponent(number)}.m3u8`), { timeout: 90_000 }).then(r => ({ ms: Date.now() - started, status: r.status() }));
      try {
        await page.locator(`#rail button[data-number=${JSON.stringify(number)}]`).click({ timeout: 10_000 });
        const response = await playlist;
        row.requestMs = requestAt ?? null; row.playlistMs = response.ms; row.playlistStatus = response.status;
        if (response.status < 200 || response.status >= 300) throw new Error(`playlist HTTP ${response.status}`);
        await page.waitForFunction(() => { const v = document.querySelector('video')!; return v.readyState >= 2 && !v.paused && v.getVideoPlaybackQuality().totalVideoFrames > 0; }, null, { timeout: 90_000 });
        await page.locator('#player').evaluate((v: HTMLVideoElement) => new Promise<void>(resolve => v.requestVideoFrameCallback(() => resolve())));
        row.firstDecodedFrameMs = Date.now() - started;
        const before = await page.locator('#player').evaluate((v: HTMLVideoElement) => v.currentTime);
        await page.waitForFunction(t => (document.querySelector('#player') as HTMLVideoElement).currentTime > t + 1, before, { timeout: 20_000 });
        row.usablePlaybackMs = Date.now() - started;
        if (limit !== undefined && row.usablePlaybackMs > limit) row.error = `usable playback exceeds ${limit}ms`;
      } catch (error) { row.error = String(error); await playlist.catch(() => {}); } finally { page.off('request', onRequest); }
    }
  } finally {
    const values = results.map(r => r.usablePlaybackMs).filter(Number.isFinite).sort((a, b) => a - b);
    const percentile = (p: number) => values.length ? values[Math.ceil(values.length * p) - 1] : null;
    writeFileSync(testInfo.outputPath('channel-loads.json'), JSON.stringify({ baseURL, thresholdMs: limit ?? null, browser: browser.version(), node: process.version, build: process.env.E2E_BUILD_ID || 'UNSPECIFIED', values: results, summary: { count: values.length, p50: percentile(.5), p95: percentile(.95), max: values.at(-1) ?? null } }, null, 2));
    await context.close();
  }
  expect(results.filter(r => r.error), 'channel failures; see channel-loads.json').toEqual([]);
});
