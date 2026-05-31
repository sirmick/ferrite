// Transcription Worker — one per VoiceTranscribe block.
//
// Drains the block's tap SAB on its own coarse cadence (never the audio
// clock) and runs closed utterances through whisper.cpp. Inference is
// heavy and fully off-thread here, so it can never stall audio or the
// UI. Posts results back to the main thread, where browserRuntime
// forwards them to the transcript store.
//
// The VAD, ham cleanup, resampling, and queue/backlog bookkeeping all
// live in the shared Rust core (`ferrite-whisper`, compiled to wasm via
// `WasmTranscriber`) — the *same* code the node-side block runs, so both
// sides segment and clean identically. The only step that stays
// JavaScript is the whisper.cpp inference call itself (`whisperEngine`,
// an Emscripten module): the core hands out a 16 kHz clip, JS runs
// whisper on it, and hands the segments back for cleanup + recording.
//
// Graceful degradation: if the whisper.cpp WASM artifact isn't built the
// Worker still runs (draining the ring so the glitch counter is honest)
// but reports `unavailable` and emits no segments — the VoiceTranscribe
// block is pure passthrough so audio is unaffected.

import initBlocksWasm, { WasmTranscriber } from '../wasm/blocks/ferrite_blocks';
import { AudioRingReader } from '../audio/ringBuffer';
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
  | { type: 'dropped'; total: number; shed: number }
  | {
      // Per-utterance instrumentation → backend log (one line per
      // segment, not the 150 ms telemetry). Lets us spot whisper falling
      // behind / shedding from `/tmp/ferrite-run.log`.
      type: 'stat';
      segS: number;
      inferMs: number;
      queued: number;
    }
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
      /** Raw whisper output before ham post-processing — the store keeps
       *  it so the panel can toggle raw/clean. */
      rawText: string;
      tokens: { text: string; p: number }[];
      confidence: number;
      noSpeechProb: number;
      cont: boolean;
      gapMs: number;
      speakerTurn?: boolean;
    };

/** The camelCase `TranscriptEntry` shape `WasmTranscriber.drainNew()`
 *  returns — mirrors the Rust `transcript::TranscriptEntry` serialize. */
interface CoreEntry {
  id: number;
  atMs: number;
  vfoHz: number | null;
  t0: number;
  t1: number;
  rawText: string;
  text: string;
  confidence: number;
  noSpeechProb: number;
  cont: boolean;
  gapMs: number;
  speakerTurn: boolean;
}

const POLL_MS = 150;

let wasmReady = false;
let reader: AudioRingReader | undefined;
let core: WasmTranscriber | undefined;
let engine: WhisperEngine | undefined;
let timer: ReturnType<typeof setInterval> | undefined;
let srcRateHz = 0;
let curModel: WhisperModelDef = MODELS[0];
/** Operator prompt base, stashed until the core exists (the `prompt`
 *  message can arrive before `rate`). Empty = built-in ham corpus. */
let promptBase = DEFAULT_HAM_PROMPT;
/** Re-entrancy guard: `drainPending` calls `poll` between inferences to
 *  keep the SAB from overflowing, and the timer fires both. */
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
    // VAD is always the Rust energy gate now (we segment before the
    // whisper call); whisper.cpp's bundled Silero is never the gate.
    vad: 'energy',
  });
}

/** Build the core once we know both the wasm module is ready and the
 *  negotiated source rate. Idempotent. Applies any stashed prompt base. */
function maybeCreateCore(): void {
  if (core || !wasmReady || srcRateHz <= 0) return;
  core = new WasmTranscriber(srcRateHz);
  core.setPromptBase(promptBase === DEFAULT_HAM_PROMPT ? '' : promptBase);
}

/** Drain the SAB into the core (resample + VAD + enqueue) and post the
 *  live meter/backlog telemetry. Never blocks on inference. */
function poll(): void {
  // Don't touch the ring until the core exists. The `rate` message
  // arrives a few runner ticks after `init`, so draining pre-rate would
  // consume and discard the first ~hundreds of ms of speech. The SAB
  // ring is large (≈ seconds), so buffering until then loses nothing.
  if (!reader || !core) return;
  let n = reader.read(scratch);
  while (n > 0) {
    const shed = core.feed(scratch.subarray(0, n));
    if (shed > 0) {
      // Whisper is behind the speaker — the core shed the oldest
      // utterance(s) so latency stays bounded. Loud, greppable line.
      post({ type: 'dropped', total: Number(core.droppedTotal()), shed });
    }
    if (n < scratch.length) break;
    n = reader.read(scratch);
  }

  const m = core.meter() as { level: number; threshold: number; speaking: boolean };
  const ringMs = srcRateHz > 0 ? (reader.availableRead() / srcRateHz) * 1000 : 0;
  post({
    type: 'telemetry',
    gateOpen: m.speaking,
    level: m.level,
    threshold: m.threshold,
    queued: core.pendingLen(),
    lagMs: Math.round(ringMs + core.queuedMs()),
  });
}

/** Run whisper on every closed utterance the core has queued, recording
 *  each result back into the core and posting the cleaned entries. The
 *  whisper call is the one blocking step; everything around it is the
 *  shared Rust core. */
function drainPending(): void {
  if (draining) return;
  draining = true;
  try {
    if (!core || !engine?.loaded) return;
    let pcm = core.takePending();
    while (pcm) {
      status('transcribing');
      const t0 = performance.now();
      try {
        const res = engine.transcribe(pcm, core.rollingPrompt());
        post({
          type: 'stat',
          segS: pcm.length / 16_000,
          inferMs: Math.round(performance.now() - t0),
          queued: core.pendingLen(),
        });
        // Hand the segments back: the core applies the max-cut lead
        // drop, ham cleanup, cont/gap, wall-clock stamping, and callsign
        // bias — identically to the node side.
        core.ingestInflight(JSON.stringify(res.segments), Date.now());
        for (const e of core.drainNew() as CoreEntry[]) {
          post({
            type: 'segment',
            atMs: e.atMs,
            t0: e.t0,
            t1: e.t1,
            text: e.text,
            rawText: e.rawText,
            tokens: [], // per-token probabilities aren't carried by the core
            confidence: e.confidence,
            noSpeechProb: e.noSpeechProb,
            cont: e.cont,
            gapMs: e.gapMs,
            speakerTurn: e.speakerTurn,
          });
        }
      } catch (err) {
        status('error', String(err));
      }
      // Drain the SAB between queued inferences so a backlog doesn't
      // overflow the ring while we catch up.
      poll();
      pcm = core.takePending();
    }
    status('listening');
  } finally {
    draining = false;
  }
}

function tick(): void {
  poll();
  drainPending();
}

async function init(sab: SharedArrayBuffer): Promise<void> {
  reader = AudioRingReader.fromSab(sab);
  timer = setInterval(tick, POLL_MS);

  // The shared Rust core (VAD/cleanup/queue). Cheap; always available.
  await initBlocksWasm();
  wasmReady = true;
  maybeCreateCore();

  // The whisper.cpp inference engine (Emscripten). Heavy; may be absent.
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
      maybeCreateCore();
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
      core?.setPromptBase(promptBase === DEFAULT_HAM_PROMPT ? '' : promptBase);
      break;
    case 'stop':
      if (timer) clearInterval(timer);
      timer = undefined;
      reader = undefined;
      core = undefined;
      break;
  }
};
