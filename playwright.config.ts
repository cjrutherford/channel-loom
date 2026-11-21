import { defineConfig } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { join } from "node:path";

const channel = process.env.E2E_CHANNEL || undefined;
// Playwright cleans outputDir at startup. Keep it and the HTML reporter in a
// fresh, test-owned directory, separate from manually retained test-results.
const scratch = join("/tmp", `awesome-tv-playwright-${randomUUID()}`);

export default defineConfig({
  testDir: "./tests/e2e",
  outputDir: join(scratch, "results"),
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  // Live runs cross the public network, so allow an opt-in retry for
  // transient failures such as ERR_NETWORK_CHANGED.
  retries: Number(process.env.E2E_RETRIES || 0),
  reporter: [["list"], ["html", { open: "never", outputFolder: join(scratch, "report") }]],
  use: {
    baseURL: process.env.E2E_BASE_URL || "http://127.0.0.1:8409",
    ignoreHTTPSErrors: true,
    headless: true,
    viewport: { width: 1440, height: 1000 },
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    channel,
  },
});
