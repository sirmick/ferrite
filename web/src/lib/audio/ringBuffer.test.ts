import { describe, expect, it } from 'vitest';

import { AUDIO_RING_HEADER_BYTES, AudioRingWriter, audioRingSabBytes } from './ringBuffer.js';

function floatArr(...xs: number[]): Float32Array {
  return Float32Array.from(xs);
}

describe('audioRingSabBytes', () => {
  it('sizes SAB for header + capacity * 4 bytes', () => {
    expect(audioRingSabBytes(1024)).toBe(AUDIO_RING_HEADER_BYTES + 1024 * 4);
    expect(audioRingSabBytes(16)).toBe(AUDIO_RING_HEADER_BYTES + 64);
  });

  it('rejects non-power-of-two capacities', () => {
    expect(() => audioRingSabBytes(1000)).toThrow(/power of two/);
    expect(() => audioRingSabBytes(0)).toThrow(/power of two/);
    expect(() => audioRingSabBytes(-4)).toThrow(/power of two/);
  });
});

describe('AudioRingWriter.create', () => {
  it('builds a writer starting empty', () => {
    const w = AudioRingWriter.create(16);
    expect(w.capacity).toBe(16);
    expect(w.availableRead()).toBe(0);
    expect(w.availableWrite()).toBe(16);
    expect(w.sab.byteLength).toBe(audioRingSabBytes(16));
  });

  it('rejects non-power-of-two capacity', () => {
    expect(() => AudioRingWriter.create(100)).toThrow(/power of two/);
  });
});

describe('AudioRingWriter.fromSab', () => {
  it('wraps an SAB allocated elsewhere with the matching capacity', () => {
    const original = AudioRingWriter.create(64);
    const wrapped = AudioRingWriter.fromSab(original.sab);
    expect(wrapped.capacity).toBe(64);
    expect(wrapped.sab).toBe(original.sab);
  });

  it('rejects SABs that do not describe a power-of-two ring', () => {
    const sab = new SharedArrayBuffer(AUDIO_RING_HEADER_BYTES + 100 * 4);
    expect(() => AudioRingWriter.fromSab(sab)).toThrow(/power of two/);
  });

  it('rejects SABs with misaligned bodies', () => {
    const sab = new SharedArrayBuffer(AUDIO_RING_HEADER_BYTES + 3);
    expect(() => AudioRingWriter.fromSab(sab)).toThrow(/f32-aligned/);
  });

  it('shares state between two wrappers over one SAB', () => {
    // Not a supported runtime pattern (SPSC means one producer only),
    // but the test pins the invariant that fromSab is a pure mapping.
    const a = AudioRingWriter.create(8);
    const b = AudioRingWriter.fromSab(a.sab);
    a.write(floatArr(1, 2, 3));
    expect(b.availableRead()).toBe(3);
  });
});

describe('AudioRingWriter.write', () => {
  it('writes all samples when the ring has room', () => {
    const w = AudioRingWriter.create(8);
    const n = w.write(floatArr(1, 2, 3, 4));
    expect(n).toBe(4);
    expect(w.availableRead()).toBe(4);
    expect(w.availableWrite()).toBe(4);
  });

  it('returns 0 when the ring is full', () => {
    const w = AudioRingWriter.create(4);
    w.write(floatArr(1, 2, 3, 4));
    expect(w.availableWrite()).toBe(0);
    expect(w.write(floatArr(5))).toBe(0);
  });

  it('writes a partial count when near-full', () => {
    const w = AudioRingWriter.create(4);
    w.write(floatArr(1, 2, 3));
    const n = w.write(floatArr(10, 20, 30));
    expect(n).toBe(1); // only 1 slot free
    expect(w.availableRead()).toBe(4);
  });

  it('wraps around the ring body', () => {
    const w = AudioRingWriter.create(4);
    w.write(floatArr(1, 2, 3));
    w._testAdvanceTail(3); // consumer drained
    // head=3, tail=3, capacity=4 → 4 free, write pos wraps
    const n = w.write(floatArr(10, 20, 30, 40));
    expect(n).toBe(4);
    // Verify layout: slots [3,0,1,2] hold [10,20,30,40]
    const body = new Float32Array(w.sab, AUDIO_RING_HEADER_BYTES, w.capacity);
    expect(Array.from(body)).toEqual([20, 30, 40, 10]);
  });

  it('never overwrites unread samples', () => {
    const w = AudioRingWriter.create(4);
    w.write(floatArr(1, 2, 3, 4)); // full
    // Attempt to write 10 more — should reject all.
    const n = w.write(floatArr(10, 20, 30, 40, 50, 60, 70, 80, 90, 100));
    expect(n).toBe(0);
    const body = new Float32Array(w.sab, AUDIO_RING_HEADER_BYTES, w.capacity);
    expect(Array.from(body)).toEqual([1, 2, 3, 4]);
  });

  it('handles head/tail wrap past 2^32 boundary', () => {
    const w = AudioRingWriter.create(4);
    // Jam head/tail just shy of u32 wrap; count = 0.
    const header = new Uint32Array(w.sab, 0, 2);
    header[0] = 0xfffffffe;
    header[1] = 0xfffffffe;
    expect(w.availableWrite()).toBe(4);
    const n = w.write(floatArr(1, 2, 3, 4));
    expect(n).toBe(4);
    // head is (0xfffffffe + 4) mod 2^32 = 2
    expect(Atomics.load(header, 0)).toBe(2);
    expect(w.availableRead()).toBe(4);
  });

  it('empty Float32Array is a no-op', () => {
    const w = AudioRingWriter.create(4);
    expect(w.write(new Float32Array(0))).toBe(0);
    expect(w.availableRead()).toBe(0);
  });
});

describe('AudioRingWriter.reset', () => {
  it('clears head and tail back to zero', () => {
    const w = AudioRingWriter.create(4);
    w.write(floatArr(1, 2, 3));
    w._testAdvanceTail(1);
    w.reset();
    expect(w.availableRead()).toBe(0);
    expect(w.availableWrite()).toBe(4);
  });
});
