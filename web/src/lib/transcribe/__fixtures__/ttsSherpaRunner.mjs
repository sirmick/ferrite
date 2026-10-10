// Node runner for the TTS → sherpa golden e2e.
//
// Sibling of sherpaRunner.mjs, but the audio is *synthesised* from text
// by espeak-ng rather than a committed clip — a true round-trip
// (text → speech → sherpa → text). Runs the built sherpa-onnx
// Emscripten ASR module in a real Node process (vitest's http module
// runner breaks Emscripten's pthread worker URL).
//
// espeak-ng emits 22.05 kHz mono PCM16 WAV; the streaming zipformer
// wants 16 kHz, so we chunk-scan the WAV header (espeak's isn't a bare
// 44-byte one) and linear-resample. Linear is plenty — intelligibility
// here is dominated by the model.
//
// argv: <sherpa-onnx-wasm-main-asr.js> <sherpa-onnx-asr.js> <text>
//   → prints {"text": "..."} JSON.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [, , mjsPath, apiPath, text] = process.argv;

/** Scan RIFF chunks (don't assume a 44-byte header) → mono f32 + rate. */
function decodeWav(buf) {
  if (buf.toString('ascii', 0, 4) !== 'RIFF' || buf.toString('ascii', 8, 12) !== 'WAVE') {
    throw new Error('not a WAV');
  }
  let p = 12;
  let fmt;
  let dataOff = -1;
  let dataLen = 0;
  while (p + 8 <= buf.length) {
    const id = buf.toString('ascii', p, p + 4);
    const sz = buf.readUInt32LE(p + 4);
    if (id === 'fmt ') {
      fmt = {
        format: buf.readUInt16LE(p + 8),
        channels: buf.readUInt16LE(p + 10),
        rate: buf.readUInt32LE(p + 12),
        bits: buf.readUInt16LE(p + 22),
      };
    } else if (id === 'data') {
      dataOff = p + 8;
      dataLen = sz;
    }
    p += 8 + sz + (sz & 1); // chunks are word-aligned
  }
  if (!fmt || dataOff < 0) throw new Error('WAV missing fmt/data');
  if (fmt.format !== 1 || fmt.bits !== 16) throw new Error('expected PCM16 WAV');
  const n = Math.min(dataLen, buf.length - dataOff) >> 1;
  const ch = fmt.channels || 1;
  const frames = Math.floor(n / ch);
  const mono = new Float32Array(frames);
  for (let i = 0; i < frames; i++) {
    let s = 0;
    for (let c = 0; c < ch; c++) s += buf.readInt16LE(dataOff + (i * ch + c) * 2);
    mono[i] = s / ch / 32768;
  }
  return { pcm: mono, rate: fmt.rate };
}

function resampleTo16k(pcm, srcRate) {
  if (srcRate === 16_000) return pcm;
  const ratio = 16_000 / srcRate;
  const out = new Float32Array(Math.floor(pcm.length * ratio));
  for (let i = 0; i < out.length; i++) {
    const x = i / ratio;
    const i0 = Math.floor(x);
    const frac = x - i0;
    const a = pcm[i0] ?? 0;
    const b = pcm[i0 + 1] ?? a;
    out[i] = a + (b - a) * frac;
  }
  return out;
}

const dir = mkdtempSync(join(tmpdir(), 'ferrite-tts-'));
const wavPath = join(dir, 'tts.wav');
let pcm16k;
try {
  // Speaking rate (wpm). Overridable for tuning: TTS_WPM=130 node …
  const wpm = process.env.TTS_WPM ?? '140';
  const voice = process.env.TTS_VOICE ?? 'en-us';
  execFileSync('espeak-ng', ['-v', voice, '-s', wpm, '-w', wavPath, text], {
    stdio: 'ignore',
  });
  const { pcm, rate } = decodeWav(readFileSync(wavPath));
  pcm16k = resampleTo16k(pcm, rate);
} finally {
  rmSync(dir, { recursive: true, force: true });
}

const wasmPath = mjsPath.replace(/\.js$/, '.wasm');
const dataPath = mjsPath.replace(/\.js$/, '.data');

const { default: createSherpaAsrModule } = await import(pathToFileURL(mjsPath).href);
const { createOnlineRecognizer } = await import(pathToFileURL(apiPath).href);

const Module = await createSherpaAsrModule({
  wasmBinary: readFileSync(wasmPath),
  locateFile: (path) => {
    if (path.endsWith('.wasm')) return wasmPath;
    if (path.endsWith('.data')) return dataPath;
    return path;
  },
});

const recognizer = createOnlineRecognizer(Module);
const stream = recognizer.createStream();
// The streaming zipformer warm-up clips the first few hundred ms of a
// stream (the JFK golden notes the same). espeak starts speaking at
// t=0, so lead with 1 s of silence — in the live path the VAD clip
// carries its own pre-roll, so this mirrors production rather than
// papering over a model fault.
stream.acceptWaveform(16000, new Float32Array(16000));
stream.acceptWaveform(16000, pcm16k);
// ~0.5 s of silence flushes the last words through the feature window.
stream.acceptWaveform(16000, new Float32Array(8000));
while (recognizer.isReady(stream)) recognizer.decode(stream);
const result = recognizer.getResult(stream).text;
process.stdout.write(JSON.stringify({ text: result }));
process.exit(0);
