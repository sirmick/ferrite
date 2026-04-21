// End-to-end DSP correctness test through the real Rust runtime.
//
// Synthesises an FM-modulated 1 kHz tone at 240 kHz sample rate, pushes
// the interleaved-float IQ into a WsBridgeRx → FmDemod → AudioSink
// graph via the same `pushIq` path the browser Worker uses, lets the
// RunnerCore tick loop drain audio into a SAB-backed AudioRingWriter,
// then reads the demodulated audio back via AudioRingReader and checks
// the tone frequency by counting zero crossings.
//
// This is the pattern we want future digital-mode decoders to follow:
// fixture in, semantic assertion out. For CW that will be "audio file
// in → text string out"; for now the demodulated tone gives us a
// regression signal on the entire browser pipeline without needing a
// browser.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';

import { AudioRingReader } from '../audio/ringBuffer.js';
import type { FlowgraphDoc } from '../flowgraph.js';
import type { FrameClient, FrameHandler } from '../ws/client.js';
import { PayloadType, type ParsedFrame } from '../ws/frame.js';
import { initSync } from '../wasm/runtime/runtime.js';
import { RunnerCore, type RunnerEnv } from './runnerCore.js';
import { createRuntime, splitFlowgraphForEnv } from './rustRuntime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');

const SAMPLE_RATE_HZ = 240_000;
const MAX_DEVIATION_HZ = 75_000;
const TONE_HZ = 1_000;
// Modulation deviation — scales the demodulated tone amplitude
// (out_peak ≈ deviation / max_deviation_hz → ~0.4 here).
const TONE_DEVIATION_HZ = 30_000;
const DURATION_SEC = 0.5;

// Ring capacity in samples. 262_144 is already a power of two and holds
// ~1.1 s of 240 kHz audio — comfortably more than the 120 000 samples
// this test produces, so nothing is dropped between tick and drain.
const BUFFER_SAMPLES = 262_144;

/**
 * Synthesise a baseband FM-modulated tone as interleaved I/Q f32.
 *
 * The instantaneous phase integrates the modulating tone:
 *   φ(t) = 2π · ∫ Δf · cos(2π f_m t) dt = 2π · (Δf / f_m) · sin(2π f_m t)
 * so the resulting signal's instantaneous frequency is Δf · cos(2π f_m t),
 * and the FmDemod output (scaled by 1/max_deviation) is a cosine at f_m.
 */
function fmModulatedTone(
  toneHz: number,
  deviationHz: number,
  durationSec: number,
  sampleRateHz: number,
): Float32Array {
  const n = Math.floor(durationSec * sampleRateHz);
  const out = new Float32Array(n * 2);
  const beta = deviationHz / toneHz;
  const omega = 2 * Math.PI * toneHz;
  for (let i = 0; i < n; i++) {
    const t = i / sampleRateHz;
    const phase = beta * Math.sin(omega * t);
    out[i * 2] = Math.cos(phase);
    out[i * 2 + 1] = Math.sin(phase);
  }
  return out;
}

/** Count sign changes ignoring exact-zero runs. */
function zeroCrossings(samples: Float32Array, start = 0): number {
  let count = 0;
  let prevSign = 0;
  for (let i = start; i < samples.length; i++) {
    const v = samples[i]!;
    const sign = v > 0 ? 1 : v < 0 ? -1 : 0;
    if (sign !== 0 && prevSign !== 0 && sign !== prevSign) count++;
    if (sign !== 0) prevSign = sign;
  }
  return count;
}

const DOC: FlowgraphDoc = {
  name: 'dsp-fm-tone',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRx',
      placement: 'browser',
      params: { stream_id: 42, buffer_samples: BUFFER_SAMPLES },
    },
    demod: {
      type: 'FmDemod',
      placement: 'browser',
      params: { sample_rate_hz: SAMPLE_RATE_HZ, max_deviation_hz: MAX_DEVIATION_HZ },
    },
    sink: {
      type: 'AudioSink',
      placement: 'browser',
      params: { buffer_samples: BUFFER_SAMPLES },
    },
  },
  wires: [
    ['rx.out', 'demod.in'],
    ['demod.out', 'sink.in'],
  ],
} as unknown as FlowgraphDoc;

