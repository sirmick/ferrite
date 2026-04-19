// Single-producer / single-consumer audio ring buffer backed by a
// SharedArrayBuffer. The producer lives in the flowgraph runner Worker;
// the consumer is an AudioWorklet on the audio thread. Neither side may
// block on the other — underruns are tolerated (AudioWorklet will emit
// silence) and overruns drop samples on the producer side so that the
// audio clock never stalls.
//
// This commit lands the **producer** half. The AudioWorklet consumer
// (commit #82) reads the same SAB layout through a matching `Reader`.
//
// Layout:
//
//   offset 0   ┌─ Uint32 header ────────────────────────────┐
//              │ [0] head       — monotonic write index      │
//              │ [1] tail       — monotonic read index       │
//              │ [2..15] reserved (kept zeroed, future use)  │
//              └─────────────────────────────────────────────┘
//   offset 64  ┌─ Float32 ring body, `capacity` samples ────┐
//              └─────────────────────────────────────────────┘
//
// `head` and `tail` are stored modulo 2^32 and the producer uses
// `(head - tail) >>> 0` to recover the unsigned number of samples in
// flight. This tolerates either counter wrapping past 2^32 as long as
// `capacity < 2^31`, which is orders of magnitude beyond what any real
// audio buffer needs.
//
// Why 64-byte header rather than 8: keeps the `head`/`tail` cells on
// separate 64-byte cache lines from the ring body (avoids false
// sharing with the consumer when it scans from the front of the body).
// Cheap insurance — 56 bytes of address space.
//
// Cross-origin isolation (COOP/COEP) is required for `SharedArrayBuffer`
// to be constructible; see `docs/01-architecture.md`. Forgetting the
// headers turns the constructor into a TypeError here.

const HEAD_INDEX = 0;
const TAIL_INDEX = 1;
export const AUDIO_RING_HEADER_BYTES = 64;
const HEADER_U32_WORDS = AUDIO_RING_HEADER_BYTES / 4;

function isPow2(n: number): boolean {
  return Number.isInteger(n) && n > 0 && (n & (n - 1)) === 0;
}

/**
 * Total byte length of a ring buffer that holds `capacity` f32 samples.
 * Exposed so a producer can allocate an appropriately-sized SAB before
 * transferring it to another thread.
 */
export function audioRingSabBytes(capacity: number): number {
  if (!isPow2(capacity)) {
    throw new RangeError(`audioRingSabBytes: capacity must be a power of two, got ${capacity}`);
  }
  return AUDIO_RING_HEADER_BYTES + capacity * Float32Array.BYTES_PER_ELEMENT;
}

/**
 * Producer-side handle on an SPSC audio ring. All writes are non-blocking:
 * if the ring is full, `write` returns a short count (including 0) and
 * the caller decides whether to drop samples or retry.
 *
 * One instance owns the `head` index; never construct two writers over
 * the same SAB.
 */
export class AudioRingWriter {
  readonly sab: SharedArrayBuffer;
  readonly capacity: number;
  private readonly mask: number;
  private readonly header: Uint32Array;
  private readonly data: Float32Array;

  private constructor(sab: SharedArrayBuffer, capacity: number) {
    this.sab = sab;
    this.capacity = capacity;
    this.mask = capacity - 1;
    this.header = new Uint32Array(sab, 0, HEADER_U32_WORDS);
    this.data = new Float32Array(sab, AUDIO_RING_HEADER_BYTES, capacity);
  }

  /** Allocate a fresh SAB sized for `capacity` f32 samples. */
  static create(capacity: number): AudioRingWriter {
    const sab = new SharedArrayBuffer(audioRingSabBytes(capacity));
    return new AudioRingWriter(sab, capacity);
  }

  /**
   * Wrap an existing SAB. Used by the flowgraph runner Worker to take
   * over an SAB that was allocated on the main thread and handed over
   * alongside the `AudioWorklet` port.
   */
  static fromSab(sab: SharedArrayBuffer): AudioRingWriter {
    const bodyBytes = sab.byteLength - AUDIO_RING_HEADER_BYTES;
    if (bodyBytes <= 0 || bodyBytes % Float32Array.BYTES_PER_ELEMENT !== 0) {
      throw new RangeError(
        `AudioRingWriter.fromSab: SAB body is not f32-aligned (body bytes ${bodyBytes})`,
      );
    }
    const capacity = bodyBytes / Float32Array.BYTES_PER_ELEMENT;
    if (!isPow2(capacity)) {
      throw new RangeError(
        `AudioRingWriter.fromSab: inferred capacity ${capacity} is not a power of two`,
      );
    }
    return new AudioRingWriter(sab, capacity);
  }

