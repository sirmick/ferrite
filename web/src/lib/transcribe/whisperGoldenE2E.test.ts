// Golden-transcript e2e — the real ship gate for in-browser STT.
//
// Drives the *actual* whisper.cpp WASM module (built by
// blocks/native/whisper/emscripten/build.sh) — same stance as
// fldigiBrowserE2E: the artifact is built in CI, so if the Emscripten
// build or the C glue ABI regresses this fails loudly.
//
// Runs inference in a real Node process via __fixtures__/whisperRunner
// .mjs (not vitest's http module runner — Emscripten's pthread worker
// needs a file:// URL). Fixture is whisper.cpp's canonical jfk.wav
// (16 kHz mono PCM, committed); tiny.en-q5 recognises it verbatim. The
// assertion is a robust substring so it survives quant/build
// non-determinism. The ham phonetic→callsign layer is proven
// separately in hamPostProcess.test.ts — this gate proves "real audio
// → whisper wasm → glue JSON → sensible text + per-token probs".
//
// skipIf keeps it green-by-skip until `pnpm wasm:build:whisper` has
// run (vendored sources + emsdk); once built it is a hard gate.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { describe, expect, it } from 'vitest';

const WHISPER_MJS = resolve(__dirname, '../wasm/whisper/whisper.mjs');
const WHISPER_WASM = resolve(__dirname, '../wasm/whisper/whisper.wasm');
const MODEL = resolve(__dirname, '../../../static/models/ggml-tiny.en-q5_1.bin');
const FIXTURE = resolve(__dirname, '__fixtures__/jfk16k.wav');
const RUNNER = resolve(__dirname, '__fixtures__/whisperRunner.mjs');

const BUILT = existsSync(WHISPER_WASM) && existsSync(MODEL) && existsSync(FIXTURE);

interface GlueResult {
  segments: { text: string; tokens: { text: string; p: number }[]; avgLogprob: number }[];
}

describe.skipIf(!BUILT)('whisper.cpp WASM golden transcript', () => {
  it('transcribes the JFK clip through the built module + glue ABI', () => {
    const stdout = execFileSync(process.execPath, [RUNNER, WHISPER_MJS, MODEL, FIXTURE], {
      encoding: 'utf8',
      timeout: 90_000,
      maxBuffer: 8 * 1024 * 1024,
    });
    const parsed = JSON.parse(stdout) as GlueResult;

    const text = parsed.segments
      .map((s) => s.text)
      .join(' ')
      .toLowerCase();
    // "And so my fellow Americans, ask not what your country can do
    // for you …" — robust content-word assertions.
    expect(text).toContain('fellow americans');
    expect(text).toContain('country');
    // Glue must emit per-token probabilities (the panel dims on these).
    expect(parsed.segments[0].tokens.length).toBeGreaterThan(0);
    expect(parsed.segments[0].tokens[0].p).toBeGreaterThan(0);
  });

  it('the fixture is the canonical 16 kHz mono PCM WAV', () => {
    const h = readFileSync(FIXTURE);
    expect(h.toString('ascii', 0, 4)).toBe('RIFF');
    expect(h.readUInt32LE(24)).toBe(16_000); // sample rate
    expect(h.readUInt16LE(22)).toBe(1); // mono
  });
});
