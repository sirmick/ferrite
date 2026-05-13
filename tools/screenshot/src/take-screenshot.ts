#!/usr/bin/env -S node --import tsx/esm
// Reproducible Ferrite UI screenshot.
//
// Spawns its own ferrited (or attaches to an already-running one with
// --no-spawn), drives the UI via REST + Playwright, captures the
// waterfall + spectrum to a PNG. SineSource by default so this works
// in CI with no SDR hardware.
//
//   pnpm -C tools/screenshot shot                          # → /tmp/ferrite-screenshot.png
//   pnpm -C tools/screenshot shot -- --preset wbfm --output docs/images/wbfm.png
//   pnpm -C tools/screenshot shot -- --no-spawn --port 10000  # attach to ./run.sh

import { chromium, type Page } from '@playwright/test';
import { spawnFerrited, type FerritedHandle } from './ferrited.js';

interface SourceConfig {
  type: string;
  params: Record<string, unknown>;
}

interface Args {
  preset: string;
  source: SourceConfig;
  wait: number;
  output: string;
  port: number | null;
  noSpawn: boolean;
  headed: boolean;
  viewport: { width: number; height: number };
}

const HELP = `Usage: take-screenshot [options]

Options:
  --preset <name>         Preset basename to load (default: wbfm)
  --source <type>         Source block type (default: SineSource)
  --rate-hz <Hz>          Source sample rate (default: 2400000)
  --center-freq <Hz>      Source center frequency (default: 100000000)
  --tone-freq <Hz>        SineSource tone frequency (default: center+1000)
  --source-params <json>  Replace source params with arbitrary JSON
  --wait <seconds>        Extra settle time after pipeline starts (default: 4)
  --output <path>         PNG output path (default: /tmp/ferrite-screenshot.png)
  --port <n>              ferrited port. With --no-spawn, attach to this port
  --no-spawn              Don't spawn ferrited; attach to --port (default 10000)
  --headed                Show the browser (debugging)
  --viewport <WxH>        Browser viewport (default: 1600x1000)
  -h, --help              Show this help`;

function parseArgs(argv: string[]): Args {
  let preset = 'wbfm';
  let sourceType = 'SineSource';
  let rateHz = 2_400_000;
  let centerFreq = 100_000_000;
  let toneFreq: number | null = null;
  let sourceParamsOverride: Record<string, unknown> | null = null;
  let wait = 4;
  let output = '/tmp/ferrite-screenshot.png';
  let port: number | null = null;
  let noSpawn = false;
  let headed = false;
  let viewport = { width: 1600, height: 1000 };

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} requires a value`);
      return v;
    };
    switch (a) {
      case '--preset': preset = next(); break;
      case '--source': sourceType = next(); break;
      case '--rate-hz': rateHz = Number(next()); break;
      case '--center-freq': centerFreq = Number(next()); break;
      case '--tone-freq': toneFreq = Number(next()); break;
      case '--source-params': sourceParamsOverride = JSON.parse(next()); break;
      case '--wait': wait = Number(next()); break;
      case '--output': output = next(); break;
      case '--port': port = Number(next()); break;
      case '--no-spawn': noSpawn = true; break;
      case '--headed': headed = true; break;
      case '--viewport': {
        const m = /^(\d+)x(\d+)$/.exec(next());
        if (!m) throw new Error('--viewport must be WxH (e.g. 1600x1000)');
        viewport = { width: Number(m[1]), height: Number(m[2]) };
        break;
      }
      case '-h':
      case '--help':
        console.log(HELP);
        process.exit(0);
      default:
        throw new Error(`unknown option: ${a}`);
    }
  }

  const params: Record<string, unknown> = sourceParamsOverride ?? (
    sourceType === 'SineSource'
      ? {
          rate_hz: rateHz,
          center_freq_hz: centerFreq,
          tone_freq_abs_hz: toneFreq ?? centerFreq + 1000,
          amplitude: 0.25,
        }
      : { rate_hz: rateHz, center_freq_hz: centerFreq }
  );

  return {
    preset,
    source: { type: sourceType, params },
    wait,
    output,
    port,
    noSpawn,
    headed,
    viewport,
  };
}

async function configurePipeline(baseUrl: string, args: Args): Promise<void> {
  const post = async (path: string, body?: unknown) => {
    const r = await fetch(`${baseUrl}${path}`, {
      method: 'POST',
      headers: body ? { 'content-type': 'application/json' } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) throw new Error(`POST ${path} → ${r.status}: ${await r.text()}`);
    return r;
  };
  const patch = async (path: string, body: unknown) => {
    const r = await fetch(`${baseUrl}${path}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!r.ok) throw new Error(`PATCH ${path} → ${r.status}: ${await r.text()}`);
    return r;
  };

  // Stop anything previously running (no-spawn case may attach to a
  // running pipeline). 409 is fine — pipeline wasn't running.
  await fetch(`${baseUrl}/api/pipeline/stop`, { method: 'POST' }).catch(() => null);

  await post('/api/preset', { name: args.preset });
  await patch('/api/source', args.source);
  await post('/api/pipeline/start');
}

