import { defineConfig } from "@playwright/test";

const channel = process.env.E2E_CHANNEL || undefined;

export default defineConfig({
  testDir: "./tests/e2e",
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  // Live runs cross the public network, so allow an opt-in retry for
  // transient failures such as ERR_NETWORK_CHANGED.
  retries: Number(process.env.E2E_RETRIES || 0),
  reporter: [["list"], ["html", { open: "never" }]],
  use: {
    baseURL: process.env.E2E_BASE_URL || "http://127.0.0.1:8409",
    ignoreHTTPSErrors: true,
    headless: true,
    viewport: { width: 1440, height: 1000 },
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    channel: channel as never,
  },
});
