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
  /** Hard cap so a stuck-open gate still flushes (ms). */
  readonly maxSegmentMs: number;
}

export const DEFAULT_VAD: VadConfig = {
  rateHz: 16_000,
  openRatio: 3.0,
  hangoverMs: 700,
  minSpeechMs: 350,
  maxSegmentMs: 28_000,
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

  constructor(
    cfg: Partial<VadConfig>,
    private readonly onSegment: (pcm16k: Float32Array) => void,
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
    const frameMs = (FRAME / this.cfg.rateHz) * 1000;
    const open = level > this.noiseFloor * this.cfg.openRatio;

    if (open) {
      if (!this.speaking) this.speaking = true;
      this.silenceMs = 0;
      for (let i = off; i < off + FRAME; i++) this.acc.push(data[i]);
    } else if (this.speaking) {
      // Keep accumulating through the hangover so trailing consonants
      // and short inter-word gaps stay in the same segment.
      for (let i = off; i < off + FRAME; i++) this.acc.push(data[i]);
      this.silenceMs += frameMs;
      if (this.silenceMs >= this.cfg.hangoverMs) this.flush();
    } else {
      // Silence while idle — adapt the noise-floor estimate slowly.
      this.noiseFloor = this.noiseFloor * 0.97 + level * 0.03;
    }

    if (this.acc.length / this.cfg.rateHz >= this.cfg.maxSegmentMs / 1000) {
      this.flush();
    }
  }

  private flush(): void {
    const ms = (this.acc.length / this.cfg.rateHz) * 1000;
    const pcm = Float32Array.from(this.acc);
    this.acc = [];
    this.speaking = false;
    this.silenceMs = 0;
    if (ms >= this.cfg.minSpeechMs) this.onSegment(pcm);
  }

  reset(): void {
    this.speaking = false;
    this.silenceMs = 0;
    this.acc = [];
    this.heldFrame = [];
    this.noiseFloor = 1e-4;
  }
}
