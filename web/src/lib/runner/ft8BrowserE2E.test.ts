// Browser-path FT8 decode e2e.
//
// FT8 is a 15 s UTC-slot decoder: `Ft8Demod` feeds the vendored
// ft8_lib monitor during a slot's active window and decodes on slot
// rollover, keyed off wall-clock time. On wasm32 that clock is
// `web_time::SystemTime` (JS `Date`) — this test fakes it so a single
// reference slot decodes deterministically through a *browser*
// `RuntimeHandle`, proving `Ft8Demod` is genuinely browser-runnable
// (it was "fake Either": Placement::Either but absent from the wasm
// runtime feature set until `ferrite-blocks/ft8` was added).
//
// Fixture + loose callsign assertion mirror the node-side
// `blocks/tests/ft8_e2e.rs`; decode correctness is arch-independent
// (same Rust/C compiled to wasm32) so this gate targets the wasm
// wiring + the web_time slot clock, not the DSP.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { afterEach, beforeAll, describe, expect, it } from 'vitest';

import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';

const RUNTIME_WASM = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const FT8_WAV = resolve(__dirname, '../../../../samples/sigidwiki/FT8_websdr_test.wav');

const FT8_RATE = 12_000;
const FT8_SLOT_MS = 15_000;
const FT8_ACTIVE_MS = 12_640;

/** Minimal RIFF/WAVE s16-mono reader → normalized f32 (the fixture is
 *  12 kHz PCM s16 mono — same as ft8_e2e.rs's hand-rolled reader). */
function readS16MonoWav(path: string): Float32Array {
  const buf = readFileSync(path);
  let pos = 12; // skip "RIFF"<size>"WAVE"
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
  name: 'ft8-browser-e2e',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRxF32',
      placement: 'browser',
      params: { stream_id: 1, buffer_samples: 524_288, sample_rate_hz: FT8_RATE },
    },
    ft8: { type: 'Ft8Demod', placement: 'browser', params: { sample_rate_hz: FT8_RATE } },
    sink: { type: 'EventsSink', placement: 'browser', params: { capacity: 4096 } },
  },
  wires: [
    ['rx.out', 'ft8.in'],
    ['ft8.events', 'sink.in'],
  ],
};

describe('browser-path FT8 decode', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(RUNTIME_WASM) });
  });
  const realDateNow = Date.now;
  afterEach(() => {
    globalThis.Date.now = realDateNow;
  });

  it('decodes a reference FT8 slot in the browser runtime', () => {
    // `web_time::SystemTime` on wasm32 reads JS `Date.now()`, resolved
    // by the wasm-bindgen glue per call — so monkeypatching it (rather
    // than vitest fake timers, which the wasm glue captured before)
    // deterministically controls `Ft8Demod`'s slot clock. Advance the
    // clock 1 s per fed second of audio (mirrors real-time slot fill),
    // then step into the dead zone (same slot) to trigger the decode.
    const slotStart = 1_700_000_000_000 - (1_700_000_000_000 % FT8_SLOT_MS);
    let fakeNow = slotStart + 100;
    globalThis.Date.now = () => fakeNow;

    const rt = new RuntimeHandle(JSON.stringify(DOC), 'browser');
    rt.init();

    const audio = readS16MonoWav(FT8_WAV);
    const CHUNK = FT8_RATE; // 1 s per chunk
    for (let off = 0; off < audio.length; off += CHUNK) {
      rt.pushF32('rx', audio.subarray(off, off + CHUNK));
      fakeNow = slotStart + 100 + Math.floor((off / FT8_RATE) * 1000);
      for (let t = 0; t < 20; t++) rt.tick(); // drain chunk → monitor
    }
    // Step into the dead zone of the *same* slot (active_ms ≤ t < slot_ms)
    // → Ft8Demod decodes the accumulated waterfall.
    fakeNow = slotStart + FT8_ACTIVE_MS + 560;
    for (let t = 0; t < 40; t++) rt.tick();

    const events = (rt.drainEvents('sink') as string[]).map(
      (e) => JSON.parse(e) as { msg?: string; text?: string },
    );
    rt.free();

    expect(events.length).toBeGreaterThan(0);
    // Loose callsign-shaped token (same sanity check as ft8_e2e.rs):
    // resilient to which messages the upstream WAV captured.
    const callsignLike = events.some((e) =>
      (e.msg ?? e.text ?? '')
        .split(/\s+/)
        .some(
          (tok) =>
            tok.length >= 3 && tok.length <= 11 && /\d/.test(tok) && /^[A-Z0-9/]+$/.test(tok),
        ),
    );
    expect(callsignLike).toBe(true);
  });
});
