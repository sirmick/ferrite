#!/usr/bin/env -S node --import tsx/esm
// One-off: produce the docs/images/screenshots/* set against the already-
// running dev stack (./run.sh on :10000/10001/10002 with SDRplay attached).
//
// Scenes:
//   1. fm-settings.png        — wbfm @ 100 MHz on SDRplay, Settings tab,
//                                Noise Reduction + Input sections expanded
//   2. ai-amsw-scan.png       — AI tab after "Let's scan the AM/SW bands…"
//   3. ai-pager-decode.png    — AI tab after "Let's decode some pager data…"
//
// Run with: pnpm -C tools/screenshot exec tsx src/capture-set.ts

import { chromium, type Page } from '@playwright/test';
import * as fs from 'node:fs';
import * as path from 'node:path';

const UI_URL = process.env.FERRITE_UI_URL ?? 'https://127.0.0.1:10000';
const API_URL = process.env.FERRITE_API_URL ?? 'http://127.0.0.1:10001';
const OUT_DIR = path.resolve(process.cwd(), '../../docs/images/screenshots');
const VIEWPORT = { width: 1920, height: 1200 };

// Resolved at runtime from GET /api/devices (see resolveSdrplayArgs)
// so the scenes run on any connected RSP, not just the one this file
// was first written against. Override with FERRITE_SDRPLAY_ARGS.
let SDRPLAY_ARGS = process.env.FERRITE_SDRPLAY_ARGS ?? '';

