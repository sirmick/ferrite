//! Per-wire accumulating ring buffer with one writer and N readers.
//!
//! Each wire in the scheduler owns a `TypedRing`. The upstream block
//! writes samples into the ring on its turn; each downstream consumer
//! (fan-out wires have more than one) carries its own read pointer and
//! advances independently. A slow consumer cannot pull other consumers'
//! samples out from under them — the writer's available space is bounded
//! by the **slowest** reader.
//!
//! This is the storage backing the D23 scheduler rewrite. Under the old
//! overwrite-per-tick model upstream samples were lost whenever the
//! consumer's rate was below the producer's; here they accumulate and
//! stay available for later `process` calls until every reader has
//! consumed them.
//!
//! ### Contiguity and wrap
//!
//! [`TypedRing::read_peek`] and [`TypedRing::write_peek`] return
//! **contiguous** slices. When the ring's available region would wrap
//! across the storage boundary, the peek returns only the part up to
//! the boundary; the caller advances, then peeks again for the
//! wrapped-around half. The scheduler's work loop already re-runs
//! blocks that can still make progress, so a peek-advance-peek cycle
//! drains a ring without copies.
//!
//! ### Integer pointers
//!
//! `write_head` and `read_heads[i]` are `u32` modulo 2^32. Available
//! bytes are `write_head.wrapping_sub(read_head[i])`, which stays
//! correct for the ring's lifetime as long as the distance between any
//! writer and any reader stays below `u32::MAX` elements. At 2.4 MS/s
//! that's 29 minutes — far longer than any single preset run today,
//! and the distance resets on every `init`.

use ferrite_blocks::{InBuf, OutBuf, PortType};
use num_complex::Complex;

/// The backing storage variants, mirroring [`PortType`]. One `Vec<T>`
/// per port type; the variant is fixed at construction and is the
/// source of truth for the ring's element type.
enum Storage {
    IqF32(Vec<Complex<f32>>),
    IqS16(Vec<Complex<i16>>),
    RealF32(Vec<f32>),
    RealI16(Vec<i16>),
    FftF32(Vec<f32>),
    FftU8(Vec<u8>),
    Bits(Vec<u8>),
    Frames(Vec<u8>),
    Events(Vec<u8>),
}

impl Storage {
    fn with_capacity(port_type: PortType, capacity: usize) -> Self {
        match port_type {
            PortType::IqF32 => Self::IqF32(vec![Complex::new(0.0, 0.0); capacity]),
            PortType::IqS16 => Self::IqS16(vec![Complex::new(0, 0); capacity]),
            PortType::RealF32 => Self::RealF32(vec![0.0; capacity]),
            PortType::RealI16 => Self::RealI16(vec![0; capacity]),
            PortType::FftF32 => Self::FftF32(vec![0.0; capacity]),
            PortType::FftU8 => Self::FftU8(vec![0; capacity]),
            PortType::Bits => Self::Bits(vec![0; capacity]),
            PortType::Frames => Self::Frames(vec![0; capacity]),
            PortType::Events => Self::Events(vec![0; capacity]),
        }
    }

    fn port_type(&self) -> PortType {
        match self {
            Self::IqF32(_) => PortType::IqF32,
            Self::IqS16(_) => PortType::IqS16,
            Self::RealF32(_) => PortType::RealF32,
            Self::RealI16(_) => PortType::RealI16,
            Self::FftF32(_) => PortType::FftF32,
            Self::FftU8(_) => PortType::FftU8,
            Self::Bits(_) => PortType::Bits,
            Self::Frames(_) => PortType::Frames,
            Self::Events(_) => PortType::Events,
        }
    }
}

/// Accumulating ring buffer for one wire. Storage is power-of-two sized
/// so head-to-index conversion is a mask.
pub struct TypedRing {
    storage: Storage,
    /// `capacity - 1` — `head & mask` indexes storage.
    mask: usize,
    /// Writer advances this after each successful write. Modulo 2^32.
    write_head: u32,
    /// One reader per fan-out consumer, all start at 0. `read_heads[i]`
    /// advances after reader `i`'s block has consumed samples.
    read_heads: Vec<u32>,
}

