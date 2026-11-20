import { test, expect } from '@playwright/test';
import { readFileSync } from 'fs';

const html = readFileSync('crates/awesometv-server/web/public.html', 'utf8');
test('stale manifests cannot take over a new tune and fatal errors are retryable', async ({ page }) => {
  await page.addInitScript(() => {
    class FakeHls {
      static instances: FakeHls[] = [];
      static Events = { LEVEL_UPDATED: 'level', MANIFEST_PARSED: 'manifest', ERROR: 'error' };
      static ErrorTypes = { NETWORK_ERROR: 'network', MEDIA_ERROR: 'media' };
      static isSupported() { return true; }
      handlers: Record<string, Function> = {};
      destroyed = false;
      constructor() { FakeHls.instances.push(this); }
      on(event: string, cb: Function) { this.handlers[event] = cb; }
      loadSource() {} attachMedia() {} destroy() { this.destroyed = true; }
      startLoad() { throw new Error('fatal network error restarted'); }
      recoverMediaError() {}
    }
    (window as any).Hls = FakeHls;
  });
  await page.route('**/*', async route => {
    const url = route.request().url();
    if (url.endsWith('/')) return route.fulfill({ contentType: 'text/html', body: html });
    if (url.includes('/api/v1/public/channels') && !url.includes('/playout')) return route.fulfill({ json: [{ id:'a', number:'1', name:'One' }, { id:'b', number:'2', name:'Two' }] });
    if (url.includes('/api/')) return route.fulfill({ json: url.includes('guide') ? [] : {} });
    return route.abort();
  });
  await page.goto('http://test.local/');
  await page.waitForFunction(() => (window as any).Hls.instances.length === 1);
  await page.evaluate(() => { (window as any).selectChannel('2', true); });
  await page.waitForFunction(() => (window as any).Hls.instances.length === 2);
  await page.evaluate(() => { const h = (window as any).Hls.instances; h[0].handlers.manifest(); h[0].handlers.error(null, { fatal: true, type: 'network' }); });
  await expect(page.locator('#retryPlayback')).toBeHidden();
  await page.evaluate(() => { const h = (window as any).Hls.instances; h[1].handlers.error(null, { fatal: true, type: 'network' }); });
  await expect(page.locator('#retryPlayback')).toBeVisible();
  await page.evaluate(() => { (window as any).selectChannel('1', true); });
  await page.waitForFunction(() => (window as any).Hls.instances.length === 3);
  await page.evaluate(() => { const h = (window as any).Hls.instances; h[1].handlers.manifest(); h[1].handlers.error(null, { fatal: true, type: 'network' }); });
  await expect(page.locator('#retryPlayback')).toBeHidden();
  await page.locator('#guideToggle').click();
  await expect(page.locator('#guideToggle')).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator('#guidePanel')).toBeHidden();
});
