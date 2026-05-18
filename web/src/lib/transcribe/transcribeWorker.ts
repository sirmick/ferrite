// Transcription Worker — one per VoiceTranscribe block.
//
// Drains the block's tap SAB on its own coarse cadence (never the
// audio clock), resamples to 16 kHz, VAD-gates into utterances, and
// runs each through whisper.cpp. Inference is heavy and fully
// off-thread here, so it can never stall audio or the UI. Posts
// results back to the main thread, where browserRuntime forwards them
// to the transcript store.
//
// Graceful degradation: if the whisper.cpp WASM artifact isn't built
// the Worker still runs (draining the ring so the glitch counter is
// honest) but reports `unavailable` and emits no segments — the
// VoiceTranscribe block is pure passthrough so audio is unaffected.

import { AudioRingReader } from '../audio/ringBuffer';
import { applyHamPostProcess, extractCallsigns } from './hamPostProcess';
import { LinearResampler } from './resample';
import { EnergyVadSegmenter } from './vad';
import { DEFAULT_HAM_PROMPT } from './hamPrompt';
import {
  EngineUnavailableError,
  MODELS,
  WhisperEngine,
  type WhisperModelDef,
} from './whisperEngine';

type InMsg =
  | { type: 'init'; sab: SharedArrayBuffer; blockId: string }
  | { type: 'rate'; rateHz: number }
  | { type: 'model'; modelId: string }
  | { type: 'prompt'; text: string }
  | { type: 'stop' };

type OutMsg =
  | { type: 'status'; status: string; detail: string; model: string; vad: 'silero' | 'energy' }
  | { type: 'dropped'; total: number }
  | {
      type: 'segment';
      atMs: number;
      t0: number;
      t1: number;
      text: string;
      tokens: { text: string; p: number }[];
      confidence: number;
      noSpeechProb: number;
    };

const POLL_MS = 150;
/** Ham-vocabulary `initial_prompt` base. Defaults to the dense corpus
 *  in hamPrompt.ts; overridden live by the Transcript tab's editable
 *  field (browserRuntime forwards a `prompt` message). The rolling
 *  recently-heard callsigns are appended after it so the bias self-
 *  reinforces within a QSO. */
let promptBase = DEFAULT_HAM_PROMPT;
const MAX_PROMPT_CALLS = 16;

let reader: AudioRingReader | undefined;
let resampler: LinearResampler | undefined;
let segmenter: EnergyVadSegmenter | undefined;
let engine: WhisperEngine | undefined;
let timer: ReturnType<typeof setInterval> | undefined;
let droppedTotal = 0;
let lastAvail = 0;
const recentCalls: string[] = [];
let curModel: WhisperModelDef = MODELS[0];

// Closed utterances waiting for whisper. `whisper_full` is a blocking
// WASM call (seconds for a multi-second clip) and this Worker is
// single-threaded, so segments that close while one is running are
// QUEUED, not dropped — continuous speech closes a fresh segment every
// `maxSegmentMs`, and the old "drop if busy" lost whole sentences.
// Bounded: a speaker faster than whisper sheds the OLDEST backlog so
// latency can't balloon, counted as a glitch.
const pendingSegs: Float32Array[] = [];
const MAX_PENDING = 6;
let draining = false;

const scratch = new Float32Array(65536);

function post(msg: OutMsg): void {
  (self as unknown as Worker).postMessage(msg);
}

function status(s: string, detail = ''): void {
  post({
    type: 'status',
    status: s,
    detail,
    model: curModel.label,
    vad: engine?.vadAvailable ? 'silero' : 'energy',
  });
}

function rollingPrompt(): string {
  return promptBase + recentCalls.slice(-MAX_PROMPT_CALLS).join(' ');
}

function transcribeOne(pcm16k: Float32Array): void {
  if (!engine?.loaded) return;
  status('transcribing');
  const res = engine.transcribe(pcm16k, rollingPrompt());
  const atMs = Date.now();
  for (const seg of res.segments) {
    const cleaned = applyHamPostProcess(seg.text);
    if (!cleaned) continue;
    for (const c of extractCallsigns(cleaned)) {
      if (!recentCalls.includes(c)) recentCalls.push(c);
    }
    post({
      type: 'segment',
      atMs,
      t0: seg.t0,
      t1: seg.t1,
      text: cleaned,
      tokens: seg.tokens ?? [],
      // avg log-prob → rough 0..1 confidence for the panel's dimming.
      confidence: Math.max(0, Math.min(1, Math.exp(seg.avgLogprob))),
      noSpeechProb: seg.noSpeechProb ?? 0,
    });
  }
}

