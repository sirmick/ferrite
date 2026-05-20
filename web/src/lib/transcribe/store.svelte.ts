// Transcript store — the single source of truth the advanced
// transcription panel renders.
//
// One entry per closed VAD segment. The transcription Worker posts
// segments here via `browserRuntime`; the panel reads `segments`
// reactively. Stamped with wall-clock + VFO frequency at capture so
// the transcript doubles as a band log ("set it and leave it").
//
// Kept deliberately dumb: no inference, no audio — just an append-only
// ring of results plus per-engine status for the panel's header.

/** One recognised token with its model probability. The panel dims /
 *  underlines low-`p` tokens so the operator's eye does the
 *  disambiguation — the right UX for noisy SSB review. */
export interface TranscriptToken {
  readonly text: string;
  /** Whisper token probability, 0..1. */
  readonly p: number;
}

export interface TranscriptSegment {
  readonly id: number;
  /** Wall-clock at capture (ms since epoch) — segment close time. */
  readonly atMs: number;
  /** VFO absolute frequency (Hz) at capture, or null when unknown. */
  readonly vfoHz: number | null;
  /** Segment start/end offsets within the captured audio (seconds). */
  readonly t0: number;
  readonly t1: number;
  /** Final, ham-post-processed text (phonetic→callsign etc. applied). */
  readonly text: string;
  /** Per-token text + probability for the confidence rendering. */
  readonly tokens: ReadonlyArray<TranscriptToken>;
  /** Segment-level confidence (avg token log-prob mapped to 0..1). */
  readonly confidence: number;
  /** Whisper's no-speech probability — high ⇒ likely VAD false-fire. */
  readonly noSpeechProb: number;
  /** Continues the previous segment with no speaker pause between
   *  (mid-utterance max-cut / later sub-segment of one clip). */
  readonly cont: boolean;
  /** Silence (ms) before this utterance; 0 mid-utterance. The rolling
   *  transcript breaks a paragraph when this exceeds the threshold. */
  readonly gapMs: number;
  /** tinydiarize speaker-turn flag — true when the model thinks the
   *  talker changed at this segment. Only meaningful on the
   *  `small.en-tdrz` model; false on others. */
  readonly speakerTurn?: boolean;
}

export type EngineStatus =
  | 'idle' // no VoiceTranscribe block / mode=off
  | 'loading-model' // fetching / decoding the ggml model
  | 'listening' // VAD armed, waiting for speech
  | 'transcribing' // a segment is in flight through whisper
  | 'unavailable' // whisper.cpp WASM not built (run pnpm wasm:build:whisper)
  | 'error';

const MAX_SEGMENTS = 2000;

class TranscriptStore {
  /** Append-only, capped. Newest last. */
  segments = $state<TranscriptSegment[]>([]);
  status = $state<EngineStatus>('idle');
  /** Free-text detail for `unavailable` / `error` / `loading-model`. */
  statusDetail = $state<string>('');
  /** Cumulative tap-ring samples dropped (worker fell behind). */
  droppedSamples = $state<number>(0);
  /** Model the worker is using, for the panel header. */
  modelName = $state<string>('');

  // Live behaviour for the gate/level meter + backlog readout. Updated
  // ~7×/s from the worker; the panel only renders these while armed.
  /** VAD gate open (an utterance is being captured). */
  gateOpen = $state<boolean>(false);
  /** Most-recent frame RMS (linear). */
  level = $state<number>(0);
  /** Adaptive open threshold the gate fires above (linear). */
  threshold = $state<number>(0);
  /** Utterances queued for whisper (whisper behind the speaker). */
  queued = $state<number>(0);
  /** Audio captured but not yet transcribed (ms): ring + queue. */
  lagMs = $state<number>(0);

  private nextId = 1;

  push(seg: Omit<TranscriptSegment, 'id'>): void {
    const withId: TranscriptSegment = { ...seg, id: this.nextId++ };
    // Reassign (not .push) so Svelte's $state proxy sees the change.
    const next =
      this.segments.length >= MAX_SEGMENTS
        ? [...this.segments.slice(this.segments.length - MAX_SEGMENTS + 1), withId]
        : [...this.segments, withId];
    this.segments = next;
  }

  setStatus(status: EngineStatus, detail = ''): void {
    this.status = status;
    this.statusDetail = detail;
  }

  setTelemetry(t: {
    gateOpen: boolean;
    level: number;
    threshold: number;
    queued: number;
    lagMs: number;
  }): void {
    this.gateOpen = t.gateOpen;
    this.level = t.level;
    this.threshold = t.threshold;
    this.queued = t.queued;
    this.lagMs = t.lagMs;
  }

  clear(): void {
    this.segments = [];
    this.nextId = 1;
  }

  reset(): void {
    this.clear();
    this.status = 'idle';
    this.statusDetail = '';
    this.droppedSamples = 0;
    this.modelName = '';
    this.gateOpen = false;
    this.level = 0;
    this.threshold = 0;
    this.queued = 0;
    this.lagMs = 0;
  }
}

export const transcript = new TranscriptStore();
