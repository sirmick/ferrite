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
  splitDocForEnvironment,
  version,
} from '../wasm/runtime/runtime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const EXPECTED_VERSION = '0.0.1';

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

  // The full wbfm preset starts with SoapySource, which is feature-gated
  // on native only and therefore not in the browser's inventory — the
  // real split happens on the server and the browser just runs its half.
  // These tests use a browser-only fixture (every block type compiles
  // into the wasm build) to exercise the wasm-bindgen plumbing.
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
});