function enqueueSegment(pcm16k: Float32Array): void {
  if (!engine?.loaded) return;
  if (pendingSegs.length >= MAX_PENDING) {
    // Whisper is behind the speaker — shed the oldest utterance so
    // latency stays bounded; surfaced as a glitch.
    pendingSegs.shift();
    droppedTotal += 1;
    post({ type: 'dropped', total: droppedTotal });
  }
  pendingSegs.push(pcm16k);
  void drainSegments();
}

// Synchronous in practice (no awaits — `engine.transcribe` is a
// blocking WASM call). The `draining` guard makes the re-entrant
// poll()→segmenter→enqueueSegment path safe: a segment closed *during*
// drain just lands on the queue and the loop picks it up.
async function drainSegments(): Promise<void> {
  if (draining) return;
  draining = true;
  try {
    while (pendingSegs.length > 0 && engine?.loaded) {
      const pcm = pendingSegs.shift() as Float32Array;
      try {
        transcribeOne(pcm);
      } catch (e) {
        status('error', String(e));
      }
      // Drain the SAB between queued inferences so a backlog doesn't
      // overflow the ring while we catch up.
      poll();
    }
    status('listening');
  } finally {
    draining = false;
  }
}

function poll(): void {
  if (!reader) return;
  // Drain everything available; the ring is small and the producer
  // (runner tick) is faster than this poll, so empty it each pass.
  let n = reader.read(scratch);
  while (n > 0) {
    if (resampler && segmenter) {
      const r = resampler.feed(scratch.subarray(0, n));
      if (r.length > 0) segmenter.feed(r);
    }
    if (n < scratch.length) break;
    n = reader.read(scratch);
  }
  // Surface ring-overflow as the panel's glitch counter. The producer
  // tracks drops Rust-side; we approximate from the high-water gap.
  const avail = reader.availableRead();
  if (avail < lastAvail) droppedTotal += 0; // overflow is counted Rust-side
  lastAvail = avail;
  post({ type: 'dropped', total: droppedTotal });
}

async function init(sab: SharedArrayBuffer): Promise<void> {
  reader = AudioRingReader.fromSab(sab);
  segmenter = new EnergyVadSegmenter({}, (pcm) => enqueueSegment(pcm));
  timer = setInterval(poll, POLL_MS);

  status('loading-model', `${curModel.label} (~${curModel.approxMB} MB, one-time)`);
  engine = new WhisperEngine();
  try {
    await engine.load(curModel);
    status('listening');
  } catch (e) {
    engine = undefined;
    if (e instanceof EngineUnavailableError) {
      status(
        'unavailable',
        'whisper.cpp WASM not built. Audio is unaffected (passthrough). ' +
          'Run pnpm wasm:build:whisper to enable transcription.',
      );
    } else {
      status('error', String(e));
    }
  }
}

self.onmessage = (ev: MessageEvent<InMsg>) => {
  const msg = ev.data;
  switch (msg.type) {
    case 'init':
      void init(msg.sab);
      break;
    case 'rate':
      resampler = new LinearResampler(msg.rateHz);
      break;
    case 'model': {
      const m = MODELS.find((x) => x.id === msg.modelId);
      if (m && m.id !== curModel.id) {
        curModel = m;
        if (engine) {
          status('loading-model', `${m.label} (~${m.approxMB} MB)`);
          engine
            .load(m)
            .then(() => status('listening'))
            .catch((e) => status('error', String(e)));
        }
      }
      break;
    }
    case 'prompt':
      // Empty → fall back to the built-in ham corpus.
      promptBase = msg.text.trim() ? msg.text : DEFAULT_HAM_PROMPT;
      break;
    case 'stop':
      if (timer) clearInterval(timer);
      timer = undefined;
      reader = undefined;
      segmenter = undefined;
      break;
  }
};
