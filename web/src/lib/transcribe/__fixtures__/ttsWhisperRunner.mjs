// Node runner for the TTS → whisper golden e2e.
//
// Sibling of whisperRunner.mjs, but the audio is *synthesised* from
// text by espeak-ng rather than a committed clip — a true round-trip
// (text → speech → whisper → text). Runs the built whisper.cpp
// Emscripten module in a real Node process (vitest's http module
// runner breaks Emscripten's pthread worker URL).
//
// espeak-ng emits 22.05 kHz mono PCM16 WAV; whisper wants 16 kHz, so
// we chunk-scan the WAV header (espeak's isn't a bare 44-byte one) and
// linear-resample. Linear is plenty — intelligibility here is
// dominated by the model, mirroring resample.ts's stance.
//
// argv: <whisper.mjs> <model.bin> <text> [prompt]  → prints glue JSON.
// `prompt` (optional) is whisper's `initial_prompt` vocabulary bias —
// the same lever the live pipeline uses (rollingPrompt() = ham corpus
// + recent callsigns). Passing the real corpus mirrors production.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [, , mjsPath, modelPath, text, promptText = ''] = process.argv;

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
try {
  execFileSync('espeak-ng', ['-v', 'en-us', '-s', '160', '-w', wavPath, text], {
    stdio: 'ignore',
  });
  const { pcm, rate } = decodeWav(readFileSync(wavPath));
  const pcm16k = resampleTo16k(pcm, rate);

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

  const pcmPtr = M._malloc(pcm16k.length * 4);
  M.HEAPF32.set(pcm16k, pcmPtr / 4);
  let promptPtr = 0;
  if (promptText) {
    const len = M.lengthBytesUTF8(promptText) + 1;
    promptPtr = M._malloc(len);
    M.stringToUTF8(promptText, promptPtr, len);
  }
  const jsonPtr = M._wsp_transcribe(pcmPtr, pcm16k.length, promptPtr);
  M._free(pcmPtr);
  if (promptPtr) M._free(promptPtr);
  if (!jsonPtr) {
    console.error('wsp_transcribe returned null');
    process.exit(3);
  }
  process.stdout.write(M.UTF8ToString(jsonPtr));
  M._free(jsonPtr);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
process.exit(0);
