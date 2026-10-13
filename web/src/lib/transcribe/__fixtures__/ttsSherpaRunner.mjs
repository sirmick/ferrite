// Node runner for the TTS → sherpa golden e2e.
//
// Sibling of sherpaRunner.mjs, but the audio is *synthesised* from text
// by espeak-ng rather than a committed clip — a true round-trip
// (text → speech → sherpa → text). Runs the built sherpa-onnx
// Emscripten ASR module in a real Node process (vitest's http module
// runner breaks Emscripten's pthread worker URL).
//
// Mirrors the production engine (`sherpaEngine.ts`): ONE stream for the
// whole run, each utterance fed as a closed VAD-style clip + a short
// zero tail, decoded to completion, read, then `reset` at that endpoint
// so the encoder context carries into the next clip. That carried
// context is what makes the streaming zipformer intelligible — a fresh
// stream per clip is cold every time (see the engine header).
//
// espeak-ng emits 22.05 kHz mono PCM16 WAV; the streaming zipformer
// wants 16 kHz, so we chunk-scan the WAV header (espeak's isn't a bare
// 44-byte one) and linear-resample. Linear is plenty — intelligibility
// here is dominated by the model.
//
// argv: <sherpa-onnx-wasm-main-asr.js> <sherpa-onnx-asr.js> <text>...
//   → prints {"texts": ["...", ...]} JSON, one entry per input text
//     (each spoken + transcribed as its own clip, in order).

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [, , mjsPath, apiPath, ...texts] = process.argv;

/** Same tail the engine appends per clip (`CLIP_TAIL_S`): enough for
 *  the model's chunk + lookahead so the clip is fully decoded in its
 *  own turn, no longer (silent runs degrade the carried state). */
const CLIP_TAIL_S = 0.5;

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

/** espeak-ng → 16 kHz mono f32. */
function speak(text) {
  const dir = mkdtempSync(join(tmpdir(), 'ferrite-tts-'));
  const wavPath = join(dir, 'tts.wav');
  try {
    // Speaking rate (wpm). Overridable for tuning: TTS_WPM=130 node …
    const wpm = process.env.TTS_WPM ?? '140';
    const voice = process.env.TTS_VOICE ?? 'en-us';
    execFileSync('espeak-ng', ['-v', voice, '-s', wpm, '-w', wavPath, text], {
      stdio: 'ignore',
    });
    const { pcm, rate } = decodeWav(readFileSync(wavPath));
    return resampleTo16k(pcm, rate);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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
const results = [];
for (const text of texts) {
  stream.acceptWaveform(16000, speak(text));
  stream.acceptWaveform(16000, new Float32Array(Math.round(16000 * CLIP_TAIL_S)));
  while (recognizer.isReady(stream)) recognizer.decode(stream);
  results.push(recognizer.getResult(stream).text);
  recognizer.reset(stream);
}
process.stdout.write(JSON.stringify({ texts: results }));
process.exit(0);