impl TypedRing {
    /// Construct a ring of `capacity` elements with `num_readers`
    /// independent read pointers (all starting at 0).
    ///
    /// `capacity` is rounded up to the next power of two so head bits
    /// can be masked instead of reduced mod-n. `num_readers` must be
    /// ≥ 1 — a ring with no readers can never free space and would
    /// block the writer forever.
    #[must_use]
    pub fn new(port_type: PortType, capacity: usize, num_readers: usize) -> Self {
        assert!(num_readers >= 1, "TypedRing needs at least one reader");
        assert!(capacity >= 1, "TypedRing capacity must be >= 1");
        let capacity = capacity.next_power_of_two();
        Self {
            storage: Storage::with_capacity(port_type, capacity),
            mask: capacity - 1,
            write_head: 0,
            read_heads: vec![0; num_readers],
        }
    }

    /// Element type this ring carries.
    #[must_use]
    pub fn port_type(&self) -> PortType {
        self.storage.port_type()
    }

    /// Total capacity in elements. Always a power of two.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Number of independent readers this ring was built with.
    #[must_use]
    pub fn num_readers(&self) -> usize {
        self.read_heads.len()
    }

    /// Elements available to reader `idx` right now. Does **not**
    /// account for wrap — call [`Self::read_peek`] to get the contiguous
    /// slice (which may be shorter when the region straddles the
    /// capacity boundary).
    #[must_use]
    pub fn available_read(&self, idx: usize) -> usize {
        self.write_head
            .wrapping_sub(self.read_heads[idx])
            .min(u32::try_from(self.capacity()).unwrap_or(u32::MAX)) as usize
    }

    /// Free slots the writer can fill without overrunning the slowest
    /// reader. Like [`Self::available_read`], does not account for
    /// wrap.
    #[must_use]
    pub fn available_write(&self) -> usize {
        // The slowest reader is the one furthest behind the writer —
        // i.e. the largest `write_head - read_head[i]` (wrap-safe). Free
        // space is capacity - that distance.
        let cap = u32::try_from(self.capacity()).unwrap_or(u32::MAX);
        let slowest_lag = self
            .read_heads
            .iter()
            .map(|&r| self.write_head.wrapping_sub(r))
            .max()
            .unwrap_or(0);
        (cap - slowest_lag.min(cap)) as usize
    }

    /// Reset all pointers to zero. Called from the scheduler's `init`
    /// so every tick run starts from a known empty state. Storage
    /// contents are not zeroed — stale bytes remain but are
    /// unreadable because both heads are 0.
    pub fn reset(&mut self) {
        self.write_head = 0;
        for h in &mut self.read_heads {
            *h = 0;
        }
    }

