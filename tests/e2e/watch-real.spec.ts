import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

test.skip(process.env.E2E_ISOLATED !== '1', 'Run through watch-real-harness.py');
for (const width of [375, 1440]) {
  test(`real Watch stream, controls and guide at ${width}px`, async ({ browser }, testInfo) => {
    test.setTimeout(110_000);
    const context = await browser.newContext({ viewport: { width, height: 900 }, hasTouch: width === 375, reducedMotion: 'reduce', serviceWorkers: 'block' });
    const page = await context.newPage();
    const errors: string[] = [];
    page.on('pageerror', e => errors.push(e.message));
    const response = await page.goto('/');
    expect(await response!.text()).toBe(readFileSync('crates/awesometv-server/web/public.html', 'utf8'));
    const channel = page.locator('#rail').getByRole('button', { name: 'Watch Fixture', exact: true });
    await expect(channel).toBeVisible();
    if (width === 375) await channel.tap();
    else { await channel.focus(); await page.keyboard.press('Enter'); }
    const video = page.locator('#player');
    await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.getVideoPlaybackQuality().totalVideoFrames), { timeout: 80_000 }).toBeGreaterThan(5);
    const before = await video.evaluate((v: HTMLVideoElement) => ({ time: v.currentTime, frames: v.getVideoPlaybackQuality().totalVideoFrames, width: v.videoWidth }));
    await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime), { timeout: 15_000 }).toBeGreaterThan(before.time + 1);
    const after = await video.evaluate((v: HTMLVideoElement) => ({ time: v.currentTime, frames: v.getVideoPlaybackQuality().totalVideoFrames }));
    expect(before.width).toBeGreaterThan(0); expect(after.frames).toBeGreaterThan(before.frames);
    await video.focus(); await expect(video).toBeFocused();
    await page.keyboard.press('Space'); await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.paused)).toBe(true);
    await page.keyboard.press('Space'); await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.paused)).toBe(false);
    const guide = page.locator('#guideBody').getByRole('button', { name: 'Watch Second fixture' });
    await guide.focus(); await expect(guide).toBeFocused();
    const manifest = page.waitForResponse(r => r.url().includes('/channel/98.m3u8') && r.ok(), { timeout: 80_000 });
    await page.keyboard.press('Enter'); await expect(page.locator('#channelName')).toHaveText('Second fixture');
    await manifest;
    await expect.poll(() => video.evaluate((v: HTMLVideoElement) => !v.paused && v.readyState >= 2), { timeout: 80_000 }).toBe(true);
    const transitioned = await video.evaluate((v: HTMLVideoElement) => v.currentTime);
    await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime), { timeout: 20_000 }).toBeGreaterThan(transitioned + 0.5);
    await page.screenshot({ path: testInfo.outputPath('real-watch.png'), fullPage: true });
    console.log(`REAL ${width}: ${JSON.stringify({ before, after })}`);
    expect(errors).toEqual([]);
    await context.close();
  });
}

test('retry recovers from a transient channels failure against the real server', async ({ page }) => {
  let fail = true;
  await page.route('**/api/v1/public/channels', route => fail ? route.fulfill({ status: 503 }) : route.continue());
  const response = await page.goto('/');
  expect(await response!.text()).toBe(readFileSync('crates/awesometv-server/web/public.html', 'utf8'));
  await expect(page.locator('#retryChannels')).toBeVisible();
  fail = false;
  await page.locator('#retryChannels').click();
  await expect(page.locator('#rail').getByRole('button', { name: 'Watch Fixture', exact: true })).toBeVisible();
  await expect(page.locator('#retryChannels')).toBeHidden();
});
