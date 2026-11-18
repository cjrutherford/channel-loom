import { expect, test } from "@playwright/test";

test.describe("Watch", () => {
  test("loads channels, guide, player, buffer meter, and channel switching", async ({ page }) => {
    await page.goto("/");
    await expect(page).toHaveTitle(/ChannelLoom/);
    await expect(page.locator(".brand img.brand-wordmark")).toHaveAttribute("alt", "ChannelLoom");

    const rail = page.locator("#rail .rail-card");
    await expect(rail.first()).toBeVisible({ timeout: 30_000 });
    expect(await rail.count()).toBeGreaterThan(0);

    await expect(page.locator("#player")).toBeVisible();
    await expect(page.locator("#bufferOverlay")).toBeAttached();
    await expect(page.locator("#bufferFill")).toBeAttached();
    await expect(page.locator("#guideBody .guide-row").first()).toBeVisible({ timeout: 30_000 });

    const second = rail.nth(1);
    const name = await second.locator(".rail-name").innerText();
    await second.click();
    await expect(page.locator("#channelName")).toHaveText(name);
    await expect(page).toHaveURL(/channel=/);
  });

  test("shows a real buffer meter instead of only a spinner", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#rail .rail-card").first()).toBeVisible({ timeout: 30_000 });
    await expect(page.locator("#bufferOverlay")).toBeHidden();

    // Wait for live playback to actually be underway; otherwise the real
    // `playing` event can land right after the synthetic `waiting` event and
    // hide the meter before the assertion.
    await page.waitForFunction(
      () => {
        const player = document.getElementById("player") as HTMLVideoElement | null;
        return Boolean(player && !player.paused && player.readyState >= 3 && player.currentTime > 0);
      },
      undefined,
      { timeout: 60_000 },
    );
    await page.waitForTimeout(2_000);
    await expect(page.locator("#bufferOverlay")).toBeHidden();

    await page.evaluate(() => {
      document.getElementById("player")?.dispatchEvent(new Event("waiting"));
    });
    await expect(page.locator("#bufferOverlay")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#bufferStats")).toContainText("waiting");
    await expect(page.locator("#bufferFill")).toBeAttached();
    await expect(page.locator(".buffer-track")).toBeVisible();

    await page.evaluate(() => {
      document.getElementById("player")?.dispatchEvent(new Event("playing"));
    });
    await expect(page.locator("#bufferOverlay")).toBeHidden();
  });

  test("public APIs and interoperability endpoints respond", async ({ request }) => {
    const endpoints = [
      "/healthz",
      "/api/v1/public/channels",
      "/api/v1/public/guide",
      "/api/v1/public/config",
      "/discover.json",
      "/lineup.json",
      "/lineup_status.json",
      "/device.xml",
      "/iptv/channels.m3u",
      "/iptv/xmltv.xml",
      "/manifest.webmanifest",
      "/sw.js",
      "/icons/icon-192.png",
      "/icons/channelloom-wordmark.png",
      "/cast-receiver",
    ];
    for (const endpoint of endpoints) {
      const response = await request.get(endpoint);
      expect(response.ok(), `${endpoint} returned ${response.status()}`).toBeTruthy();
    }

    const channels = await (await request.get("/api/v1/public/channels")).json();
    expect(Array.isArray(channels)).toBeTruthy();
    expect(channels.length).toBeGreaterThan(0);
    expect(channels[0]).toHaveProperty("number");
    expect(channels[0]).toHaveProperty("now");

    const guide = await (await request.get("/api/v1/public/guide?hours=6")).json();
    expect(Array.isArray(guide)).toBeTruthy();
    expect(guide[0]).toHaveProperty("items");

    const m3u = await (await request.get("/iptv/channels.m3u")).text();
    expect(m3u).toContain("#EXTM3U");
    const xmltv = await (await request.get("/iptv/xmltv.xml")).text();
    expect(xmltv).toContain("<tv");
    const device = await (await request.get("/device.xml")).text();
    expect(device).toContain("ChannelLoom");
    const lineup = await (await request.get("/lineup.json")).json();
    expect(Array.isArray(lineup)).toBeTruthy();
    expect(lineup[0]).toHaveProperty("GuideNumber");

    const playlist = await request.get("/channel/1.m3u8", { timeout: 60_000 });
    expect(playlist.ok(), `playlist returned ${playlist.status()}`).toBeTruthy();
    expect(await playlist.text()).toContain("#EXTM3U");
  });
});