async function api(method: string, route: string, body?: unknown): Promise<unknown> {
  const r = await fetch(`${API_URL}${route}`, {
    method,
    headers: body ? { 'content-type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) throw new Error(`${method} ${route} → ${r.status}: ${await r.text()}`);
  const txt = await r.text();
  return txt ? JSON.parse(txt) : null;
}

// Build the SoapySDR args string for the connected SDRplay by reading
// GET /api/devices. Every entry (available or not) carries `info`
// with the driver + the raw arg map; we reconstruct the same
// `k=v,k=v` form `DeviceInfo::args_string()` produces server-side.
async function resolveSdrplayArgs(): Promise<string> {
  if (SDRPLAY_ARGS) return SDRPLAY_ARGS;
  const devices = (await api('GET', '/api/devices')) as Array<{
    info?: { driver?: string; args?: Record<string, string> };
  }>;
  const sdrplay = devices.find((d) => d.info?.driver === 'sdrplay');
  if (!sdrplay?.info?.args) {
    throw new Error(
      'no sdrplay device in GET /api/devices — connect an RSP or set FERRITE_SDRPLAY_ARGS',
    );
  }
  SDRPLAY_ARGS = Object.entries(sdrplay.info.args)
    .map(([k, v]) => `${k}=${v}`)
    .join(',');
  return SDRPLAY_ARGS;
}

async function ensurePipeline(preset: string, source: {
  type: string;
  params: Record<string, unknown>;
}): Promise<void> {
  await fetch(`${API_URL}/api/pipeline/stop`, { method: 'POST' }).catch(() => null);
  await api('POST', '/api/preset', { name: preset });
  await api('PATCH', '/api/source', source);
  await api('POST', '/api/pipeline/start');
}

const TAB_LABELS = {
  bands: 'Bands',
  catalog: 'Catalog',
  settings: 'Settings',
  logs: 'Logs',
  flow: 'Flow',
  ai: 'AI',
} as const;

async function clickTab(page: Page, name: keyof typeof TAB_LABELS): Promise<void> {
  await page.getByRole('button', { name: TAB_LABELS[name], exact: true }).first().click();
  await page.waitForTimeout(300);
}

async function expandDetails(page: Page, summaryText: string): Promise<void> {
  const detail = page.locator(`details:has(summary:has-text("${summaryText}"))`).first();
  const open = await detail.evaluate((el) => (el as HTMLDetailsElement).open);
  if (!open) {
    await detail.locator('summary').first().click();
    await page.waitForTimeout(250);
  }
}

/** Give the waterfall time to scroll history into view. Pixel-counting
 *  is unreliable for live RF (lots of dark space between stations), so
 *  this is just a fixed wait against the canvas being mounted. */
async function waitForWaterfallFilled(page: Page, ms = 15_000): Promise<void> {
  await page.waitForSelector('canvas', { timeout: 15_000 });
  await page.waitForTimeout(ms);
}

const AI_TEXTAREA = 'textarea[placeholder*="Ask the AI"]';

async function sendAiPrompt(page: Page, prompt: string): Promise<void> {
  await page.waitForSelector(AI_TEXTAREA, { timeout: 10_000 });
  // Wait until the textarea is enabled (sidecar connected).
  await page.waitForFunction(
    (sel) => {
      const t = document.querySelector(sel) as HTMLTextAreaElement | null;
      return !!t && !t.disabled;
    },
    AI_TEXTAREA,
    { timeout: 30_000, polling: 250 },
  );
  await page.fill(AI_TEXTAREA, prompt);
  await page.press(AI_TEXTAREA, 'Enter');
}

/** Wait for the AI turn to complete. The Stop button is `disabled={!ai.isStreaming()}`,
 *  so when it goes disabled the most recent assistant turn has settled.
 *  We also require ≥1 second of stability — the AI can briefly pause between
 *  tool calls and we don't want to snap during a tool-result gap. */
async function waitForAiIdle(page: Page, maxMs = 30 * 60_000): Promise<void> {
  const stopBtn = page.locator('button[title="Stop"], button:has-text("Stop"):not(:has-text("Stopped"))').first();
  // First, wait for streaming to start (Stop becomes enabled).
  await page.waitForFunction(
    () => {
      const btns = Array.from(document.querySelectorAll('button'));
      return btns.some((b) => /^stop$/i.test(b.textContent?.trim() ?? '') && !(b as HTMLButtonElement).disabled);
    },
    null,
    { timeout: 30_000, polling: 250 },
  ).catch(() => {
    // Some prompts (like /reset) never enter the streaming state — that's fine.
  });

  // Then wait for it to stop, with a stability window.
  const start = Date.now();
  let lastBusy = Date.now();
  while (Date.now() - start < maxMs) {
    const busy = await page
      .evaluate(() => {
        const btns = Array.from(document.querySelectorAll('button'));
        return btns.some(
          (b) => /^stop$/i.test(b.textContent?.trim() ?? '') && !(b as HTMLButtonElement).disabled,
        );
      })
      .catch(() => false);
    if (busy) {
      lastBusy = Date.now();
    } else if (Date.now() - lastBusy > 4000) {
      // 4 seconds idle → call it done.
      return;
    }
    await page.waitForTimeout(500);
  }
  console.warn('[capture-set] AI did not idle within', maxMs / 60_000, 'minutes — snapping anyway');
}

async function scrollChatToBottom(page: Page): Promise<void> {
  await page.evaluate(() => {
    // Find the scrollable chat container (the messages list) and scroll it
    // to the bottom so the final summary lands in the screenshot.
    const candidates = Array.from(document.querySelectorAll('div, section')) as HTMLElement[];
    let best: HTMLElement | null = null;
    for (const el of candidates) {
      const cs = getComputedStyle(el);
      if ((cs.overflowY === 'auto' || cs.overflowY === 'scroll') && el.scrollHeight > el.clientHeight + 50) {
        // Prefer the largest scrollable area (the chat panel).
        if (!best || el.scrollHeight > best.scrollHeight) best = el;
      }
    }
    if (best) best.scrollTop = best.scrollHeight;
  });
  await page.waitForTimeout(500);
}

async function snap(page: Page, name: string): Promise<void> {
  fs.mkdirSync(OUT_DIR, { recursive: true });
  const file = path.join(OUT_DIR, name);
  await page.screenshot({ path: file, fullPage: false });
  console.log(`[capture-set] saved ${file}`);
}

function existing(name: string): boolean {
  return process.env.FORCE !== '1' && fs.existsSync(path.join(OUT_DIR, name));
}

async function runScene(name: string, n: number, fn: () => Promise<void>): Promise<void> {
  if (existing(name)) {
    console.log(`[capture-set] scene ${n}: ${name} already exists, skipping (FORCE=1 to redo)`);
    return;
  }
  try {
    await fn();
  } catch (e) {
    console.error(`[capture-set] scene ${n} FAILED:`, (e as Error).message);
  }
}

async function main(): Promise<void> {
  // Resolve the SDRplay device before any scene runs (scenes capture
  // the module-level SDRPLAY_ARGS binding by reference).
  await resolveSdrplayArgs();

  const browser = await chromium.launch({
    headless: true,
    args: ['--autoplay-policy=no-user-gesture-required', '--use-gl=swiftshader'],
  });
  try {
    const ctx = await browser.newContext({ viewport: VIEWPORT, ignoreHTTPSErrors: true });
    const page = await ctx.newPage();
    page.on('console', (m) => {
      if (m.type() === 'error') console.error(`[browser] ${m.text()}`);
    });

    // ---------- Scene 1: FM + Settings (NR + Input expanded) ----------
    await runScene('fm-settings.png', 1, async () => {
      console.log('[capture-set] scene 1: FM @ 100 MHz on SDRplay, Settings tab');
      await ensurePipeline('wbfm', {
        type: 'SoapySource',
        params: {
          args: SDRPLAY_ARGS,
          antenna: 'Antenna A',
          sample_rate_hz: 2_000_000,
          bandwidth_hz: 1_536_000,
          center_freq_hz: 100_000_000,
          gain_db: 45,
          agc: false,
          settings: {
            // FM broadcast notch must be OFF when receiving inside the FM band.
            rfnotch_ctrl: 'false',
            dabnotch_ctrl: 'true',
          },
        },
      });

      await page.goto(UI_URL, { waitUntil: 'domcontentloaded' });
      await page.waitForTimeout(2000); // let the UI mount + WS connect
      await clickTab(page, 'settings');
      await expandDetails(page, 'Noise Reduction');
      await expandDetails(page, 'Input');
      await waitForWaterfallFilled(page, 20_000);
      await snap(page, 'fm-settings.png');
    });

    // The two AI scenes share a navigation: only goto if we haven't been
    // there yet (skipped scene 1 because already saved).
    if (page.url() === 'about:blank') {
      await page.goto(UI_URL, { waitUntil: 'domcontentloaded' });
      await page.waitForTimeout(2000);
    }

    // ---------- Scene 2: AI scans AM/SW ----------
    await runScene('ai-amsw-scan.png', 2, async () => {
      console.log('[capture-set] scene 2: AI scan AM/SW (long-running)');
      await clickTab(page, 'ai');
      await sendAiPrompt(page, '/reset');
      await page.waitForTimeout(1500);
      await sendAiPrompt(page, "Let's scan the AM/SW bands, summarize what you found.");
      await waitForAiIdle(page);
      await scrollChatToBottom(page);
      await snap(page, 'ai-amsw-scan.png');
    });

    // ---------- Scene 3: AI decodes pager data ----------
    await runScene('ai-pager-decode.png', 3, async () => {
      console.log('[capture-set] scene 3: AI decode pager data (long-running)');
      await clickTab(page, 'ai');
      await sendAiPrompt(page, '/reset');
      await page.waitForTimeout(1500);
      await sendAiPrompt(page, "Let's decode some pager data, summarize what you find.");
      await waitForAiIdle(page);
      await scrollChatToBottom(page);
      await snap(page, 'ai-pager-decode.png');
    });

    // ---------- Scene 4: HF busy bands (parallel to scene 1, different band overlay) ----------
    // 18 MHz center @ 10 MS/s gives 13–23 MHz visible — 20m (14.0–14.35),
    // 17m (18.068–18.168), and 15m (21.0–21.45) ham bands plus the WWV
    // time-signal slot at 15 MHz all fall inside the band overlay.
    // SDRplay LNA index 0 ≈ no attenuation, the right choice for HF where
    // signals are weaker than the default rfgain_sel=4 budget assumes.
    await runScene('hf-busy-bands.png', 4, async () => {
      console.log('[capture-set] scene 4: HF @ 18 MHz on SDRplay, busy band overlay');
      await ensurePipeline('wbam', {
        type: 'SoapySource',
        params: {
          args: SDRPLAY_ARGS,
          antenna: 'Antenna A',
          sample_rate_hz: 10_000_000,
          bandwidth_hz: 8_000_000,
          center_freq_hz: 18_000_000,
          gain_db: 55,
          agc: false,
          settings: {
            rfgain_sel: '0',
            rfnotch_ctrl: 'true',
            dabnotch_ctrl: 'true',
          },
        },
      });
      // Reload to flush any stale UI state from prior scenes (the AI
      // scenes leave the chat panel mounted with its own websocket
      // traffic, and the Settings panel's block list can lag a few
      // seconds behind a REST-driven preset swap).
      await page.goto(UI_URL, { waitUntil: 'domcontentloaded' });
      await page.waitForTimeout(3000);
      await clickTab(page, 'settings');
      await expandDetails(page, 'Noise Reduction');
      await expandDetails(page, 'Input');
      await waitForWaterfallFilled(page, 20_000);
      await snap(page, 'hf-busy-bands.png');
    });

    // ---------- Scene 5: AI scans for FT8 stations ----------
    await runScene('ai-ft8-scan.png', 5, async () => {
      console.log('[capture-set] scene 5: AI scan FT8 stations (long-running)');
      await clickTab(page, 'ai');
      await sendAiPrompt(page, '/reset');
      await page.waitForTimeout(1500);
      await sendAiPrompt(page, "Let's scan for FT8 stations, summarize what you find.");
      await waitForAiIdle(page);
      await scrollChatToBottom(page);
      await snap(page, 'ai-ft8-scan.png');
    });

    console.log('[capture-set] all scenes processed');
  } finally {
    await browser.close();
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