class FakeFrameClient {
  readonly subs = new Map<number, Set<FrameHandler>>();

  subscribe(streamId: number, handler: FrameHandler): () => void {
    let set = this.subs.get(streamId);
    if (!set) {
      set = new Set();
      this.subs.set(streamId, set);
    }
    set.add(handler);
    return () => set!.delete(handler);
  }

  close(): void {}

  deliver(streamId: number, floats: Float32Array): void {
    const frame: ParsedFrame = {
      header: {
        payloadType: PayloadType.IqF32,
        streamId,
        seq: 0,
        timestampNs: 0n,
      },
      payload: new Uint8Array(floats.buffer, floats.byteOffset, floats.byteLength),
    };
    this.subs.get(streamId)?.forEach((h) => h(frame));
  }
}

function makeEnv(): { env: RunnerEnv; clients: FakeFrameClient[] } {
  const clients: FakeFrameClient[] = [];
  const env: RunnerEnv = {
    createFrameClient: () => {
      const c = new FakeFrameClient();
      clients.push(c);
      return c as unknown as FrameClient;
    },
    splitDoc: (doc, e) => splitFlowgraphForEnv(doc, e),
    createRuntime: (doc, e) => createRuntime(doc, e),
    tickIntervalMs: 1,
  };
  return { env, clients };
}

/** Drain every available sample out of the SAB into one contiguous array. */
function readAllAudio(sab: SharedArrayBuffer): Float32Array {
  const reader = AudioRingReader.fromSab(sab);
  const chunks: Float32Array[] = [];
  const scratch = new Float32Array(BUFFER_SAMPLES);
  let total = 0;
  while (reader.availableRead() > 0) {
    const n = reader.read(scratch);
    if (n === 0) break;
    chunks.push(scratch.slice(0, n));
    total += n;
  }
  const out = new Float32Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

describe('Runner DSP (end-to-end through Rust runtime)', () => {
  beforeAll(() => {
    const bytes = readFileSync(WASM_PATH);
    initSync({ module: bytes });
  });

  it('recovers a 1 kHz tone from an FM-modulated IQ fixture', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);

    const load = await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    expect(load.ok).toBe(true);
    if (!load.ok || load.kind !== 'load') throw new Error('load failed');
    const sab = load.data.audioSabs.sink;

    const iq = fmModulatedTone(TONE_HZ, TONE_DEVIATION_HZ, DURATION_SEC, SAMPLE_RATE_HZ);
    clients[0]!.deliver(42, iq);

    await core.handle({ id: 2, kind: 'start' });
    // Give the tick loop time to pull the full fixture through the
    // demod and drain it into the SAB. 150 ms is comfortable for
    // 120 000 input samples at tickIntervalMs: 1.
    await new Promise((r) => setTimeout(r, 150));
    await core.handle({ id: 3, kind: 'stop' });

    const audio = readAllAudio(sab);

    // Skip the initial settling samples — the FmDemod carries a `prev`
    // phase that starts at zero, so the first few outputs don't
    // represent the steady-state tone yet.
    const SKIP = 200;
    expect(audio.length).toBeGreaterThan(SKIP + SAMPLE_RATE_HZ / 20);

    const zcs = zeroCrossings(audio, SKIP);
    const durationSec = (audio.length - SKIP) / SAMPLE_RATE_HZ;
    const observedHz = zcs / (2 * durationSec);

    // Zero-crossing rate should match the modulating tone within 5 %.
    expect(observedHz).toBeGreaterThan(TONE_HZ * 0.95);
    expect(observedHz).toBeLessThan(TONE_HZ * 1.05);

    // Amplitude check: demodulated tone peak ≈ deviation / max_deviation.
    const expectedPeak = TONE_DEVIATION_HZ / MAX_DEVIATION_HZ;
    let peak = 0;
    for (let i = SKIP; i < audio.length; i++) {
      const a = Math.abs(audio[i]!);
      if (a > peak) peak = a;
    }
    expect(peak).toBeGreaterThan(expectedPeak * 0.9);
    expect(peak).toBeLessThan(expectedPeak * 1.1);
  });
});
