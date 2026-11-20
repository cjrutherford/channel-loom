import { expect, test } from "@playwright/test";

test("generated media appears in the guide", async ({ page }) => {
  test.skip(process.env.E2E_ISOLATED !== "1", "Run through the isolated smoke lifecycle");
  const response = await page.goto("/");
  expect(response?.ok()).toBeTruthy();
  await expect(page.locator('#guideBody .guide-row[data-number="99"] .guide-block').filter({ hasText: "fixture a" }).first()).toBeVisible();
});
