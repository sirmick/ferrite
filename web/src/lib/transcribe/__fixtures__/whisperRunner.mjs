// Node runner for the whisper golden e2e.
//
// Runs the built whisper.cpp Emscripten module in a *real* Node
// process — not vitest's http module runner — so Emscripten's
// `ENVIRONMENT=worker,node` pthread path resolves its worker via a
// file:// URL (vitest serves modules over http://, which breaks
// Emscripten's `new Worker(new URL(...))`). The test execs this and
// parses the JSON the glue ABI returns.
//
// argv: <whisper.mjs> <model.bin> <audio.wav>  → prints glue JSON.

import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const [, , mjsPath, modelPath, wavPath] = process.argv;

function wavToF32(buf) {
  const off = 44; // canonical PCM16 mono WAV
  const n = (buf.length - off) >> 1;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = buf.readInt16LE(off + i * 2) / 32768;
  return out;
}

const { default: createWhisperModule } = await import(pathToFileURL(mjsPath).href);
const M = await createWhisperModule({
  wasmBinary: readFileSync(mjsPath.replace(/\.mjs$/, '.wasm')),
});

const model = readFileSync(modelPath);
const mPtr = M._malloc(model.length);
M.HEAPU8.set(model, mPtr);
if (M._wsp_init(mPtr, model.length, 0, 0) !== 1) {
  console.error('wsp_init failed');
  process.exit(2);
}
M._free(mPtr);

const pcm = wavToF32(readFileSync(wavPath));
const pcmPtr = M._malloc(pcm.length * 4);
M.HEAPF32.set(pcm, pcmPtr / 4);
const jsonPtr = M._wsp_transcribe(pcmPtr, pcm.length, 0);
M._free(pcmPtr);
if (!jsonPtr) {
  console.error('wsp_transcribe returned null');
  process.exit(3);
}
process.stdout.write(M.UTF8ToString(jsonPtr));
M._free(jsonPtr);
process.exit(0);
