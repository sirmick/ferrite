//! SPSC audio ring buffer — Rust port of `web/src/lib/audio/ringBuffer.ts`.
//!
//! Producer-writer / consumer-reader semantics, both sides non-blocking:
//! overruns drop samples (audio clock wins over flowgraph clock) and
//! underruns read short. `head` and `tail` are stored modulo 2^32, same
//! wrap-safe arithmetic as the TS side so a future SAB-backed variant
//! (the browser AudioWorklet bridge) can reuse the algorithm unchanged.
//!
//! The storage here is a plain `Vec<f32>` driven through `&mut self` —
//! useful today for AudioSink's internal accumulator and for round-trip
//! unit tests. The M4 commit that lands the real Web Audio glue will
//! introduce a second backend over `js_sys::SharedArrayBuffer` that
//! uses JS-side atomics for the cross-thread fences. Both types will
//! pass the same algorithmic test suite.

/// Single-owner SPSC ring. In test flow the same caller plays both
/// producer and consumer by interleaving `write` and `read` calls.
pub struct AudioRing {
    capacity: usize,
    mask: u32,
    head: u32,
    tail: u32,
    data: Vec<f32>,
}

impl AudioRing {
    /// Allocate a fresh ring of `capacity` f32 samples. Panics if
    /// `capacity` is not a power of two — the mask arithmetic relies on
    /// it, and callers pick capacities from a small fixed set anyway.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0 && capacity.is_power_of_two(),
            "AudioRing capacity must be a power of two, got {capacity}",
        );
        assert!(
            capacity <= (u32::MAX as usize) / 2,
            "AudioRing capacity {capacity} exceeds half of u32::MAX — wrap-safe arithmetic would fail",
        );
        Self {
            capacity,
            mask: (capacity - 1) as u32,
            head: 0,
            tail: 0,
            data: vec![0.0; capacity],
        }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Samples currently written but not yet read.
    #[must_use]
    pub fn available_read(&self) -> usize {
        self.head.wrapping_sub(self.tail) as usize
    }

    /// Samples the producer can write before overrunning the consumer.
    #[must_use]
    pub fn available_write(&self) -> usize {
        self.capacity - self.available_read()
    }

    /// Write up to `samples.len()` samples; returns the actual count
    /// accepted. Partial writes happen when the ring is near-full;
    /// never overwrites unread samples.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        let used = self.head.wrapping_sub(self.tail) as usize;
        let free = self.capacity - used;
        if free == 0 {
            return 0;
        }
        let to_write = free.min(samples.len());
        let write_pos = (self.head & self.mask) as usize;
        let first_span = to_write.min(self.capacity - write_pos);
        self.data[write_pos..write_pos + first_span].copy_from_slice(&samples[..first_span]);
        if to_write > first_span {
            self.data[..to_write - first_span].copy_from_slice(&samples[first_span..to_write]);
        }
        self.head = self.head.wrapping_add(to_write as u32);
        to_write
    }

    /// Read up to `out.len()` samples into `out`; returns the actual
    /// count copied. Consumer side of the SPSC contract.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let avail = self.head.wrapping_sub(self.tail) as usize;
        if avail == 0 {
            return 0;
        }
        let to_read = avail.min(out.len());
        let read_pos = (self.tail & self.mask) as usize;
        let first_span = to_read.min(self.capacity - read_pos);
        out[..first_span].copy_from_slice(&self.data[read_pos..read_pos + first_span]);
        if to_read > first_span {
            out[first_span..to_read].copy_from_slice(&self.data[..to_read - first_span]);
        }
        self.tail = self.tail.wrapping_add(to_read as u32);
        to_read
    }

    /// Clear the ring. Intended for pre-start and post-stop, mirroring
    /// the TS `reset()`.
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::AudioRing;

    #[test]
    fn new_starts_empty() {
        let r = AudioRing::new(16);
        assert_eq!(r.capacity(), 16);
        assert_eq!(r.available_read(), 0);
        assert_eq!(r.available_write(), 16);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn new_rejects_non_pow2() {
        let _ = AudioRing::new(100);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn new_rejects_zero() {
        let _ = AudioRing::new(0);
    }

    #[test]
    fn write_then_read_round_trips_samples() {
        let mut r = AudioRing::new(8);
        let n = r.write(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(n, 4);
        assert_eq!(r.available_read(), 4);
        let mut out = [0.0; 4];
        let got = r.read(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(r.available_read(), 0);
    }

    #[test]
    fn write_returns_zero_when_full() {
        let mut r = AudioRing::new(4);
        assert_eq!(r.write(&[1.0, 2.0, 3.0, 4.0]), 4);
        assert_eq!(r.available_write(), 0);
        assert_eq!(r.write(&[5.0]), 0);
    }

    #[test]
    fn write_partial_when_nearly_full() {
        let mut r = AudioRing::new(4);
        r.write(&[1.0, 2.0]);
        let n = r.write(&[3.0, 4.0, 5.0, 6.0]);
        assert_eq!(n, 2, "only 2 free slots remain");
        let mut out = [0.0; 4];
        let got = r.read(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn wraps_around_cleanly() {
        let mut r = AudioRing::new(4);
        // Fill, drain, fill again — the second fill wraps the write
        // cursor across the index-0 boundary.
        r.write(&[1.0, 2.0, 3.0, 4.0]);
        let mut drain = [0.0; 4];
        r.read(&mut drain);
        let n = r.write(&[5.0, 6.0, 7.0, 8.0]);
        assert_eq!(n, 4);
        let mut out = [0.0; 4];
        r.read(&mut out);
        assert_eq!(out, [5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn split_span_write_crosses_index_zero() {
        let mut r = AudioRing::new(4);
        r.write(&[1.0, 2.0, 3.0]);
        let mut tmp = [0.0; 2];
        r.read(&mut tmp); // drain two → tail=2, head=3
                          // Now writing 3 samples puts one at index 3 and two at indices
                          // 0,1 — the wrap-around path inside `write`.
        let n = r.write(&[4.0, 5.0, 6.0]);
        assert_eq!(n, 3);
        let mut out = [0.0; 4];
        let got = r.read(&mut out);
        assert_eq!(got, 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn read_empty_returns_zero() {
        let mut r = AudioRing::new(4);
        let mut out = [0.0; 4];
        assert_eq!(r.read(&mut out), 0);
    }

    #[test]
    fn reset_empties_the_ring() {
        let mut r = AudioRing::new(8);
        r.write(&[1.0, 2.0, 3.0]);
        assert_eq!(r.available_read(), 3);
        r.reset();
        assert_eq!(r.available_read(), 0);
        assert_eq!(r.available_write(), 8);
    }
}
