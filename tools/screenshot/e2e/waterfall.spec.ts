// End-to-end smoke: boot ferrited, load wbfm with a SineSource carrier,
// confirm the waterfall actually paints non-zero pixels, capture a PNG
// to test-results/ as an artifact for human eyeballing.

import { test, expect, startPipeline, waitForWaterfall } from './fixtures.js';

test('wbfm + SineSource: waterfall paints a tone', async ({ page, serverUrl }) => {
  await startPipeline(serverUrl, 'wbfm', {
    type: 'SineSource',
    params: {
      rate_hz: 2_400_000,
      center_freq_hz: 100_000_000,
      tone_freq_abs_hz: 100_001_000,
      amplitude: 0.25,
    },
  });

  await page.goto(serverUrl);
  await waitForWaterfall(page, 2000);

  // Snapshot of the whole viewport, plus a tight crop of the waterfall
  // canvas — the canvas crop is what we'd use for visual regression
  // once the layout stabilises.
  await page.screenshot({ path: 'test-results/wbfm-viewport.png' });
  const canvas = page.locator('canvas').first();
  await expect(canvas).toBeVisible();
  await canvas.screenshot({ path: 'test-results/wbfm-canvas.png' });
});
