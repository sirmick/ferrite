// Integration test for the wasm-pack runtime shim.
//
// Instantiates the generated module via `initSync` with the .wasm bytes
// read from disk — this bypasses the fetch-based default init, which
// jsdom doesn't serve, and gives us a real Rust-call round-trip under
// vitest. If `version()` returns the expected crate version string,
// the wasm-pack → web pipeline is wired correctly.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';

import wbfmJson from '../../../../flowgraphs/wbfm.json';
import {
  initSync,
  parseAndValidateDoc,
  RuntimeHandle,
  splitDocForEnvironment,
  version,
} from '../wasm/runtime/runtime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const EXPECTED_VERSION = '0.9.0';

describe('rust runtime wasm shim', () => {
  beforeAll(() => {
    const bytes = readFileSync(WASM_PATH);
    initSync({ module: bytes });
  });

  it('returns the crate version string', () => {
    expect(version()).toBe(EXPECTED_VERSION);
  });

  it('parses and validates the shipped wbfm preset', () => {
    const name = parseAndValidateDoc(JSON.stringify(wbfmJson));
    expect(name).toBe('wbfm');
  });

  it('throws on malformed flowgraph JSON', () => {
    expect(() => parseAndValidateDoc('{"not":"a flowgraph"}')).toThrow();
  });

  it('throws with a structured error on a wire endpoint referring to an unknown block', () => {
    const bad = {
      name: 'bad',
      environments: ['browser'],
      blocks: { a: { type: 'Src' } },
      wires: [['a.out', 'ghost.in']],
    };
    expect(() => parseAndValidateDoc(JSON.stringify(bad))).toThrow(/ghost/);
  });

  // The full wbfm preset uses a `Source` placeholder that the server
  // composes with a concrete hardware source before splitting — the
  // browser just runs its half. These tests use a browser-only fixture
  // (every block type compiles into the wasm build) to exercise the
  // wasm-bindgen plumbing.
  it('splits a browser-only doc into the same doc (trivial identity case)', () => {
    const doc = {
      name: 'sine-audio',
      environments: ['browser'],
      blocks: {
        src: { type: 'SineSource', placement: 'browser' },
        sink: { type: 'AudioSink', placement: 'browser' },
      },
      wires: [['src.out', 'sink.in']],
    };
    const json = splitDocForEnvironment(JSON.stringify(doc), 'browser');
    const out = JSON.parse(json) as {
      blocks: Record<string, { type: string; placement?: string }>;
    };
    expect(Object.keys(out.blocks).sort()).toEqual(['sink', 'src']);
  });

  it('throws on an unknown environment string', () => {
    const doc = { name: 'x', environments: ['browser'], blocks: {}, wires: [] };
    expect(() => splitDocForEnvironment(JSON.stringify(doc), 'mainframe')).toThrow(
      /unknown environment/,
    );
  });

  // Browser-only fixture — SineSource (IqF32) → FmDemod (→ RealF32) →
  // AudioSink. AudioSink is a no-op stub today (real SAB glue lands in
  // a later M4 slice), but the graph type-checks and exercises the full
  // Created → Initialized → Running → Stopped lifecycle under the Rust
  // runtime without any JS-side block bridge.
  const sineToSinkDoc = JSON.stringify({
    name: 'sine-audio',
    environments: ['browser'],
    blocks: {
      src: { type: 'SineSource', placement: 'browser', params: { rate_hz: 240000 } },
      demod: {
        type: 'FmDemod',
        placement: 'browser',
        params: { sample_rate_hz: 240000, max_deviation_hz: 75000 },
      },
      sink: { type: 'AudioSink', placement: 'browser' },
    },
    wires: [
      ['src.out', 'demod.in'],
      ['demod.out', 'sink.in'],
    ],
  });

  describe('RuntimeHandle', () => {
    it('walks Created → Initialized → Running → Stopped', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        expect(rt.state).toBe('Created');
        rt.init();
        expect(rt.state).toBe('Initialized');
        rt.start();
        expect(rt.state).toBe('Running');
        rt.tick();
        rt.tick();
        rt.stop();
        expect(rt.state).toBe('Stopped');
      } finally {
        rt.free();
      }
    });

    it('honours a custom frames_hint', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser', 256);
      try {
        expect(rt.framesHint).toBe(256);
      } finally {
        rt.free();
      }
    });

    it('defaults frames_hint when omitted', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        expect(rt.framesHint).toBe(1024);
      } finally {
        rt.free();
      }
    });

    it('rejects tick before init', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        expect(() => rt.tick()).toThrow(/Created/);
      } finally {
        rt.free();
      }
    });

    it('throws on malformed JSON at construction', () => {
      expect(() => new RuntimeHandle('{"not":"a flowgraph"}', 'browser')).toThrow();
    });

    it('throws on unknown environment at construction', () => {
      expect(() => new RuntimeHandle(sineToSinkDoc, 'mainframe')).toThrow(/unknown environment/);
    });

    it('drains demodulated audio out of the AudioSink block', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        rt.init();
        rt.tick();
        const out = new Float32Array(1024);
        const got = rt.drainAudio('sink', out);
        // FmDemod decimates 240k → 48k by factor 5, so one 1024-frame
        // tick leaves about 204 samples in the sink. Exact count
        // depends on FmDemod's internal rounding — pin the lower bound.
        expect(got).toBeGreaterThan(0);
        expect(got).toBeLessThanOrEqual(1024);
        // A pure complex exponential should demod to a non-zero
        // instantaneous-frequency estimate.
        const anyNonZero = out.slice(0, got).some((x) => x !== 0);
        expect(anyNonZero).toBe(true);
      } finally {
        rt.free();
      }
    });

    it('reports zero dropped samples on a fresh sink', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        rt.init();
        expect(rt.audioDroppedSamples('sink')).toBe(0n);
      } finally {
        rt.free();
      }
    });

    it('rejects drainAudio for a non-AudioSink block', () => {
      const rt = new RuntimeHandle(sineToSinkDoc, 'browser');
      try {
        rt.init();
        const out = new Float32Array(16);
        expect(() => rt.drainAudio('src', out)).toThrow(/AudioSink/);
        expect(() => rt.drainAudio('ghost', out)).toThrow(/ghost/);
      } finally {
        rt.free();
      }
    });

    // Browser-only fixture for the incoming-IQ side of the bridge —
    // WsBridgeRx's ring is filled from JS via pushIq, then drained by
    // process onto the IqF32 output. Pair it with FmDemod + AudioSink
    // so the end-to-end push → tick → drain pipeline is exercised.
    const wsIqToSinkDoc = JSON.stringify({
      name: 'ws-iq-audio',
      environments: ['browser'],
      blocks: {
        src: {
          type: 'WsBridgeRx',
          placement: 'browser',
          params: { stream_id: 42, buffer_samples: 4096 },
        },
        demod: {
          type: 'FmDemod',
          placement: 'browser',
          params: { sample_rate_hz: 240000, max_deviation_hz: 75000 },
        },
        sink: { type: 'AudioSink', placement: 'browser' },
      },
      wires: [
        ['src.out', 'demod.in'],
        ['demod.out', 'sink.in'],
      ],
    });

    it('buffers IQ samples pushed in from JS', () => {
      const rt = new RuntimeHandle(wsIqToSinkDoc, 'browser');
      try {
        rt.init();
        // 6 floats = 3 complex samples.
        rt.pushIq('src', new Float32Array([1, 2, 3, 4, 5, 6]));
        expect(rt.iqBufferedSamples('src')).toBe(3);
        expect(rt.iqDroppedSamples('src')).toBe(0n);
      } finally {
        rt.free();
      }
    });

    it('tick drains buffered IQ samples through the graph', () => {
      const rt = new RuntimeHandle(wsIqToSinkDoc, 'browser');
      try {
        rt.init();
        // Fill with a plausible IQ signal (unit magnitude). 1024 pairs
        // → 2048 floats; small enough that a single 1024-frame tick
        // drains it completely.
        const pairs = 1024;
        const iq = new Float32Array(pairs * 2);
        for (let i = 0; i < pairs; i++) {
          iq[i * 2] = Math.cos(i * 0.01);
          iq[i * 2 + 1] = Math.sin(i * 0.01);
        }
        rt.pushIq('src', iq);
        expect(rt.iqBufferedSamples('src')).toBe(pairs);
        rt.tick();
        expect(rt.iqBufferedSamples('src')).toBeLessThan(pairs);
      } finally {
        rt.free();
      }
    });

    it('tallies dropped IQ samples when the ring overflows', () => {
      const rt = new RuntimeHandle(wsIqToSinkDoc, 'browser');
      try {
        rt.init();
        // buffer_samples=4096 pairs → 8192 floats; push twice that in
        // one shot. Half the samples should drop on the floor.
        const overflow = new Float32Array(8192 * 2);
        rt.pushIq('src', overflow);
        expect(rt.iqBufferedSamples('src')).toBe(4096);
        expect(rt.iqDroppedSamples('src')).toBe(4096n);
      } finally {
        rt.free();
      }
    });

    it('rejects pushIq for a non-WsBridgeRx block', () => {
      const rt = new RuntimeHandle(wsIqToSinkDoc, 'browser');
      try {
        rt.init();
        const samples = new Float32Array([0, 0]);
        expect(() => rt.pushIq('demod', samples)).toThrow(/WsBridgeRx/);
        expect(() => rt.pushIq('ghost', samples)).toThrow(/ghost/);
      } finally {
        rt.free();
      }
    });
  });
});
