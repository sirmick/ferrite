// Node runner for the sherpa-onnx golden e2e.
//
// Runs the built sherpa-onnx Emscripten ASR module in a *real* Node
// process (not vitest's http module runner) — same reason as
// whisperRunner.mjs: Emscripten's pthread workers resolve via file://.
// Drives the streaming recognizer over a whole clip (acceptWaveform →
// tail-pad → decode-until-ready → getResult), mirroring how
// sherpaEngine.transcribe() finalizes one VAD clip in the browser.
//
// argv: <sherpa-onnx-wasm-main-asr.js> <sherpa-onnx-asr.js> <audio.wav>
//   → prints {"text": "..."} JSON.

import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const [, , mjsPath, apiPath, wavPath] = process.argv;

function wavToF32(buf) {
  const off = 44; // canonical PCM16 mono WAV
  const n = (buf.length - off) >> 1;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = buf.readInt16LE(off + i * 2) / 32768;
  return out;
}

const wasmPath = mjsPath.replace(/\.js$/, '.wasm');
const dataPath = mjsPath.replace(/\.js$/, '.data');

const { default: createSherpaAsrModule } = await import(pathToFileURL(mjsPath).href);
const { createOnlineRecognizer } = await import(pathToFileURL(apiPath).href);

const Module = await createSherpaAsrModule({
  wasmBinary: readFileSync(wasmPath),
  // The preloaded model (.data) is fetched by basename — point both
  // sidecars at their absolute paths so node doesn't resolve off cwd.
  locateFile: (path) => {
    if (path.endsWith('.wasm')) return wasmPath;
    if (path.endsWith('.data')) return dataPath;
    return path;
  },
});

const recognizer = createOnlineRecognizer(Module);
const stream = recognizer.createStream();
const pcm = wavToF32(readFileSync(wavPath));
stream.acceptWaveform(16000, pcm);
// ~0.5 s of silence flushes the last words through the feature window.
stream.acceptWaveform(16000, new Float32Array(8000));
while (recognizer.isReady(stream)) recognizer.decode(stream);
const text = recognizer.getResult(stream).text;
process.stdout.write(JSON.stringify({ text }));
process.exit(0);
