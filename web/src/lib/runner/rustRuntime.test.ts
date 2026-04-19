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
import { initSync, parseAndValidateDoc, version } from '../wasm/runtime/runtime.js';

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
});
