// Voice-activity segmenter.
//
// The real gate is Silero VAD, which ships *inside* the whisper.cpp
// WASM build (`--vad`, ggml Silero model). Silero is trained on
// speech-vs-noise, not loudness, so it survives the noisy/fading SSB
// this feature targets where a plain energy threshold collapses (noise
// floor ≈ speech level).
//
// This module is the **fallback** used only until the whisper.cpp WASM
// artifact exists: an adaptive energy gate with hangover. It's
// deliberately conservative (long hangover, noise-floor tracking) so a
// "leave it running" session still produces usable utterance-shaped
// chunks on a strong signal even before Silero is wired. Pure +
// unit-testable; the Worker swaps it for Silero when the engine
// reports it available.

export interface VadConfig {
  /** Input sample rate — always 16 kHz post-resample. */
  readonly rateHz: number;
  /** Speech must exceed (noiseFloor * ratio) to open a segment. */
  readonly openRatio: number;
  /** Trailing silence before a segment closes (ms). PTT/over gaps in
   *  ham voice are long — err generous so words aren't clipped. */
  readonly hangoverMs: number;
  /** Drop segments shorter than this (ms) — keys clicks, splatter. */
  readonly minSpeechMs: number;
  /** Hard cap so a stuck-open gate still flushes (ms). Doubles as the
   *  worst-case latency on *continuous* speech (no pauses → the gate
   *  only closes here): output appears every `maxSegmentMs`. Kept well
   *  under whisper.cpp's 30 s mel window, and short enough that one
   *  inference is quick — a long monologue would otherwise produce no
   *  text for ~30 s then one lagging giant chunk. */
  readonly maxSegmentMs: number;
  /** Audio (ms) carried from the end of a *max-cut* segment into the
   *  start of the next, so a word straddling the hard 10 s boundary
   *  still has lead-in context and decodes intact. Zero on
   *  silence-closed segments (those cut in a pause — no word to save).
   *  The worker drops the redundant re-decoded lead via whisper's
   *  per-segment timestamps. */
  readonly overlapMs: number;
}

export const DEFAULT_VAD: VadConfig = {
  rateHz: 16_000,
  openRatio: 3.0,
  hangoverMs: 700,
  minSpeechMs: 350,
  maxSegmentMs: 10_000,
  overlapMs: 750,
};

const FRAME = 320; // 20 ms @ 16 kHz — VAD analysis granularity

function rms(buf: Float32Array, from: number, to: number): number {
  let s = 0;
  for (let i = from; i < to; i++) s += buf[i] * buf[i];
  return Math.sqrt(s / Math.max(1, to - from));
}

/** Streaming segmenter. Feed resampled 16 kHz mono; `onSegment` fires
 *  with a contiguous utterance buffer each time the gate closes. */
export class EnergyVadSegmenter {
  private readonly cfg: VadConfig;
  private noiseFloor = 1e-4;
  private speaking = false;
  private silenceMs = 0;
  private acc: number[] = [];
  private heldFrame: number[] = [];
  /** RMS of the most recent frame — for the UI gate/level meter. */
  private lastLevel = 0;
  /** Overlap (ms) currently sitting at the *front* of `acc`, carried
   *  from the previous max-cut. Passed out so the worker can drop the
   *  redundant re-decoded lead. 0 unless a max-cut just happened. */
  private leadOverlapMs = 0;
  /** Silence accumulated since the last utterance ended (ms) — the
   *  pause that precedes the *next* utterance. */
  private idleMs = 0;
  /** The pause (ms) before the current utterance, emitted with its
   *  first chunk so the rolling transcript can break a paragraph on a
   *  sufficiently long silence. Consumed after the first emit so
   *  mid-utterance max-cut chunks report 0. */
  private utteranceGapMs = 0;

  /** Snapshot for the live gate/level meter: current frame RMS, the
   *  adaptive open threshold, and whether an utterance is being
   *  accumulated (gate open or in hangover). Pure read. */
  meterState(): { level: number; threshold: number; speaking: boolean } {
    return {
      level: this.lastLevel,
      threshold: this.noiseFloor * this.cfg.openRatio,
      speaking: this.speaking,
    };
  }

