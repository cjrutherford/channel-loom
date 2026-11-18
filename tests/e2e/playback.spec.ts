import { expect, test } from "@playwright/test";

/**
 * Long-running live playback check.
 *
 * The regular suite is intentionally short, so this spec only runs when
 * E2E_PLAYBACK_SECONDS is set. It samples the player clock and fails when the
 * timeline moves backwards, which is the signature of a producer restart
 * resetting the HLS media sequence or MPEG-TS timestamps.
 *
 *   E2E_PLAYBACK_SECONDS=180 corepack pnpm e2e:playback
 */
const seconds = Number(process.env.E2E_PLAYBACK_SECONDS || "0");

test.describe("Live playback continuity", () => {
  test.skip(!seconds, "Set E2E_PLAYBACK_SECONDS to run the long playback check.");

  test("player clock never jumps backwards", async ({ page }) => {
    test.setTimeout((seconds + 90) * 1000);

    await page.goto("/");
    await page.waitForFunction(
      () => {
        const player = document.getElementById("player") as HTMLVideoElement | null;
        return Boolean(player && player.readyState >= 2 && !player.paused);
      },
      undefined,
      { timeout: 90_000 },
    );

    const samples = await page.evaluate(async (durationSeconds) => {
      const player = document.getElementById("player") as HTMLVideoElement;
      const collected: Array<{ at: number; currentTime: number; ahead: number }> = [];
      const started = performance.now();
      while (performance.now() - started < durationSeconds * 1000) {
        let ahead = 0;
        for (let index = 0; index < player.buffered.length; index += 1) {
          const start = player.buffered.start(index);
          const end = player.buffered.end(index);
          if (start <= player.currentTime && player.currentTime <= end) {
            ahead = end - player.currentTime;
          }
        }
        collected.push({
          at: Math.round(performance.now() - started),
          currentTime: player.currentTime,
          ahead,
        });
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
      return collected;
    }, seconds);

    const jumps: Array<{ from: number; to: number; at: number }> = [];
    for (let index = 1; index < samples.length; index += 1) {
      const previous = samples[index - 1];
      const current = samples[index];
      if (current.currentTime < previous.currentTime - 1) {
        jumps.push({
          from: previous.currentTime,
          to: current.currentTime,
          at: current.at,
        });
      }
    }

    expect(samples.length).toBeGreaterThan(seconds);
    expect(jumps, `backward jumps: ${JSON.stringify(jumps)}`).toEqual([]);
    const last = samples[samples.length - 1];
    expect(last.currentTime).toBeGreaterThan(seconds * 0.5);
    expect(last.ahead).toBeGreaterThan(0);
  });
});
