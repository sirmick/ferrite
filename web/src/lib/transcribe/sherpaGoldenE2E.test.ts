// Golden-transcript e2e — the real ship gate for in-browser STT
// (light/sherpa tier). Mirrors whisperGoldenE2E: drives the *actual*
// sherpa-onnx WASM module (built by blocks/native/sherpa/emscripten/
// build.sh) so a regression in the Emscripten build, the streaming API
// glue, or the baked model fails loudly.
//
// Runs inference in a real Node process via __fixtures__/sherpaRunner.mjs
// (not vitest's http module runner — Emscripten's pthread workers need a
// file:// URL). Fixture is whisper.cpp's canonical jfk.wav (16 kHz mono
// PCM, committed); the streaming zipformer EN-20M recognises it near-
// verbatim. The assertion is a robust substring so it survives the
// model's qu/warmup non-determinism (e.g. a clipped leading word).
//
// skipIf keeps it green-by-skip until `pnpm wasm:build:sherpa` has run
// (vendored sources + emsdk + model); once built it is a hard gate.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { describe, expect, it } from 'vitest';

const SHERPA_JS = resolve(__dirname, '../wasm/sherpa/sherpa-onnx-wasm-main-asr.js');
const SHERPA_WASM = resolve(__dirname, '../wasm/sherpa/sherpa-onnx-wasm-main-asr.wasm');
const SHERPA_DATA = resolve(__dirname, '../wasm/sherpa/sherpa-onnx-wasm-main-asr.data');
const SHERPA_API = resolve(__dirname, '../wasm/sherpa/sherpa-onnx-asr.js');
const FIXTURE = resolve(__dirname, '__fixtures__/jfk16k.wav');
const RUNNER = resolve(__dirname, '__fixtures__/sherpaRunner.mjs');

const BUILT =
  existsSync(SHERPA_WASM) &&
  existsSync(SHERPA_DATA) &&
  existsSync(SHERPA_API) &&
  existsSync(FIXTURE);

describe.skipIf(!BUILT)('sherpa-onnx WASM golden transcript', () => {
  it('transcribes the JFK clip through the built module + streaming API', () => {
    const stdout = execFileSync(process.execPath, [RUNNER, SHERPA_JS, SHERPA_API, FIXTURE], {
      encoding: 'utf8',
      timeout: 120_000,
      maxBuffer: 8 * 1024 * 1024,
    });
    const { text } = JSON.parse(stdout) as { text: string };
    const lc = text.toLowerCase();
    // "...ask not what your country can do for you..." — robust content
    // words (the leading "fellow" can warm-up-clip on the 20M model, so
    // we don't assert on it).
    expect(lc).toContain('americans');
    expect(lc).toContain('what your country can do for you');
  });

  it('the fixture is the canonical 16 kHz mono PCM WAV', () => {
    const h = readFileSync(FIXTURE);
    expect(h.toString('ascii', 0, 4)).toBe('RIFF');
    expect(h.readUInt32LE(24)).toBe(16_000); // sample rate
    expect(h.readUInt16LE(22)).toBe(1); // mono
  });
});
