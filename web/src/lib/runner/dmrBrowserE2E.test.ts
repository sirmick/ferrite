// Browser-path DMR decode e2e: metadata *and* voice.
//
// `DmrDecode` is pure Rust (Placement::Either), so the same decoder the
// node runs also runs here, in the wasm32 runtime. This feeds the real
// off-air fixture from `blocks/tests/dmr_e2e.rs` (40 s of the 441.850
// MHz repeater as 48 kHz FM-discriminator audio, int8) through a
// *browser* `RuntimeHandle` and expects the calls dsd-fme reports on the
// same audio: slot 2 TG 31068 from 3107998, slot 1 TG 93 from 3228520,
// talker alias KR1JOE, colour code 1.
//
// Voice comes from the separate, lazily loaded vocoder module
// (`mbe.wasm`, mbelib): before it is loaded the decoder must still
// report every call but emit silence; once `initMbeBridge()` has run the
// slot 2 call must come out as ~40 s of speech.
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';

import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';
import { initMbeBridge, mbeBridgeReady } from '../wasm/vocoder/mbeBridge.js';

const RUNTIME_WASM = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const MBE_WASM = resolve(__dirname, '../wasm/vocoder/mbe.wasm');
const FIXTURE = resolve(__dirname, '../../../../blocks/tests/fixtures/dmr_441850_disc48k.s8');
const RATE = 48_000;

const DOC = {
  name: 'dmr-browser-e2e',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRxF32',
      placement: 'browser',
      params: { stream_id: 1, buffer_samples: 262_144, sample_rate_hz: RATE },
    },
    dmr: { type: 'DmrDecode', placement: 'browser', params: { sample_rate_hz: RATE } },
    resamp: {
      type: 'RealF32Resamp',
      placement: 'browser',
      params: { output_rate_hz: 48_000, stopband_db: 60 },
    },
    sink: { type: 'EventsSink', placement: 'browser', params: { capacity: 4096 } },
    audio: { type: 'AudioSink', placement: 'browser', params: { buffer_samples: 65_536 } },
  },
  wires: [
    ['rx.out', 'dmr.in'],
    ['dmr.events', 'sink.in'],
    ['dmr.out', 'resamp.in'],
    ['resamp.out', 'audio.in'],
  ],
};

interface DsdRec {
  event: string;
  slot?: number;
  tgt?: string;
  src?: string;
  cc?: number;
  text: string;
}

function fixture(): Float32Array {
  const raw = readFileSync(FIXTURE);
  const audio = new Float32Array(raw.length);
  for (let i = 0; i < raw.length; i++) audio[i] = ((raw[i] << 24) >> 24) / 127;
  return audio;
}

/** Run the fixture through a fresh browser runtime; return events and
 *  the voice the `out` port produced. */
function run(): { events: DsdRec[]; voice: Float32Array } {
  const audio = fixture();
  const rt = new RuntimeHandle(JSON.stringify(DOC), 'browser');
  rt.init();
  const CHUNK = 8_192;
  const events: DsdRec[] = [];
  const voice: number[] = [];
  const scratch = new Float32Array(65_536);
  const drain = () => {
    for (const e of rt.drainEvents('sink') as string[]) events.push(JSON.parse(e) as DsdRec);
    for (;;) {
      const n = rt.drainAudio('audio', scratch);
      for (let i = 0; i < n; i++) voice.push(scratch[i]);
      if (n < scratch.length) break;
    }
  };
  for (let off = 0; off < audio.length; off += CHUNK) {
    rt.pushF32('rx', audio.subarray(off, off + CHUNK));
    for (let t = 0; t < 8; t++) rt.tick();
    drain();
  }
  rt.free();
  return { events, voice: Float32Array.from(voice) };
}

const rms = (v: Float32Array) =>
  Math.sqrt(v.reduce((a, x) => a + x * x, 0) / Math.max(1, v.length));

describe('browser-path DMR decode', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(RUNTIME_WASM) });
  });

  it('decodes the calls in the wasm runtime, silent until the vocoder loads', () => {
    expect(mbeBridgeReady()).toBe(false);
    const { events, voice } = run();
    const call = (slot: number, tgt: string, src: string) =>
      events.some((e) => e.event === 'call' && e.slot === slot && e.tgt === tgt && e.src === src);
    expect(call(2, '31068', '3107998')).toBe(true);
    expect(call(1, '93', '3228520')).toBe(true);
    expect(events.filter((e) => e.event === 'call').every((e) => e.cc === 1)).toBe(true);
    expect(events.some((e) => e.event === 'alias' && e.text.startsWith('KR1JOE'))).toBe(true);
    // No vocoder module yet → the voice port is clocked but silent.
    expect(rms(voice)).toBe(0);
  });

  it.skipIf(!existsSync(MBE_WASM))('synthesises speech once the lazy vocoder loads', async () => {
    expect(await initMbeBridge({ wasmBinary: readFileSync(MBE_WASM) })).toBe(true);
    const { voice } = run();
    // `out` is 8 kHz voice, resampled to 48 kHz. The resampler only
    // learns 8 kHz from DmrDecode's *second* output port: the runtime
    // used to propagate one rate per block (port 0 = events → the 48 kHz
    // input rate), so it ran at ratio 1.0 and 40 s of input came out
    // as ~6.7 s of 6×-sped-up audio. Duration pins the per-port rate.
    expect(voice.length / 48_000).toBeGreaterThan(36);
    let speech = 0;
    for (let i = 0; i + 960 <= voice.length; i += 960) {
      if (rms(voice.subarray(i, i + 960)) > 0.003) speech += 960;
    }
    expect(speech / 48_000).toBeGreaterThan(30);
    expect(rms(voice)).toBeGreaterThan(0.01);
  });
});