  constructor(
    cfg: Partial<VadConfig>,
    /** `leadOverlapMs` = ms at the start of `pcm16k` carried from the
     *  previous max-cut (0 for silence-closed / first segments).
     *  `gapMs` = the silence that preceded this utterance (0 for
     *  mid-utterance max-cut continuations). */
    private readonly onSegment: (
      pcm16k: Float32Array,
      leadOverlapMs: number,
      gapMs: number,
    ) => void,
  ) {
    this.cfg = { ...DEFAULT_VAD, ...cfg };
  }

  feed(chunk: Float32Array): void {
    // Reframe to fixed 20 ms windows, carrying a partial across calls.
    const data =
      this.heldFrame.length > 0 ? Float32Array.from([...this.heldFrame, ...chunk]) : chunk;
    this.heldFrame = [];
    let off = 0;
    for (; off + FRAME <= data.length; off += FRAME) {
      this.processFrame(data, off);
    }
    if (off < data.length) this.heldFrame = Array.from(data.subarray(off));
  }

  private processFrame(data: Float32Array, off: number): void {
    const level = rms(data, off, off + FRAME);
    this.lastLevel = level;
    const frameMs = (FRAME / this.cfg.rateHz) * 1000;
    const open = level > this.noiseFloor * this.cfg.openRatio;

    if (open) {
      if (!this.speaking) {
        this.speaking = true;
        // New utterance — the accumulated idle is the pause before it.
        this.utteranceGapMs = this.idleMs;
        this.idleMs = 0;
      }
      this.silenceMs = 0;
      for (let i = off; i < off + FRAME; i++) this.acc.push(data[i]);
    } else if (this.speaking) {
      // Keep accumulating through the hangover so trailing consonants
      // and short inter-word gaps stay in the same segment. Do NOT
      // adapt the noise floor here: the hangover is soft speech /
      // inter-word dips, not silence — adapting toward it creeps the
      // floor up and the gate then misses the next *soft* passage
      // (a dropped chunk = a big gap).
      for (let i = off; i < off + FRAME; i++) this.acc.push(data[i]);
      this.silenceMs += frameMs;
      // Silence close — it cut in a pause, no straddling word to save.
      if (this.silenceMs >= this.cfg.hangoverMs) this.flush(false);
    } else {
      // True idle: no utterance in progress and below threshold —
      // genuine noise. The only safe place to learn the floor, and
      // where we measure the inter-utterance pause.
      this.noiseFloor = this.noiseFloor * 0.97 + level * 0.03;
      this.idleMs += frameMs;
    }

    // Hard cap on continuous speech — cut mid-word, so carry a tail.
    if (this.acc.length / this.cfg.rateHz >= this.cfg.maxSegmentMs / 1000) {
      this.flush(true);
    }
  }

  private flush(maxCut: boolean): void {
    const ms = (this.acc.length / this.cfg.rateHz) * 1000;
    const pcm = Float32Array.from(this.acc);
    const emittedLeadMs = this.leadOverlapMs;
    // The pause before this utterance — only its first emitted chunk
    // carries it; consume so a max-cut continuation reports 0.
    const emittedGapMs = this.utteranceGapMs;
    this.utteranceGapMs = 0;

    if (maxCut) {
      // Still mid-utterance: keep talking, and seed the next segment
      // with the last `overlapMs` so a word on the boundary has lead-in
      // context (the worker drops the redundant re-decode by timestamp).
      const tailN = Math.min(
        this.acc.length,
        Math.round((this.cfg.overlapMs / 1000) * this.cfg.rateHz),
      );
      this.acc = this.acc.slice(this.acc.length - tailN);
      this.leadOverlapMs = (this.acc.length / this.cfg.rateHz) * 1000;
      this.silenceMs = 0;
      // `speaking` stays true — we never stopped.
    } else {
      this.acc = [];
      this.leadOverlapMs = 0;
      this.speaking = false;
      this.silenceMs = 0;
      // The hangover that closed this is real dead air the operator
      // paused for — count it toward the next utterance's gap so the
      // paragraph break reflects the perceived pause.
      this.idleMs = this.cfg.hangoverMs;
    }

    if (ms >= this.cfg.minSpeechMs) this.onSegment(pcm, emittedLeadMs, emittedGapMs);
  }

  reset(): void {
    this.speaking = false;
    this.silenceMs = 0;
    this.acc = [];
    this.heldFrame = [];
    this.noiseFloor = 1e-4;
    this.lastLevel = 0;
    this.leadOverlapMs = 0;
    this.idleMs = 0;
    this.utteranceGapMs = 0;
  }
}