    /// Read-only view of up to `max` contiguous elements available to
    /// reader `idx`. If the available region wraps across the
    /// capacity boundary, the returned slice stops at the boundary and
    /// the caller should [`advance_reader`](Self::advance_reader) then
    /// peek again for the wrapped tail.
    #[must_use]
    pub fn read_peek(&self, idx: usize, max: usize) -> InBuf<'_> {
        let head = self.read_heads[idx];
        let start = (head as usize) & self.mask;
        let contig = (self.capacity() - start)
            .min(self.available_read(idx))
            .min(max);
        match &self.storage {
            Storage::IqF32(v) => InBuf::IqF32(&v[start..start + contig]),
            Storage::IqS16(v) => InBuf::IqS16(&v[start..start + contig]),
            Storage::RealF32(v) => InBuf::RealF32(&v[start..start + contig]),
            Storage::RealI16(v) => InBuf::RealI16(&v[start..start + contig]),
            Storage::FftF32(v) => InBuf::FftF32(&v[start..start + contig]),
            Storage::FftU8(v) => InBuf::FftU8(&v[start..start + contig]),
            Storage::Bits(v) => InBuf::Bits(&v[start..start + contig]),
            Storage::Frames(v) => InBuf::Frames(&v[start..start + contig]),
            Storage::Events(v) => InBuf::Events(&v[start..start + contig]),
        }
    }

    /// Mutable view of up to `max` contiguous free slots. Like
    /// [`read_peek`](Self::read_peek), stops at the capacity boundary
    /// on wrap; the caller writes up to the returned slice's length,
    /// then [`advance_writer`](Self::advance_writer)s by the count
    /// actually written.
    pub fn write_peek(&mut self, max: usize) -> OutBuf<'_> {
        let head = self.write_head;
        let start = (head as usize) & self.mask;
        let contig = (self.capacity() - start)
            .min(self.available_write())
            .min(max);
        match &mut self.storage {
            Storage::IqF32(v) => OutBuf::IqF32(&mut v[start..start + contig]),
            Storage::IqS16(v) => OutBuf::IqS16(&mut v[start..start + contig]),
            Storage::RealF32(v) => OutBuf::RealF32(&mut v[start..start + contig]),
            Storage::RealI16(v) => OutBuf::RealI16(&mut v[start..start + contig]),
            Storage::FftF32(v) => OutBuf::FftF32(&mut v[start..start + contig]),
            Storage::FftU8(v) => OutBuf::FftU8(&mut v[start..start + contig]),
            Storage::Bits(v) => OutBuf::Bits(&mut v[start..start + contig]),
            Storage::Frames(v) => OutBuf::Frames(&mut v[start..start + contig]),
            Storage::Events(v) => OutBuf::Events(&mut v[start..start + contig]),
        }
    }

    /// Advance reader `idx` by `n` elements. `n` must not exceed
    /// [`available_read`](Self::available_read).
    pub fn advance_reader(&mut self, idx: usize, n: usize) {
        debug_assert!(
            n <= self.available_read(idx),
            "advance_reader past write head",
        );
        self.read_heads[idx] = self.read_heads[idx].wrapping_add(n as u32);
    }

    /// Advance the writer by `n` elements. `n` must not exceed
    /// [`available_write`](Self::available_write).
    pub fn advance_writer(&mut self, n: usize) {
        debug_assert!(
            n <= self.available_write(),
            "advance_writer past slowest reader",
        );
        self.write_head = self.write_head.wrapping_add(n as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::TypedRing;
    use ferrite_blocks::{InBuf, OutBuf, PortType};
    use num_complex::Complex;

    fn fill_iq(ring: &mut TypedRing, samples: &[Complex<f32>]) -> usize {
        let OutBuf::IqF32(dst) = ring.write_peek(samples.len()) else {
            panic!("wrong port type");
        };
        let n = dst.len().min(samples.len());
        dst[..n].copy_from_slice(&samples[..n]);
        ring.advance_writer(n);
        n
    }

    #[test]
    fn new_rounds_capacity_to_power_of_two() {
        let r = TypedRing::new(PortType::IqF32, 5, 1);
        assert_eq!(r.capacity(), 8);
        let r = TypedRing::new(PortType::IqF32, 8, 1);
        assert_eq!(r.capacity(), 8);
    }

    #[test]
    fn fresh_ring_is_empty() {
        let r = TypedRing::new(PortType::RealF32, 8, 1);
        assert_eq!(r.available_read(0), 0);
        assert_eq!(r.available_write(), 8);
    }

    #[test]
    fn write_then_read_round_trip() {
        let mut r = TypedRing::new(PortType::IqF32, 8, 1);
        let input = [
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ];
        assert_eq!(fill_iq(&mut r, &input), 3);
        assert_eq!(r.available_read(0), 3);
        assert_eq!(r.available_write(), 5);

        let InBuf::IqF32(view) = r.read_peek(0, 16) else {
            panic!();
        };
        assert_eq!(view, &input);
        r.advance_reader(0, 3);
        assert_eq!(r.available_read(0), 0);
        assert_eq!(r.available_write(), 8);
    }

    #[test]
    fn read_peek_stops_at_capacity_boundary() {
        let mut r = TypedRing::new(PortType::RealF32, 4, 1);
        // Writer fills 3, reader drains 3, writer pushes 3 more — the
        // new region spans indices 3, 0, 1 (wrap).
        let OutBuf::RealF32(dst) = r.write_peek(3) else {
            panic!();
        };
        dst.copy_from_slice(&[1.0, 2.0, 3.0]);
        r.advance_writer(3);
        r.advance_reader(0, 3);

        let OutBuf::RealF32(dst) = r.write_peek(3) else {
            panic!();
        };
        assert_eq!(dst.len(), 1, "first contig-write stops at capacity bound");
        dst.copy_from_slice(&[4.0]);
        r.advance_writer(1);
        let OutBuf::RealF32(dst) = r.write_peek(2) else {
            panic!();
        };
        assert_eq!(dst.len(), 2);
        dst.copy_from_slice(&[5.0, 6.0]);
        r.advance_writer(2);

        // Reader's region is [3, 0, 1] — first peek returns just [3].
        let InBuf::RealF32(v) = r.read_peek(0, 16) else {
            panic!();
        };
        assert_eq!(v, &[4.0]);
        r.advance_reader(0, 1);
        let InBuf::RealF32(v) = r.read_peek(0, 16) else {
            panic!();
        };
        assert_eq!(v, &[5.0, 6.0]);
    }

    #[test]
    fn write_peek_respects_slowest_reader() {
        let mut r = TypedRing::new(PortType::IqF32, 4, 2);
        // Fill to the top.
        assert_eq!(
            fill_iq(
                &mut r,
                &[
                    Complex::new(1.0, 0.0),
                    Complex::new(2.0, 0.0),
                    Complex::new(3.0, 0.0),
                    Complex::new(4.0, 0.0),
                ],
            ),
            4,
        );
        assert_eq!(r.available_write(), 0);

        // Reader 0 drains two, but reader 1 is still at zero. Writer
        // should see zero free space — the slowest reader still holds
        // the whole buffer.
        r.advance_reader(0, 2);
        assert_eq!(r.available_write(), 0);

        // Only after reader 1 catches up does the writer gain space.
        r.advance_reader(1, 2);
        assert_eq!(r.available_write(), 2);
    }

    #[test]
    fn readers_advance_independently() {
        let mut r = TypedRing::new(PortType::IqF32, 8, 2);
        let input = [
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ];
        fill_iq(&mut r, &input);

        // Reader 0 peeks and drains.
        let InBuf::IqF32(v) = r.read_peek(0, 16) else {
            panic!();
        };
        assert_eq!(v, &input);
        r.advance_reader(0, 3);
        assert_eq!(r.available_read(0), 0);

        // Reader 1 still sees all three.
        let InBuf::IqF32(v) = r.read_peek(1, 16) else {
            panic!();
        };
        assert_eq!(v, &input);
        assert_eq!(r.available_read(1), 3);
    }

    #[test]
    fn reset_returns_ring_to_empty() {
        let mut r = TypedRing::new(PortType::RealF32, 8, 2);
        let OutBuf::RealF32(dst) = r.write_peek(4) else {
            panic!();
        };
        dst.copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
        r.advance_writer(4);
        r.advance_reader(0, 2);

        r.reset();
        assert_eq!(r.available_read(0), 0);
        assert_eq!(r.available_read(1), 0);
        assert_eq!(r.available_write(), 8);
    }

    #[test]
    fn max_bound_caps_peek_length() {
        let mut r = TypedRing::new(PortType::RealF32, 8, 1);
        let OutBuf::RealF32(dst) = r.write_peek(8) else {
            panic!();
        };
        dst.fill(1.0);
        r.advance_writer(8);
        let InBuf::RealF32(v) = r.read_peek(0, 3) else {
            panic!();
        };
        assert_eq!(v.len(), 3, "max argument caps the returned slice");
    }

    #[test]
    fn port_type_reflects_storage() {
        assert_eq!(
            TypedRing::new(PortType::FftU8, 8, 1).port_type(),
            PortType::FftU8,
        );
        assert_eq!(
            TypedRing::new(PortType::Events, 8, 1).port_type(),
            PortType::Events,
        );
    }
}
