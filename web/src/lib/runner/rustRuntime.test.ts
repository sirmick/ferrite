// Integration test for the wasm-pack runtime shim.
//
// Instantiates the generated module via `initSync` with the .wasm bytes
// read from disk — this bypasses the fetch-based default init, which
// jsdom doesn't serve, and gives us a real Rust-call round-trip under
// vitest. If `version()` returns the expected crate version string,
// the wasm-pack → web pipeline is wired correctly.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

import { initSync, version } from '../wasm/runtime/runtime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const EXPECTED_VERSION = '0.0.1';

describe('rust runtime wasm shim', () => {
  it('instantiates the wasm-pack module and returns the crate version', () => {
    const bytes = readFileSync(WASM_PATH);
    initSync({ module: bytes });
    expect(version()).toBe(EXPECTED_VERSION);
  });
});
