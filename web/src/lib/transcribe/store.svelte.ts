// Transcript store — the single source of truth the advanced
// transcription panel renders.
//
// One entry per closed VAD segment. Two feeds, one store, never both at
// once (placement is exclusive):
//   - **browser-placed** `VoiceTranscribe`: the in-browser Worker posts
//     segments here via `browserRuntime` (`push` / `setTelemetry`).
//   - **node-placed** (headless): the server block emits the same
//     entries on its `events` port → `ui:transcribe`; `attach` subscribes
//     this store to that JsonEvent stream (`ingestFrame`).
// The panel reads `segments` reactively regardless of which side ran.
// Stamped with wall-clock + VFO frequency at capture so the transcript
// doubles as a band log ("set it and leave it").
//
// Kept deliberately dumb: no inference, no audio — just an append-only
// ring of results plus per-engine status for the panel's header.

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';

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
  /** Raw whisper output before ham post-processing. Lets the panel
   *  toggle raw/clean and never lose the original on a bad cleanup.
   *  Empty string when unknown (older browser-worker frames omit it). */
  readonly rawText: string;
  /** Per-token text + probability for the confidence rendering. Empty
   *  on the node-placed path (the server entries carry no per-token
   *  probabilities — the panel falls back to plain `text`). */
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
  private unsubscribe?: () => void;
  private streamId?: number;

  push(seg: Omit<TranscriptSegment, 'id' | 'rawText'> & { rawText?: string }): void {
    const withId: TranscriptSegment = { rawText: '', ...seg, id: this.nextId++ };
    // Reassign (not .push) so Svelte's $state proxy sees the change.
    const next =
      this.segments.length >= MAX_SEGMENTS
        ? [...this.segments.slice(this.segments.length - MAX_SEGMENTS + 1), withId]
        : [...this.segments, withId];
    this.segments = next;
  }

  // ── Node-placed feed: subscribe to the block's `ui:transcribe`
  //    JsonEvent stream. `TranscriptView` calls attach when the pipeline
  //    advertises the sink + a FrameClient exists, detach on unmount.

  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    this.unsubscribe?.();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (frame) => this.ingestFrame(frame));
    // A live node-side feed means transcription is running server-side;
    // reflect that in the header (the browser worker never posts status
    // on this path).
    if (this.status === 'idle') this.setStatus('listening');
  }

  /** Stop receiving the node-side stream but KEEP accumulated segments
   *  (toggling the advanced view off must not lose the band log). */
  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.streamId = undefined;
  }

  /** One `ui:transcribe` frame → zero or more entries. Wire format is
   *  the camelCase `TranscriptEntry` the Rust `transcript.rs` serializes,
   *  newline-delimited (the ft8/rds idiom). */
  private ingestFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let obj: Record<string, unknown>;
      try {
        obj = JSON.parse(trimmed) as Record<string, unknown>;
      } catch {
        continue; // malformed line — drop, same as the RDS/FT8 stores
      }
      const clean = typeof obj.text === 'string' ? obj.text : '';
      if (!clean) continue;
      this.push({
        atMs: typeof obj.atMs === 'number' ? obj.atMs : Date.now(),
        vfoHz: typeof obj.vfoHz === 'number' ? obj.vfoHz : null,
        t0: typeof obj.t0 === 'number' ? obj.t0 : 0,
        t1: typeof obj.t1 === 'number' ? obj.t1 : 0,
        text: clean,
        rawText: typeof obj.rawText === 'string' ? obj.rawText : '',
        tokens: [], // node path carries no per-token probabilities
        confidence: typeof obj.confidence === 'number' ? obj.confidence : 0,
        noSpeechProb: typeof obj.noSpeechProb === 'number' ? obj.noSpeechProb : 0,
        cont: obj.cont === true,
        gapMs: typeof obj.gapMs === 'number' ? obj.gapMs : 0,
        speakerTurn: obj.speakerTurn === true,
      });
    }
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
