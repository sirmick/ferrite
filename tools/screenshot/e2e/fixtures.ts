// Per-worker Playwright fixture: spawn ferrited once, expose its
// baseURL to every test, tear it down at worker shutdown. Tests
// configure the pipeline themselves via REST.

import { test as base, expect } from '@playwright/test';
import { spawnFerrited, type FerritedHandle } from '../src/ferrited.js';

// `baseURL` is a built-in Playwright test option (settable from config
// only), so we surface our own `serverUrl` rather than shadowing it.
interface Fixtures {
  ferrited: FerritedHandle;
  serverUrl: string;
}

export const test = base.extend<object, Fixtures>({
  ferrited: [
    async ({}, use) => {
      const h = await spawnFerrited();
      await use(h);
      await h.stop();
    },
    { scope: 'worker' },
  ],
  serverUrl: [
    async ({ ferrited }, use) => {
      await use(ferrited.url);
    },
    { scope: 'worker' },
  ],
});

export { expect };

/** Drive the REST surface to load a preset + source and start the pipeline. */
export async function startPipeline(
  serverUrl: string,
  preset: string,
  source: { type: string; params: Record<string, unknown> },
): Promise<void> {
  await fetch(`${serverUrl}/api/pipeline/stop`, { method: 'POST' }).catch(() => null);

  const r1 = await fetch(`${serverUrl}/api/preset`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name: preset }),
  });
  if (!r1.ok) throw new Error(`preset ${preset}: ${r1.status} ${await r1.text()}`);

  const r2 = await fetch(`${serverUrl}/api/source`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(source),
  });
  if (!r2.ok) throw new Error(`source ${source.type}: ${r2.status} ${await r2.text()}`);

  const r3 = await fetch(`${serverUrl}/api/pipeline/start`, { method: 'POST' });
  if (!r3.ok) throw new Error(`start: ${r3.status} ${await r3.text()}`);
}

/** Wait until a canvas has painted something non-black. */
export async function waitForWaterfall(
  page: import('@playwright/test').Page,
  settleMs = 1500,
): Promise<void> {
  await page.waitForSelector('canvas', { timeout: 15_000 });
  await page.waitForFunction(
    () => {
      for (const c of Array.from(document.querySelectorAll('canvas'))) {
        if (c.width < 32 || c.height < 32) continue;
        try {
          const off = document.createElement('canvas');
          off.width = 16;
          off.height = 16;
          const ctx = off.getContext('2d');
          if (!ctx) continue;
          ctx.drawImage(c, 0, 0, off.width, off.height);
          const d = ctx.getImageData(0, 0, off.width, off.height).data;
          for (let i = 0; i < d.length; i += 4) {
            if (d[i] > 8 || d[i + 1] > 8 || d[i + 2] > 8) return true;
          }
        } catch {
          // tainted or no 2d context — skip
        }
      }
      return false;
    },
    null,
    { timeout: 30_000, polling: 250 },
  );
  if (settleMs > 0) await page.waitForTimeout(settleMs);
}
