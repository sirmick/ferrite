// Browser-path fldigi decode e2e.
//
// Proves the in-browser fldigi chain end to end: a synthetic Baudot
// RTTY signal pushed into a *browser* `RuntimeHandle` whose `RttyDemod`
// block's fldigi C ABI is satisfied — across two wasm modules' linear
// memories — by the sibling Emscripten module that `initFldigiBridge()`
// instantiates. The decoded text egresses the new `events` port into an
// `EventsSink` and is read back via `drainEvents`.
//
// This is the JS/wasm complement to the Rust-side `blocks/tests/
// fldigi_e2e.rs` (which decodes the same synthetic signal node-side
// through the tracing path). The modulator is ported verbatim from
// that test — no binary fixture (RTTY modulation is trivial and a
// committed waveform would just obscure the parameters).
//
// Runs under vitest's node env, where the Emscripten module takes its
// node code path (proven to decode identically to libstdc++). The
// real-browser-Worker path is validated manually; this gate exercises
// the identical bridge + decode logic on every push.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { beforeAll, describe, expect, it } from 'vitest';

import { initFldigiBridge } from '../wasm/fldigi/fldigiBridge.js';
import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const FLDIGI_WASM_PATH = resolve(__dirname, '../wasm/fldigi/fldigi.wasm');

// ── Synthetic Baudot RTTY modulator (ported from fldigi_e2e.rs) ──────
const FS = 8_000;
const CENTER = 1_500;
const SHIFT = 170;
const BAUD = 45;
const TAU = 2 * Math.PI;

// fldigi vendor/rtty.cxx letters[32]: index = 5-bit Baudot code.
const LETTERS = [
  0, 69, 10, 65, 32, 83, 73, 85, 13, 68, 82, 74, 78, 70, 67, 75, 84, 90, 76, 87, 72, 89, 80, 81, 79,
  66, 71, 0, 77, 88, 86, 0,
]; // 'E','\n','A',' ','S','I','U','\r','D','R','J','N','F','C','K','T','Z','L','W','H','Y','P','Q','O','B','G','M','X','V'
const LTRS = 0b11111;
const BAUDOT_SPACE = 0b00100;

function codeFor(ch: number): number {
  const i = LETTERS.indexOf(ch);
  return i >= 0 ? i : LTRS;
}

interface Phase {
  v: number;
}

function emit(out: number[], phase: Phase, bit: boolean, bits: number): void {
  const f = bit ? CENTER + SHIFT / 2 : CENTER - SHIFT / 2;
  const n = Math.round((FS / BAUD) * bits);
  for (let k = 0; k < n; k++) {
    phase.v += (TAU * f) / FS;
    if (phase.v > TAU) phase.v -= TAU;
    out.push(Math.sin(phase.v) * 0.5);
  }
}

function pushChar(out: number[], phase: Phase, code: number): void {
  emit(out, phase, false, 1.0); // start (space)
  for (let b = 0; b < 5; b++) emit(out, phase, ((code >> b) & 1) === 1, 1.0); // 5 data, LSB-first
  emit(out, phase, true, 1.5); // 1.5 stop (mark)
}

function modulate(text: string): Float32Array {
  const out: number[] = [];
  const phase: Phase = { v: 0 };
  for (let i = 0; i < FS * 0.5; i++) {
    phase.v += (TAU * (CENTER + SHIFT / 2)) / FS;
    out.push(Math.sin(phase.v) * 0.5);
  }
  for (let i = 0; i < 8; i++) pushChar(out, phase, LTRS); // preamble
  for (const ch of [...text].map((c) => c.charCodeAt(0))) {
    pushChar(out, phase, ch === 32 ? BAUDOT_SPACE : codeFor(ch));
  }
  for (let i = 0; i < 4; i++) pushChar(out, phase, LTRS); // trailer
  return Float32Array.from(out);
}

// ── Flowgraph: pushed 8 kHz audio → RttyDemod → EventsSink ───────────
const MESSAGE = 'CQ CQ DE FERRITE FERRITE K';
const RX_BUFFER = 262_144;

const DOC = {
  name: 'fldigi-browser-e2e',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRxF32',
      placement: 'browser',
      params: { stream_id: 1, buffer_samples: RX_BUFFER, sample_rate_hz: FS },
    },
    rtty: { type: 'RttyDemod', placement: 'browser', params: {} },
    sink: { type: 'EventsSink', placement: 'browser', params: { capacity: 4096 } },
  },
  wires: [
    ['rx.out', 'rtty.in'],
    ['rtty.events', 'sink.in'],
  ],
};

describe('browser-path fldigi RTTY decode', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(WASM_PATH) });
  });

  it('instantiates the Emscripten bridge and decodes RTTY in-browser', async () => {
    // The whole point: the fldigi C ABI is satisfied by the Emscripten
    // module, not linked into the Rust wasm. If emsdk never ran the
    // artifact is absent and the bridge is inert — fail loudly so CI
    // (which now builds it) catches a broken Emscripten build, the
    // exact regression that slipped through before.
    // vitest serves modules over an http:// URL scheme, so the
    // Emscripten glue's `new URL('fldigi.wasm', import.meta.url)` +
    // node-fs read can't locate the sidecar. Inject the bytes directly
    // (a real browser uses the fetch path against the vite-emitted
    // asset — exercised in the manual browser test).
    const wasmBinary = readFileSync(FLDIGI_WASM_PATH);
    const live = await initFldigiBridge({ wasmBinary });
    expect(live).toBe(true);

    const rt = new RuntimeHandle(JSON.stringify(DOC), 'browser');
    rt.init();

    const audio = modulate(MESSAGE);
    const CHUNK = 8_192;
    for (let off = 0; off < audio.length; off += CHUNK) {
      rt.pushF32('rx', audio.subarray(off, off + CHUNK));
      for (let t = 0; t < 16; t++) rt.tick(); // drain the chunk through
    }
    for (let t = 0; t < 64; t++) rt.tick(); // flush trailing decode

    const events = rt.drainEvents('sink') as string[];
    rt.free();

    const text = events.map((e) => (JSON.parse(e) as { t: string; text: string }).text).join('');
    expect(text).toContain(MESSAGE);
  });
});