  /** Samples currently occupied (written but not yet read). */
  availableRead(): number {
    const head = Atomics.load(this.header, HEAD_INDEX);
    const tail = Atomics.load(this.header, TAIL_INDEX);
    return (head - tail) >>> 0;
  }

  /** Samples the producer can write before overrunning the consumer. */
  availableWrite(): number {
    return this.capacity - this.availableRead();
  }

  /**
   * Write up to `samples.length` f32 samples; returns the actual count
   * accepted. Partial writes happen when the ring is near-full. Never
   * overwrites unread samples — the caller decides whether to drop,
   * backoff, or pre-downsample when the count is short.
   */
  write(samples: Float32Array): number {
    const head = Atomics.load(this.header, HEAD_INDEX);
    const tail = Atomics.load(this.header, TAIL_INDEX);
    const used = (head - tail) >>> 0;
    const free = this.capacity - used;
    if (free === 0) return 0;
    const toWrite = Math.min(free, samples.length);
    const writePos = head & this.mask;
    const firstSpan = Math.min(toWrite, this.capacity - writePos);
    this.data.set(samples.subarray(0, firstSpan), writePos);
    if (toWrite > firstSpan) {
      this.data.set(samples.subarray(firstSpan, toWrite), 0);
    }
    // Sequentially-consistent store acts as a release — every sample
    // write above is visible to the consumer before it sees the bumped
    // head.
    Atomics.store(this.header, HEAD_INDEX, (head + toWrite) >>> 0);
    return toWrite;
  }

  /**
   * Clear the ring. Only safe to call when the consumer is stopped —
   * concurrent `reset` + consumer reads race on `tail`. Intended for
   * use before the AudioWorklet is wired up or after it's torn down.
   */
  reset(): void {
    Atomics.store(this.header, HEAD_INDEX, 0);
    Atomics.store(this.header, TAIL_INDEX, 0);
  }
}

/**
 * Consumer-side handle on an SPSC audio ring. Owned by the AudioWorklet
 * thread. Like the writer, all reads are non-blocking — when the ring
 * is empty, `read` returns a short count (including 0) and the caller
 * fills the rest of its output with silence.
 */
export class AudioRingReader {
  readonly sab: SharedArrayBuffer;
  readonly capacity: number;
  private readonly mask: number;
  private readonly header: Uint32Array;
  private readonly data: Float32Array;

  private constructor(sab: SharedArrayBuffer, capacity: number) {
    this.sab = sab;
    this.capacity = capacity;
    this.mask = capacity - 1;
    this.header = new Uint32Array(sab, 0, HEADER_U32_WORDS);
    this.data = new Float32Array(sab, AUDIO_RING_HEADER_BYTES, capacity);
  }

  /**
   * Wrap an SAB. The AudioWorklet receives the SAB through
   * `processorOptions` and constructs its reader here.
   */
  static fromSab(sab: SharedArrayBuffer): AudioRingReader {
    const bodyBytes = sab.byteLength - AUDIO_RING_HEADER_BYTES;
    if (bodyBytes <= 0 || bodyBytes % Float32Array.BYTES_PER_ELEMENT !== 0) {
      throw new RangeError(
        `AudioRingReader.fromSab: SAB body is not f32-aligned (body bytes ${bodyBytes})`,
      );
    }
    const capacity = bodyBytes / Float32Array.BYTES_PER_ELEMENT;
    if (!isPow2(capacity)) {
      throw new RangeError(
        `AudioRingReader.fromSab: inferred capacity ${capacity} is not a power of two`,
      );
    }
    return new AudioRingReader(sab, capacity);
  }

  /** Samples currently available to read. */
  availableRead(): number {
    // Acquire-load on head pairs with the producer's release-store so
    // every sample the producer wrote before bumping head is visible
    // here.
    const head = Atomics.load(this.header, HEAD_INDEX);
    const tail = Atomics.load(this.header, TAIL_INDEX);
    return (head - tail) >>> 0;
  }

  /**
   * Read up to `out.length` samples into `out`; returns the actual count
   * copied. Partial reads happen when the ring is near-empty; the
   * caller fills the remainder of `out` with silence.
   */
  read(out: Float32Array): number {
    const head = Atomics.load(this.header, HEAD_INDEX);
    const tail = Atomics.load(this.header, TAIL_INDEX);
    const avail = (head - tail) >>> 0;
    if (avail === 0) return 0;
    const toRead = Math.min(avail, out.length);
    const readPos = tail & this.mask;
    const firstSpan = Math.min(toRead, this.capacity - readPos);
    out.set(this.data.subarray(readPos, readPos + firstSpan), 0);
    if (toRead > firstSpan) {
      out.set(this.data.subarray(0, toRead - firstSpan), firstSpan);
    }
    Atomics.store(this.header, TAIL_INDEX, (tail + toRead) >>> 0);
    return toRead;
  }
}
