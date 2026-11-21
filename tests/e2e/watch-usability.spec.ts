import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const html = readFileSync('crates/awesometv-server/web/public.html', 'utf8');
const now = Date.now();
const channels = [1, 2].map(n => ({ number: String(n), name: `Channel ${n}`, now: { title: `Program ${n}`, start_ms: now - 60000, end_ms: now + 3600000 } }));
const guide = channels.map(c => ({ ...c, items: [c.now] }));

for (const width of [320, 375, 768, 1440]) {
  test(`Watch populated at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await page.route('**/*', async route => {
      const url = new URL(route.request().url());
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
      if (url.pathname.endsWith('/channels')) return route.fulfill({ json: channels });
      if (url.pathname.endsWith('/guide')) return route.fulfill({ json: guide });
      if (url.pathname.endsWith('/config')) return route.fulfill({ json: {} });
      return route.abort();
    });
    await page.goto('http://watch.fixture.test/');
    await expect(page.locator('#channelStatus')).toHaveText('2 channels available');
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await expect(page.getByRole('button', { name: 'Jump to guide' })).toBeVisible();
    const channel = page.locator('#rail').getByRole('button', { name: 'Watch Channel 2' });
    await channel.focus();
    await expect(channel).toBeFocused();
    expect(await channel.evaluate(el => getComputedStyle(el).outlineStyle)).not.toBe('none');
    await page.keyboard.press('Enter');
    await expect(page.locator('#nowTitle')).toHaveText('Program 2');
    await expect(channel).toBeFocused();
    await expect(channel).toHaveAttribute('aria-pressed', 'true');
    await page.locator('#player').focus();
    await expect(page.locator('#player')).toBeFocused();
    await page.keyboard.press('Space');
    await expect(page.locator('#player')).toBeFocused();
    const unnamed = await page.locator('button:visible').evaluateAll(buttons => buttons.filter(b => !((b as HTMLElement).innerText.trim() || b.getAttribute('aria-label'))).length);
    expect(unnamed).toBe(0);
    const guideButton = page.locator('#guideBody').getByRole('button', { name: 'Watch Channel 1' });
    await guideButton.focus();
    await page.keyboard.press('Enter');
    await expect(page.locator('#nowTitle')).toHaveText('Program 1');
    await page.screenshot({ path: `tests/e2e/watch-evidence/populated-${width}.png`, fullPage: true });
  });
}

test('empty and failure states offer recovery, including touch input', async ({ browser }) => {
  const context = await browser.newContext({ viewport: { width: 375, height: 812 }, hasTouch: true, isMobile: true, reducedMotion: 'reduce', serviceWorkers: 'block' });
  const page = await context.newPage();
  let mode = 'empty';
  await page.route('**/*', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path.endsWith('/channels') || path.endsWith('/guide')) return mode === 'fail' ? route.fulfill({ status: 503 }) : route.fulfill({ json: mode === 'empty' ? [] : path.endsWith('/channels') ? channels : guide });
    if (path.endsWith('/config')) return route.fulfill({ json: {} });
    return route.abort();
  });
  await page.goto('http://watch.fixture.test/');
  await expect(page.locator('#channelStatus')).toHaveText('No channels are live right now.');
  await expect(page.locator('#guideStatus')).toHaveText('No guide listings available.');
  await page.screenshot({ path: 'tests/e2e/watch-evidence/empty-375.png', fullPage: true });
  mode = 'fail';
  await page.reload();
  await expect(page.locator('#retryChannels')).toBeVisible();
  await expect(page.locator('#retryGuide')).toBeVisible();
  await expect(page.locator('#channelStatus')).toHaveText('Could not load channels.');
  await page.screenshot({ path: 'tests/e2e/watch-evidence/failure-375.png', fullPage: true });
  mode = 'populated';
  await page.locator('#retryChannels').tap();
  await page.locator('#retryGuide').tap();
  await expect(page.locator('#channelStatus')).toHaveText('2 channels available');
  await page.locator('#rail').getByRole('button', { name: 'Watch Channel 2' }).tap();
  await expect(page.locator('#nowTitle')).toHaveText('Program 2');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
  await context.close();
});

 test('loading state is announced while fixture requests are pending', async ({ page }) => {
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  await page.route('**/*', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path.endsWith('/channels') || path.endsWith('/guide')) { await pending; return route.fulfill({ json: [] }); }
    if (path.endsWith('/config')) return route.fulfill({ json: {} });
    return route.abort();
  });
  await page.goto('http://watch.fixture.test/', { waitUntil: 'domcontentloaded' });
  await expect(page.locator('#channelStatus')).toHaveText('Loading channels…');
  await expect(page.locator('#guideStatus')).toHaveText('Loading guide…');
  await expect(page.locator('#channelStatus')).toHaveAttribute('role', 'status');
  release();
  await expect(page.locator('#channelStatus')).toHaveText('No channels are live right now.');
});
