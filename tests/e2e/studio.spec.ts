import { expect, test } from "@playwright/test";

const user = process.env.E2E_USER || "admin";
const password = process.env.E2E_PASSWORD;
const basic = password
  ? `Basic ${Buffer.from(`${user}:${password}`).toString("base64")}`
  : "";

test.describe("Studio", () => {
  test.skip(!password, "Set E2E_PASSWORD to run authenticated Studio tests.");

  test.beforeEach(async ({ page }) => {
    await page.goto("/admin");
    if (await page.locator("#username").count()) {
      await page.fill("#username", user);
      await page.fill("#password", password!);
      await page.click("#submitBtn");
      await page.waitForURL("**/admin");
    }
    await expect(page.locator(".crumb")).toContainText("ChannelLoom", { timeout: 30_000 });
    await expect(page.locator("#healthText")).toHaveText("SYSTEM ONLINE", { timeout: 60_000 });
  });

  test("loads every Studio view with real data", async ({ page }) => {
    await expect(page.locator("#statChannels")).not.toHaveText("0", { timeout: 60_000 });
    const views = [
      ["on-air", "On Air"],
      ["channels", "Channels"],
      ["library", "Library"],
      ["guide", "Guide"],
      ["collections", "Collections"],
      ["programming", "Programming"],
      ["sources", "Sources"],
      ["settings", "Settings"],
    ];
    for (const [view, label] of views) {
      await page.click(`a[data-view="${view}"]`);
      await expect(page.locator(`#view-${view}`)).toHaveClass(/active/);
      await expect(page.locator("#crumbView")).toHaveText(label);
    }
  });

  test("previews auto-built channels without creating them", async ({ page }) => {
    await page.click('a[data-view="channels"]');
    await page.click("#autoChannelBtn");
    await page.fill("#autoChannelCount", "2");
    await page.click("#autoPreviewBtn");
    const plans = page.locator("#autoChannelPreview .block-row");
    await expect(plans.first()).toBeVisible({ timeout: 20_000 });
    await expect(plans).toHaveCount(2);
    await expect(page.locator("#autoCreateBtn")).toBeEnabled();
  });

  test("shows a real Studio buffer meter", async ({ page }) => {
    await page.click('a[data-view="on-air"]');
    await page.waitForFunction(
      () => {
        const player = document.getElementById("player") as HTMLVideoElement | null;
        return Boolean(player && !player.paused && player.readyState >= 3 && player.currentTime > 0);
      },
      undefined,
      { timeout: 60_000 },
    );
    await page.waitForTimeout(2_000);
    // The Studio overlay is a genuine stall indicator: it only appears when
    // readyState drops below HAVE_FUTURE_DATA. Simulate a stall by shadowing
    // readyState for the duration of the assertion.
    await page.evaluate(() => {
      const player = document.getElementById("player") as HTMLVideoElement;
      Object.defineProperty(player, "readyState", { configurable: true, get: () => 0 });
      player.dispatchEvent(new Event("waiting"));
    });
    await expect(page.locator("#studioBufferOverlay")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#studioBufferStats")).toContainText("waiting");
    await expect(page.locator("#studioBufferFill")).toBeAttached();
    await page.evaluate(() => {
      const player = document.getElementById("player") as HTMLVideoElement;
      delete (player as unknown as Record<string, unknown>).readyState;
      player.dispatchEvent(new Event("playing"));
    });
    await expect(page.locator("#studioBufferOverlay")).toBeHidden();
  });

  test("authenticated APIs and auto-builder preview respond", async ({ request }) => {
    const headers = { Authorization: basic };
    const endpoints = [
      "/api/v1/auth/me",
      "/api/v1/stats",
      "/api/v1/channels",
      "/api/v1/media",
      "/api/v1/cloud/providers",
      "/api/v1/cloud/metrics",
      "/api/v1/audit",
      "/api/v1/tokens",
      "/metrics",
    ];
    for (const endpoint of endpoints) {
      const response = await request.get(endpoint, { headers });
      expect(response.ok(), `${endpoint} returned ${response.status()}`).toBeTruthy();
    }
    const me = await (await request.get("/api/v1/auth/me", { headers })).json();
    expect(me.role).toBe("admin");

    const preview = await request.post("/api/v1/channels/auto", {
      headers,
      data: { count: 2, items_per_cycle: 2, order: "shuffle", dry_run: true },
    });
    expect(preview.ok()).toBeTruthy();
    const body = await preview.json();
    expect(body.dry_run).toBe(true);
    expect(body.channels.length).toBeGreaterThan(0);
    expect(body.created).toHaveLength(0);
  });

  test("protected endpoints reject anonymous requests", async ({ request }) => {
    const response = await request.get("/api/v1/stats");
    expect(response.status()).toBe(401);
  });
});
