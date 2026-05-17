// Browser-path VoiceTranscribe tap/passthrough e2e.
//
// Proves the contract the transcription feature rests on, in a real
// browser `RuntimeHandle`, with no whisper/model needed (deterministic,
// runs every push): a known ramp is pushed through
// `WsBridgeRxF32 → VoiceTranscribe → AudioSink` and we assert, per
// tri-state mode, exactly what reaches the audio ring (`drainAudio`)
// vs. the transcription tap ring (`drainTranscribe`), plus that the
// negotiated input rate is reported (`transcribeInputRate`).
//
// JS/wasm complement to the Rust `voice_transcribe` unit tests — same
// guarantees, exercised across the wasm-bindgen boundary the runner
// actually uses. Mirrors fldigiBrowserE2E's harness.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { beforeAll, describe, expect, it } from 'vitest';

import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const RATE = 12_000;
const N = 8192;

function ramp(n: number): Float32Array {
  const x = new Float32Array(n);
  for (let i = 0; i < n; i++) x[i] = (i % 257) * 0.001 - 0.128;
  return x;
}

function docFor(mode: string) {
  return {
    name: 'vt-e2e',
    environments: ['browser'],
    blocks: {
      rx: {
        type: 'WsBridgeRxF32',
        placement: 'browser',
        params: { stream_id: 1, buffer_samples: 65536, sample_rate_hz: RATE },
      },
      vt: {
        type: 'VoiceTranscribe',
        placement: 'browser',
        params: { mode, buffer_samples: 16384 },
      },
      sink: { type: 'AudioSink', placement: 'browser', params: { buffer_samples: 16384 } },
    },
    wires: [
      ['rx.out', 'vt.in'],
      ['vt.out', 'sink.in'],
    ],
  };
}

/** Push the ramp through, then drain a ring dry (ticking between
 *  reads so the scheduler keeps flushing). Returns the concatenation. */
function runMode(mode: string): { audio: Float32Array; tap: Float32Array; rate: number } {
  const rt = new RuntimeHandle(JSON.stringify(docFor(mode)), 'browser');
  rt.init();
  const input = ramp(N);
  const CHUNK = 2048;
  for (let off = 0; off < input.length; off += CHUNK) {
    rt.pushF32('rx', input.subarray(off, off + CHUNK));
    for (let t = 0; t < 16; t++) rt.tick();
  }
  for (let t = 0; t < 64; t++) rt.tick();

  const drainRing = (kind: 'audio' | 'tap'): Float32Array => {
    const acc: number[] = [];
    const scratch = new Float32Array(8192);
    for (let pass = 0; pass < 80; pass++) {
      const n =
        kind === 'audio' ? rt.drainAudio('sink', scratch) : rt.drainTranscribe('vt', scratch);
      for (let i = 0; i < n; i++) acc.push(scratch[i]);
      if (n === 0) {
        for (let t = 0; t < 4; t++) rt.tick();
        const n2 =
          kind === 'audio' ? rt.drainAudio('sink', scratch) : rt.drainTranscribe('vt', scratch);
        for (let i = 0; i < n2; i++) acc.push(scratch[i]);
        if (n2 === 0) break;
      }
    }
    return Float32Array.from(acc);
  };

  const audio = drainRing('audio');
  const tap = drainRing('tap');
  const rate = rt.transcribeInputRate('vt');
  rt.free();
  return { audio, tap, rate };
}

function maxAbs(a: Float32Array): number {
  let m = 0;
  for (const v of a) m = Math.max(m, Math.abs(v));
  return m;
}
function firstMismatch(a: Float32Array, b: Float32Array, n: number): number {
  for (let i = 0; i < n; i++) if (Math.abs(a[i] - b[i]) > 1e-6) return i;
  return -1;
}

describe('browser-path VoiceTranscribe tap/passthrough', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(WASM_PATH) });
  });

  it('off: audio passes through verbatim, nothing tapped (zero cost)', () => {
    const { audio, tap, rate } = runMode('off');
    const input = ramp(N);
    expect(audio.length).toBeGreaterThan(0);
    expect(firstMismatch(audio, input, audio.length)).toBe(-1);
    expect(tap.length).toBe(0);
    expect(rate).toBe(RATE);
  });

  it('on: audio passes through AND the clean signal is tapped', () => {
    const { audio, tap } = runMode('on');
    const input = ramp(N);
    expect(audio.length).toBeGreaterThan(0);
    expect(firstMismatch(audio, input, audio.length)).toBe(-1);
    expect(tap.length).toBeGreaterThan(0);
    expect(firstMismatch(tap, input, tap.length)).toBe(-1);
  });

  it('muted: output silenced but the tap still sees the clean signal', () => {
    const { audio, tap } = runMode('muted');
    const input = ramp(N);
    expect(audio.length).toBeGreaterThan(0);
    expect(maxAbs(audio)).toBeLessThan(1e-6); // silence
    expect(tap.length).toBeGreaterThan(0);
    expect(firstMismatch(tap, input, tap.length)).toBe(-1); // un-muted tap
  });
});
