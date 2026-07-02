// TTS round-trip e2e — text → espeak-ng speech → our pipeline → text.
//
// Two layers, both gated `skipIf` (green-by-skip until provisioned —
// same stance as whisperGoldenE2E and the fldigi/emsdk gates; CI does
// `apt-get install -y espeak-ng` + `pnpm wasm:build:whisper`):
//
//   1. TTS → whisper (needs espeak-ng + the built whisper wasm/model):
//      synthesised speech actually transcribes, incl. a long no-pause
//      utterance (the continuous-speech case the segmentation/queue work
//      targets). Runs whisper in a subprocess (`ttsWhisperRunner.mjs`)
//      since the Emscripten module's async fetch-init doesn't work under
//      vitest/jsdom — the runner does its own minimal resample.
//   2. TTS → shared Rust core (needs only espeak-ng): a long
//      *continuous* utterance is chopped into bounded segments, never one
//      unbounded blob. Drives the WASM `WasmTranscriber` (the SAME core
//      the worker and the node-side block run) directly — resample + VAD
//      + queue — so this is the regression gate for the maxSegmentMs cut
//      on the unified path. The wasm core inits synchronously from bytes
//      (`initSync`), which works in node (unlike the whisper module).
//
// The old in-process layer-2 drove the TypeScript `EnergyVadSegmenter` +
// `LinearResampler`; those are gone (one Rust impl now), so it drives the
// wasm core instead. The core's logic is unit-tested in isolation in
// blocks/native/whisper; this covers it on real TTS audio.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { describe, expect, it } from 'vitest';

import { initSync, WasmTranscriber } from '../wasm/blocks/ferrite_blocks';

const WHISPER_MJS = resolve(__dirname, '../wasm/whisper/whisper.mjs');
const WHISPER_WASM = resolve(__dirname, '../wasm/whisper/whisper.wasm');
const MODEL = resolve(__dirname, '../../../static/models/ggml-tiny.en-q5_1.bin');
const RUNNER = resolve(__dirname, '__fixtures__/ttsWhisperRunner.mjs');
const BLOCKS_WASM = resolve(__dirname, '../wasm/blocks/ferrite_blocks_bg.wasm');

/** The VAD hard segment cap (ms) — mirrors `VadConfig::default`'s
 *  `max_segment_ms` in blocks/native/whisper/src/vad.rs. The whole point
 *  of layer 2 is that no segment of continuous speech exceeds this. */
const MAX_SEGMENT_MS = 10_000;
/** DEFAULT_HAM_PROMPT lives in the Rust core now; layer 1's runner uses
 *  the engine default, and layer 2 doesn't need a prompt. */

