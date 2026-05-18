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
      // Live behaviour for the panel's gate/level meter + backlog
      // readout. Posted every poll (~150 ms) once armed.
      type: 'telemetry';
      gateOpen: boolean;
      level: number;
      threshold: number;
      queued: number;
      lagMs: number;
    }
  | {
      type: 'segment';
      atMs: number;
      t0: number;
      t1: number;
      text: string;
      tokens: { text: string; p: number }[];
      confidence: number;
      noSpeechProb: number;
      /** This segment continues the previous one with no speaker pause
       *  between (mid-utterance: a max-cut split, or a later sub-
       *  segment of the same clip). */
      cont: boolean;
      /** Silence (ms) before this utterance — only the first chunk of
       *  an utterance carries it; 0 otherwise. The rolling transcript
       *  starts a new paragraph when it exceeds the threshold. */
      gapMs: number;
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
let srcRateHz = 0;
const recentCalls: string[] = [];
let curModel: WhisperModelDef = MODELS[0];

// Closed utterances waiting for whisper. `whisper_full` is a blocking
// WASM call (seconds for a multi-second clip) and this Worker is
// single-threaded, so segments that close while one is running are
// QUEUED, not dropped — continuous speech closes a fresh segment every
// `maxSegmentMs`, and the old "drop if busy" lost whole sentences.
// Bounded: a speaker faster than whisper sheds the OLDEST backlog so
// latency can't balloon, counted as a glitch.
interface PendingSeg {
  pcm: Float32Array;
  /** ms at the start of `pcm` carried from the previous max-cut —
   *  whisper segments fully inside this were already emitted. */
  leadMs: number;
  /** Silence (ms) before this utterance — for the paragraph break. */
  gapMs: number;
}
const pendingSegs: PendingSeg[] = [];
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

function transcribeOne(pcm16k: Float32Array, leadMs: number, gapMs: number): void {
  if (!engine?.loaded) return;
  status('transcribing');
  const res = engine.transcribe(pcm16k, rollingPrompt());
  // The clip just ended (~now). Whisper segment times (`t1`, seconds
  // into the clip) place each segment in real wall-clock relative to
  // that end — otherwise every segment of a 10 s utterance shares one
  // timestamp and the band log loses intra-utterance ordering.
  const endMs = Date.now();
  const lastT1 = res.segments.length ? (res.segments[res.segments.length - 1].t1 ?? 0) : 0;
  // Carried-over max-cut lead: whisper segments that end inside it were
  // already emitted by the previous chunk — drop them. A segment that
  // straddles the boundary (t1 past the lead) is kept: that's the word
  // we re-decoded *with* its lead-in context, the whole point.
  const leadSec = leadMs / 1000;
  let kept = 0;
  for (const seg of res.segments) {
    if (leadSec > 0 && (seg.t1 ?? 0) <= leadSec) continue;
    const cleaned = applyHamPostProcess(seg.text);
    if (!cleaned) continue;
    // The first kept segment continues the prior chunk iff this clip
    // carried a max-cut lead (mid-utterance). Later sub-segments of the
    // same clip are always continuous. `cont=false` ⇒ fresh utterance
    // after a silence ⇒ paragraph break in the rolling transcript.
    const cont = kept > 0 || leadMs > 0;
    // Only the very first kept chunk of an utterance carries the
    // preceding pause; later sub-segments are mid-utterance.
    const segGapMs = kept === 0 && leadMs === 0 ? gapMs : 0;
    kept += 1;
    const atMs = Math.round(endMs - Math.max(0, lastT1 - (seg.t1 ?? lastT1)) * 1000);
    for (const c of extractCallsigns(cleaned)) {
      if (!recentCalls.includes(c)) {
        recentCalls.push(c);
        // Bound it: only the last MAX_PROMPT_CALLS feed the prompt, and
        // an ever-growing array is both a leak and (via the prompt) a
        // whisper repetition trigger. Keep a little history headroom.
        if (recentCalls.length > MAX_PROMPT_CALLS * 3) recentCalls.shift();
      }
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
      cont,
      gapMs: segGapMs,
    });
  }
}

function enqueueSegment(pcm16k: Float32Array, leadMs: number, gapMs: number): void {
  if (!engine?.loaded) return;
  if (pendingSegs.length >= MAX_PENDING) {
    // Whisper is behind the speaker — shed the oldest utterance so
    // latency stays bounded; surfaced as a glitch.
    pendingSegs.shift();
    droppedTotal += 1;
    post({ type: 'dropped', total: droppedTotal });
  }
  pendingSegs.push({ pcm: pcm16k, leadMs, gapMs });
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
      const item = pendingSegs.shift() as PendingSeg;
      try {
        transcribeOne(item.pcm, item.leadMs, item.gapMs);
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
  // Don't touch the ring until the resampler exists. The `rate`
  // message arrives a few runner ticks after `init` (the runner only
  // knows the negotiated input rate once the block has run), so
  // draining here pre-rate would *consume and discard* the first
  // ~hundreds of ms of speech. The SAB ring is large (≈ seconds), so
  // leaving the audio buffered until `rate` lands loses nothing.
  if (!reader || !resampler || !segmenter) return;
  let n = reader.read(scratch);
  while (n > 0) {
    const r = resampler.feed(scratch.subarray(0, n));
    if (r.length > 0) segmenter.feed(r);
    if (n < scratch.length) break;
    n = reader.read(scratch);
  }
  // `droppedTotal` reflects pending-queue shedding (whisper behind the
  // speaker). Ring overflow isn't separately surfaced — the big ring +
  // queue make it rare, and the Rust side doesn't expose a count.
  post({ type: 'dropped', total: droppedTotal });

  // Live behaviour for the panel. `lagMs` = audio captured but not yet
  // transcribed: still-in-ring backlog + queued utterances' duration.
  const m = segmenter.meterState();
  const ringMs = srcRateHz > 0 ? (reader.availableRead() / srcRateHz) * 1000 : 0;
  let queuedMs = 0;
  for (const s of pendingSegs) queuedMs += (s.pcm.length / 16_000) * 1000;
  post({
    type: 'telemetry',
    gateOpen: m.speaking,
    level: m.level,
    threshold: m.threshold,
    queued: pendingSegs.length,
    lagMs: Math.round(ringMs + queuedMs),
  });
}

async function init(sab: SharedArrayBuffer): Promise<void> {
  reader = AudioRingReader.fromSab(sab);
  segmenter = new EnergyVadSegmenter({}, (pcm, leadMs, gapMs) =>
    enqueueSegment(pcm, leadMs, gapMs),
  );
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
      srcRateHz = msg.rateHz;
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