/** Wait until the waterfall canvas has actually painted a non-empty texture. */
async function waitForWaterfall(page: Page, settleMs: number): Promise<void> {
  // Wait for a canvas to mount.
  await page.waitForSelector('canvas', { timeout: 15_000 });

  // Poll a canvas pixel sample. WebGL canvases need preserveDrawingBuffer
  // for readPixels-from-screen; the simplest portable check is to grab
  // the canvas as a data URL and compare against an all-zero baseline.
  // We instead sample via getContext('2d').drawImage but that requires
  // CORS-clean which is fine for same-origin. Use a worker-thread-free
  // poll in the page context.
  await page.waitForFunction(
    () => {
      const canvases = Array.from(document.querySelectorAll('canvas'));
      // Look for one whose backing store is non-zero, then sample a
      // few pixels via a 2D offscreen mirror.
      for (const c of canvases) {
        if (c.width < 32 || c.height < 32) continue;
        try {
          const off = document.createElement('canvas');
          off.width = 16;
          off.height = 16;
          const ctx = off.getContext('2d');
          if (!ctx) continue;
          ctx.drawImage(c, 0, 0, off.width, off.height);
          const data = ctx.getImageData(0, 0, off.width, off.height).data;
          // Non-zero pixel that isn't pure-black background?
          for (let i = 0; i < data.length; i += 4) {
            if (data[i] > 8 || data[i + 1] > 8 || data[i + 2] > 8) return true;
          }
        } catch {
          // tainted (cross-origin) or no context — skip.
        }
      }
      return false;
    },
    null,
    { timeout: 30_000, polling: 250 },
  );

  if (settleMs > 0) await page.waitForTimeout(settleMs);
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));

  let handle: FerritedHandle | null = null;
  let baseUrl: string;
  if (args.noSpawn) {
    baseUrl = `http://127.0.0.1:${args.port ?? 10000}`;
    console.log(`[shot] attaching to ${baseUrl}`);
  } else {
    handle = await spawnFerrited({ port: args.port ?? undefined });
    baseUrl = handle.url;
    console.log(`[shot] spawned ferrited at ${baseUrl}`);
  }

  let exitCode = 0;
  try {
    console.log(`[shot] preset=${args.preset} source=${args.source.type}`);
    await configurePipeline(baseUrl, args);

    const browser = await chromium.launch({
      headless: !args.headed,
      // Headless Chromium gates AudioContext behind a user gesture by
      // default. We don't care about audio here, but the gate also
      // suppresses some worklets the UI mounts at boot — flip it off.
      args: [
        '--autoplay-policy=no-user-gesture-required',
        '--use-gl=swiftshader',
      ],
    });
    try {
      const ctx = await browser.newContext({
        viewport: args.viewport,
        // Single-port plain HTTP loopback, but harmless on https too.
        ignoreHTTPSErrors: true,
      });
      const page = await ctx.newPage();
      page.on('console', (m) => {
        if (m.type() === 'error') console.error(`[browser] ${m.text()}`);
      });

      await page.goto(baseUrl, { waitUntil: 'domcontentloaded' });
      await waitForWaterfall(page, args.wait * 1000);

      await page.screenshot({ path: args.output, fullPage: false });
      console.log(`[shot] saved ${args.output}`);
    } finally {
      await browser.close();
    }
  } catch (e) {
    console.error(e);
    exitCode = 1;
  } finally {
    if (handle) await handle.stop();
  }
  process.exit(exitCode);
}

main();
