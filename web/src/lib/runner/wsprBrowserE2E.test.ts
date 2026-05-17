// Browser-path WSPR decode e2e.
//
// WSPR is a 120 s UTC-slot decoder: `WsprDemod` carries the front-end
// (mix 1500 Hz → baseband, ÷32 decimate to 375 Hz I/Q), buffers a
// whole slot, and decodes on slot rollover — keyed off wall-clock
// time. On wasm32 that clock is `web_time::SystemTime` (JS `Date`);
// this test fakes it so the wsprsim reference slot decodes
// deterministically through a *browser* `RuntimeHandle`, proving
// `WsprDemod` is genuinely browser-runnable (it was "fake Either":
// Placement::Either but absent from the wasm runtime feature set until
// `ferrite-blocks/wspr` was added, and `SystemTime::now()` panicked on
// wasm32 before the web_time swap).
//
// Fixture + expected decode mirror `blocks/tests/wspr_e2e.rs`; the
// wsprd core (incl. its FFTW→kiss_fft swap) is arch-independent, so
// this gate targets the wasm wiring + web_time slot clock.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { afterEach, beforeAll, describe, expect, it } from 'vitest';

import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';

const RUNTIME_WASM = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const WSPR_WAV = resolve(__dirname, '../../../../samples/sigidwiki/WSPR_refsim_12k.wav');

const WSPR_RATE = 12_000;
const WSPR_SLOT_MS = 120_000;
const EXPECTED = 'K1JT FN20 20';

/** Minimal RIFF/WAVE s16-mono reader → normalized f32. */
function readS16MonoWav(path: string): Float32Array {
  const buf = readFileSync(path);
  let pos = 12;
  while (pos + 8 <= buf.length) {
    const id = buf.toString('ascii', pos, pos + 4);
    const size = buf.readUInt32LE(pos + 4);
    if (id === 'data') {
      const n = size >> 1;
      const out = new Float32Array(n);
      for (let i = 0; i < n; i++) out[i] = buf.readInt16LE(pos + 8 + i * 2) / 32768;
      return out;
    }
    pos += 8 + size + (size & 1);
  }
  throw new Error('no data chunk');
}

const DOC = {
  name: 'wspr-browser-e2e',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRxF32',
      placement: 'browser',
      params: { stream_id: 1, buffer_samples: 524_288, sample_rate_hz: WSPR_RATE },
    },
    wspr: { type: 'WsprDemod', placement: 'browser', params: { sample_rate_hz: WSPR_RATE } },
    sink: { type: 'EventsSink', placement: 'browser', params: { capacity: 4096 } },
  },
  wires: [
    ['rx.out', 'wspr.in'],
    ['wspr.events', 'sink.in'],
  ],
};

describe('browser-path WSPR decode', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(RUNTIME_WASM) });
  });
  const realDateNow = Date.now;
  afterEach(() => {
    globalThis.Date.now = realDateNow;
  });

  it('decodes the wsprsim reference slot in the browser runtime', () => {
    // web_time::SystemTime on wasm32 → JS Date.now(), resolved by the
    // wasm-bindgen glue per call. Advance the clock 1 s per fed second
    // of audio so the whole slot fills within slot N, then jump to
    // slot N+1 — WsprDemod decodes the just-completed slot on rollover.
    const slotStart = 1_700_000_000_000 - (1_700_000_000_000 % WSPR_SLOT_MS);
    let fakeNow = slotStart + 100;
    globalThis.Date.now = () => fakeNow;

    const rt = new RuntimeHandle(JSON.stringify(DOC), 'browser');
    rt.init();

    const audio = readS16MonoWav(WSPR_WAV);
    const CHUNK = WSPR_RATE; // 1 s per chunk
    for (let off = 0; off < audio.length; off += CHUNK) {
      rt.pushF32('rx', audio.subarray(off, off + CHUNK));
      // Stay strictly inside slot N (max ≈ slotStart+119 s < 120 s).
      fakeNow = slotStart + 100 + Math.floor((off / WSPR_RATE) * 1000);
      for (let t = 0; t < 20; t++) rt.tick();
    }
    // Roll into slot N+1. WsprDemod.process() early-returns on an
    // empty input port *before* its slot logic, so the rollover (and
    // its decode of the just-completed slot) only fires on a tick that
    // still has input — keep pushing a little silence across the
    // boundary so the buffered slot N actually decodes.
    fakeNow = slotStart + WSPR_SLOT_MS + 500;
    const silence = new Float32Array(WSPR_RATE);
    for (let k = 0; k < 6; k++) {
      rt.pushF32('rx', silence);
      for (let t = 0; t < 20; t++) rt.tick();
    }

    const events = (rt.drainEvents('sink') as string[]).map(
      (e) => JSON.parse(e) as { msg?: string; text?: string },
    );
    rt.free();

    expect(events.length).toBeGreaterThan(0);
    const decoded = events.map((e) => e.msg ?? e.text ?? '').join(' ');
    expect(decoded).toContain(EXPECTED);
  });
});
