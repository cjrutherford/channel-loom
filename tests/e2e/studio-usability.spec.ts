import { expect, test } from '@playwright/test';
import { readFileSync } from 'fs';

test.use({ hasTouch: true });

for (const width of [320, 375, 768, 1440]) {
  test(`populated Studio at ${width}px`, async ({ page }) => {
    test.skip(process.env.E2E_ISOLATED !== '1');
    await page.setViewportSize({ width, height: 900 });
    const response = await page.goto('/admin?autoplay=0');
    await page.locator('#username').fill('fixture');
    await page.locator('#password').fill(process.env.E2E_PASSWORD!);
    await page.locator('#submitBtn').click();
    await expect(page.locator('#healthText')).toHaveText('SYSTEM ONLINE');
    const html = await page.evaluate(async () => (await fetch('/admin?autoplay=0')).text());
    expect(html).toBe(readFileSync('crates/awesometv-server/web/index.html', 'utf8'));
    if (width === 320) await page.locator('[data-view="library"]').tap();
    else await page.locator('[data-view="library"]').click();
    await page.locator('#mediaSearch').fill('no matching title');
    await expect(page.locator('#mediaTable')).toContainText('No matching media');
    await page.locator('#mediaSearch').fill('');
    const box = page.getByRole('checkbox', { name: /^Select / }).first();
    await expect(box).toBeVisible();
    await box.focus(); await page.keyboard.press('Space');
    const create = page.locator('#createFromSelBtn');
    await create.focus(); await page.keyboard.press('Enter');
    await expect(page.getByRole('dialog', { name: 'New channel' })).toBeVisible();
    await expect(page.locator('#channelNumber')).toBeFocused();
    await page.keyboard.press('Shift+Tab');
    await expect(page.locator('#modalConfirm')).toBeFocused();
    await page.keyboard.press('Escape'); await expect(create).toBeFocused();
    await create.click();
    await page.locator('#channelNumber').fill(String(200 + width));
    await page.locator('#channelName').fill(`Viewport ${width}`);
    await page.locator('#modalConfirm').click();
    await expect(page.locator('#channelCards')).toContainText(`Viewport ${width}`);
    if (width === 320) {
      await page.locator('#autoChannelBtn').focus(); await page.keyboard.press('Enter');
      await page.locator('#autoPreviewBtn').focus(); await page.keyboard.press('Enter');
      await expect(page.locator('#autoChannelPreview')).toContainText('Fixture collection');
    }
    await page.locator('[data-view="library"]').click();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await page.screenshot({ path: `${process.env.STUDIO_EVIDENCE_DIR || '/tmp/wave10-studio-evidence'}/studio-${width}.png`, fullPage: true });
    await page.locator('[data-view="settings"]').click();
    await expect(page.locator('#setAudioLanguage')).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    if (width === 375 && process.env.STUDIO_AXE_PATH) {
      await page.addScriptTag({ path: process.env.STUDIO_AXE_PATH });
      const result = await page.evaluate(async () => (window as any).axe.run(document, { runOnly: { type: 'tag', values: ['wcag2a','wcag2aa','wcag21a','wcag21aa','best-practice'] } }));
      expect(result.violations.filter((v: any) => v.id !== 'skip-link')).toEqual([]);
      await expect(page.locator('.skip-link')).toHaveAttribute('href', '#studio-main');
      await expect(page.locator('#studio-main')).toHaveAttribute('tabindex', '-1');
      console.log('AXE', JSON.stringify({ violations: result.violations.map((v: any) => ({ id:v.id, impact:v.impact, count:v.nodes.length })), incomplete: result.incomplete.map((v: any) => ({ id:v.id, count:v.nodes.length })) }));
    }
  });
}

test('library loading, failure and refresh recovery', async ({ page }) => {
  test.skip(process.env.E2E_ISOLATED !== '1');
  let release!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  let requested!: () => void;
  const started = new Promise<void>(resolve => { requested = resolve; });
  let first = true;
  await page.route('**/api/v1/media', async route => {
    if (first) { first = false; requested(); await gate; }
    await route.fulfill({ status: 503, body: 'unavailable' });
  });
  await page.goto('/admin?autoplay=0');
  await page.locator('#username').fill('fixture');
  await page.locator('#password').fill(process.env.E2E_PASSWORD!);
  await page.locator('#submitBtn').click();
  await started;
  await page.locator('[data-view="library"]').click();
  await expect(page.locator('#mediaTable')).toContainText('Loading library');
  release();
  await expect(page.locator('#mediaTable')).toContainText('Library unavailable');
  await page.unroute('**/api/v1/media');
  await page.locator('#refreshBtn').click();
  await expect(page.getByRole('checkbox', { name: /^Select / }).first()).toBeVisible();
});
