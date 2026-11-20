import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
const html = readFileSync('crates/awesometv-server/web/public.html', 'utf8');
const axe = process.env.WATCH_AXE_PATH;
test('Watch populated focused axe scan', async ({ page }) => {
  test.skip(!axe, 'Set WATCH_AXE_PATH to axe-core 4.10.3');
  await page.route('**/*', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path.endsWith('/channels')) return route.fulfill({ json: [{ number: '1', name: 'Fixture', now: null }] });
    if (path.endsWith('/guide')) return route.fulfill({ json: [] });
    if (path.endsWith('/config')) return route.fulfill({ json: {} });
    return route.abort();
  });
  await page.goto('http://watch.fixture.test/');
  await expect(page.locator('#channelStatus')).toHaveText('1 channels available');
  await page.addScriptTag({ path: axe! });
  const result = await page.evaluate(async () => (window as any).axe.run(document, { runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'] } }));
  console.log('AXE', JSON.stringify({ violations: result.violations.map((v: any) => ({ id: v.id, impact: v.impact, nodes: v.nodes.map((n: any) => n.target) })), incomplete: result.incomplete.map((v: any) => v.id), passes: result.passes.length }));
  expect(result.violations).toEqual([]);
});
