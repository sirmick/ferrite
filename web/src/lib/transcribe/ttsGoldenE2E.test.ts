// TTS round-trip e2e — text → espeak-ng speech → our pipeline → text.
//
// Two layers, both gated `skipIf` (green-by-skip until provisioned —
// same stance as whisperGoldenE2E and the fldigi/emsdk gates; CI does
// `apt-get install -y espeak-ng` + `pnpm wasm:build:whisper`):
//
//   1. TTS → whisper (needs espeak-ng + the built whisper wasm/model):
//      synthesised speech actually transcribes, incl. a long
//      no-pause utterance (the continuous-speech case the
//      segmentation/queue work targets).
//   2. TTS → VAD segmenter (needs only espeak-ng, no whisper — fast,
//      deterministic): a long *continuous* utterance is chopped into
//      bounded segments, never one unbounded blob. This is the direct
//      regression gate for the maxSegmentMs cut.
//
// espeak-ng emits 22.05 kHz PCM16; we decode + resample with the
// shipping LinearResampler so the path under test is the real one.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

import { describe, expect, it } from 'vitest';

import { DEFAULT_VAD, EnergyVadSegmenter } from './vad';
import { LinearResampler } from './resample';
import { DEFAULT_HAM_PROMPT } from './hamPrompt';

const WHISPER_MJS = resolve(__dirname, '../wasm/whisper/whisper.mjs');
const WHISPER_WASM = resolve(__dirname, '../wasm/whisper/whisper.wasm');
const MODEL = resolve(__dirname, '../../../static/models/ggml-tiny.en-q5_1.bin');
const RUNNER = resolve(__dirname, '__fixtures__/ttsWhisperRunner.mjs');

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

/** espeak-ng → temp WAV → decoded mono f32 + its sample rate. */
function speak(text: string): { pcm: Float32Array; rate: number } {
  const dir = mkdtempSync(join(tmpdir(), 'ferrite-tts-'));
  const wav = join(dir, 'tts.wav');
  try {
    execFileSync('espeak-ng', ['-v', 'en-us', '-s', '160', '-w', wav, text], { stdio: 'ignore' });
    const buf = readFileSync(wav);
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
    const n = Math.min(dataLen, buf.length - dataOff) >> 1;
    const frames = Math.floor(n / channels);
    const pcm = new Float32Array(frames);
    for (let i = 0; i < frames; i++) {
      let s = 0;
      for (let c = 0; c < channels; c++) s += buf.readInt16LE(dataOff + (i * channels + c) * 2);
      pcm[i] = s / channels / 32768;
    }
    return { pcm, rate };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** How many of `words` appear in `haystack` (case-insensitive). espeak
 *  + tiny.en is intelligible, not verbatim — assert a majority, not
 *  an exact string. */
function hits(haystack: string, words: string[]): number {
  const h = haystack.toLowerCase();
  return words.filter((w) => h.includes(w)).length;
}

/** Fraction of `words` present — graceful scoring for longform, where
 *  espeak + tiny.en will mangle some words but should get the gist. */
function ratio(haystack: string, words: string[]): number {
  return hits(haystack, words) / words.length;
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
    // ≥4 of 6 robust content words — survives espeak/quant noise.
    // (espeak+tiny.en renders this verbatim; the pangram does not —
    // on-topic plain prose is the realistic input anyway.)
    expect(
      hits(out, ['radio', 'operator', 'reported', 'clear', 'signal', 'station']),
    ).toBeGreaterThanOrEqual(4);
  });

  it('round-trips a long continuous (no-pause) utterance', () => {
    const out = transcribe(
      'this is a long continuous radio transmission with no pauses ' +
        'the weather today is clear with a light wind from the north ' +
        'and the signal report is five nine here in the city',
      DEFAULT_HAM_PROMPT,
    );
    expect(
      hits(out, ['continuous', 'transmission', 'weather', 'clear', 'wind', 'signal', 'city']),
    ).toBeGreaterThanOrEqual(4);
  });

  it('round-trips a longform monologue with the ham vocab prompt, gracefully', () => {
    // ~40 s of realistic on-air prose — the "set it and leave it"
    // case. Biased with the *actual* shipping vocab (DEFAULT_HAM_PROMPT,
    // = rollingPrompt()'s base) so the test mirrors production. espeak
    // + tiny.en will mangle words; the gate is the GIST, not verbatim —
    // ≥55 % of content words present.
    const out = transcribe(
      'good evening and thanks for the call this is a long form test of the ' +
        'transcription system on the amateur radio band the receiver is ' +
        'working well tonight and the signal report is five and nine with ' +
        'very little noise on the channel conditions on the band are good ' +
        'and the antenna is performing nicely the weather here is cold and ' +
        'clear with a light wind from the north thanks again for the contact ' +
        'and i will listen for any other stations calling now',
      DEFAULT_HAM_PROMPT,
    );
    const words = [
      'evening',
      'transcription',
      'system',
      'amateur',
      'radio',
      'receiver',
      'signal',
      'report',
      'noise',
      'conditions',
      'antenna',
      'weather',
      'clear',
      'wind',
      'stations',
    ];
    expect(ratio(out, words)).toBeGreaterThanOrEqual(0.55);
  });
});

describe.skipIf(!TTS)('TTS → VAD segmenter (continuous speech is bounded)', () => {
  it('chops a long no-pause utterance into segments capped at maxSegmentMs', () => {
    // ~20 s of run-on speech, no punctuation → the gate never closes
    // on silence, only the maxSegmentMs hard cut. Pre-fix this was one
    // 28 s blob (or worse); now every segment is ≤ the 10 s cap.
    const phrase =
      'this is a very long continuous transmission with no pauses at all ' +
      'just one operator talking and talking about the band conditions ' +
      'the antenna the weather and the contest while never stopping to ' +
      'take a breath so the voice activity gate can only ever close on ' +
      'the hard duration cap and not on any moment of silence whatsoever';
    const { pcm, rate } = speak(phrase);

    const segs: Float32Array[] = [];
    const seg = new EnergyVadSegmenter({}, (s) => segs.push(s));
    const rs = new LinearResampler(rate);
    // Feed in ring-sized chunks like the worker does, so seam handling
    // is exercised too.
    const CHUNK = 4096;
    for (let i = 0; i < pcm.length; i += CHUNK) {
      seg.feed(rs.feed(pcm.subarray(i, i + CHUNK)));
    }
    // Trailing silence so the final open segment flushes.
    seg.feed(new Float32Array(16_000)); // 1 s @ 16 k

    const durations = segs.map((s) => (s.length / 16_000) * 1000);
    const total = durations.reduce((a, b) => a + b, 0);

    expect(segs.length).toBeGreaterThanOrEqual(1);
    // The whole point: no segment exceeds the hard cap (+1 frame slack).
    for (const ms of durations) {
      expect(ms).toBeLessThanOrEqual(DEFAULT_VAD.maxSegmentMs + 50);
    }
    // It really was a long utterance (sanity the TTS produced enough).
    expect(total).toBeGreaterThan(8_000);
    // >cap of continuous speech ⇒ it had to be cut at least once.
    if (total > DEFAULT_VAD.maxSegmentMs) {
      expect(segs.length).toBeGreaterThanOrEqual(2);
    }
  });
});
