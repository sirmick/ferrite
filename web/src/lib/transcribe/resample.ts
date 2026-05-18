// Linear resampler — source audio rate → whisper's 16 kHz mono.
//
// Whisper wants 16 kHz f32 mono. The demod chain hands us whatever the
// preset negotiated (12 k SSB, 48 k FM, …). A streaming linear
// resampler is plenty for speech intelligibility — whisper is robust to
// the mild aliasing, and accuracy-vs-fidelity here is dominated by the
// model, not the interpolation kernel. Stateful so it can be fed
// arbitrary ring-drained chunks without clicks at the seams.

export const WHISPER_RATE = 16_000;

export class LinearResampler {
  /** Fractional read position into the *input* stream, carried across
   *  feed() calls so chunk boundaries don't glitch. */
  private pos = 0;
  /** Last sample of the previous chunk — the left neighbour for the
   *  first interpolation of the next chunk. */
  private prev = 0;
  private primed = false;

  constructor(private srcRate: number) {}

  /** True when the source already is 16 kHz (passthrough). */
  get isIdentity(): boolean {
    return this.srcRate === WHISPER_RATE;
  }

  /** Resample one chunk. Returns a freshly-allocated 16 kHz buffer. */
  feed(input: Float32Array): Float32Array {
    if (input.length === 0) return new Float32Array(0);
    if (this.isIdentity) return input.slice();

    const step = this.srcRate / WHISPER_RATE; // input samples per output sample
    const out: number[] = [];
    if (!this.primed) {
      this.prev = input[0];
      this.primed = true;
    }
    // `pos` is an absolute index into a virtual stream whose sample -1
    // is `this.prev` and 0..n-1 is `input`. Emit until we'd need a
    // sample past the end of this chunk.
    while (this.pos < input.length - 1) {
      const i = Math.floor(this.pos);
      const frac = this.pos - i;
      const a = i < 0 ? this.prev : input[i];
      const b = input[i + 1];
      out.push(a + (b - a) * frac);
      this.pos += step;
    }
    // Carry the fractional remainder into the next chunk's coordinate
    // space and stash the trailing sample as the next left-neighbour.
    this.pos -= input.length;
    this.prev = input[input.length - 1];
    return Float32Array.from(out);
  }

  reset(): void {
    this.pos = 0;
    this.prev = 0;
    this.primed = false;
  }
}