function hasEspeak(): boolean {
  try {
    execFileSync('espeak-ng', ['--version'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

const TTS = hasEspeak();
const WHISPER_BUILT = existsSync(WHISPER_WASM) && existsSync(MODEL);
const BLOCKS_BUILT = existsSync(BLOCKS_WASM);

/** espeak-ng → WAV bytes → decoded mono f32 + its sample rate. */
function speak(text: string): { pcm: Float32Array; rate: number } {
  // `-w /dev/stdout` would interleave the WAV header oddly through the
  // pipe on some builds; capture to a buffer via stdout is reliable for
  // espeak-ng's `--stdout`.
  const buf = execFileSync('espeak-ng', ['-v', 'en-us', '-s', '160', '--stdout', text], {
    maxBuffer: 64 * 1024 * 1024,
  });
  if (buf.toString('ascii', 0, 4) !== 'RIFF') throw new Error('not a WAV');
  let p = 12;
  let rate = 22_050;
  let bits = 16;
  let channels = 1;
  let dataOff = -1;
  let dataLen = 0;
  while (p + 8 <= buf.length) {
    const id = buf.toString('ascii', p, p + 4);
    const sz = buf.readUInt32LE(p + 4);
    if (id === 'fmt ') {
      channels = buf.readUInt16LE(p + 10);
      rate = buf.readUInt32LE(p + 12);
      bits = buf.readUInt16LE(p + 22);
    } else if (id === 'data') {
      dataOff = p + 8;
      dataLen = sz;
    }
    p += 8 + sz + (sz & 1);
  }
  if (dataOff < 0 || bits !== 16) throw new Error('expected PCM16 WAV');
  // espeak-ng `--stdout` streams, so the `data` chunk size is a
  // placeholder (0 or 0xFFFFFFFF) — it can't seek back to fill it. Trust
  // the actual bytes that arrived, not the header field.
  const avail = buf.length - dataOff;
  const dataBytes = dataLen > 0 && dataLen <= avail ? dataLen : avail;
  const n = dataBytes >> 1;
  const frames = Math.floor(n / channels);
  const pcm = new Float32Array(frames);
  for (let i = 0; i < frames; i++) {
    let s = 0;
    for (let c = 0; c < channels; c++) s += buf.readInt16LE(dataOff + (i * channels + c) * 2);
    pcm[i] = s / channels / 32768;
  }
  return { pcm, rate };
}

/** How many of `words` appear in `haystack` (case-insensitive). espeak +
 *  tiny.en is intelligible, not verbatim — assert a majority. */
function hits(haystack: string, words: string[]): number {
  const h = haystack.toLowerCase();
  return words.filter((w) => h.includes(w)).length;
}

describe.skipIf(!(TTS && WHISPER_BUILT))('TTS → whisper round-trip', () => {
  function transcribe(text: string, prompt = ''): string {
    const args = [RUNNER, WHISPER_MJS, MODEL, text];
    if (prompt) args.push(prompt);
    const stdout = execFileSync(process.execPath, args, {
      encoding: 'utf8',
      timeout: 180_000,
      maxBuffer: 8 * 1024 * 1024,
    });
    const parsed = JSON.parse(stdout) as { segments: { text: string }[] };
    return parsed.segments.map((s) => s.text).join(' ');
  }

  it('round-trips a short spoken phrase', () => {
    const out = transcribe('the radio operator reported a clear signal from the station');
    expect(
      hits(out, ['radio', 'operator', 'reported', 'clear', 'signal', 'station']),
    ).toBeGreaterThanOrEqual(4);
  });

  it('round-trips a long continuous (no-pause) utterance', () => {
    const out = transcribe(
      'this is a long continuous radio transmission with no pauses ' +
        'the weather today is clear with a light wind from the north ' +
        'and the signal report is five nine here in the city',
    );
    expect(
      hits(out, ['continuous', 'transmission', 'weather', 'clear', 'wind', 'signal', 'city']),
    ).toBeGreaterThanOrEqual(4);
  });
});

describe.skipIf(!(TTS && BLOCKS_BUILT))('TTS → shared core (continuous speech is bounded)', () => {
  it('chops a long no-pause utterance into segments capped at maxSegmentMs', () => {
    // Init the blocks wasm synchronously from bytes — the fetch-based
    // default init doesn't work under node/jsdom, but initSync does (same
    // pattern the runner DSP tests use for the runtime wasm).
    initSync({ module: readFileSync(BLOCKS_WASM) });

    // ~20 s of run-on speech, no punctuation → the VAD never closes on
    // silence, only the maxSegmentMs hard cut.
    const phrase =
      'this is a very long continuous transmission with no pauses at all ' +
      'just one operator talking and talking about the band conditions ' +
      'the antenna the weather and the contest while never stopping to ' +
      'take a breath so the voice activity gate can only ever close on ' +
      'the hard duration cap and not on any moment of silence whatsoever';
    const { pcm, rate } = speak(phrase);

    // Drive the SAME core the worker runs: feed at the source rate (it
    // resamples to 16 k internally), then trailing silence so the final
    // open segment flushes on hangover.
    const core = new WasmTranscriber(rate);
    const CHUNK = 4096;
    for (let i = 0; i < pcm.length; i += CHUNK) {
      core.feed(pcm.subarray(i, i + CHUNK));
    }
    core.feed(new Float32Array(rate)); // 1 s of silence at the source rate

    const durations: number[] = [];
    let clip = core.takePending();
    while (clip) {
      durations.push((clip.length / 16_000) * 1000);
      clip = core.takePending();
    }
    const total = durations.reduce((a, b) => a + b, 0);

    expect(durations.length).toBeGreaterThanOrEqual(1);
    // The whole point: no segment exceeds the hard cap (+1 frame slack).
    for (const ms of durations) {
      expect(ms).toBeLessThanOrEqual(MAX_SEGMENT_MS + 50);
    }
    // It really was a long utterance (sanity the TTS produced enough).
    expect(total).toBeGreaterThan(8_000);
    // >cap of continuous speech ⇒ it had to be cut at least once.
    if (total > MAX_SEGMENT_MS) {
      expect(durations.length).toBeGreaterThanOrEqual(2);
    }
  });
});
