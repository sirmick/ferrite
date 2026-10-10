//! `BroadcastSink` — the one transport hook the runtime's bridge-Tx
//! blocks push through. Stamps the envelope on every incoming [`Frame`]
//! and fans the postcard-serialized bytes out on the shared
//! [`FrameBus`].
//!
//! The runtime instantiates a `WsBridgeTx` or `WsBridgeTxFftU8` for
//! each cross-env wire in the node half of a preset. Every Tx block
//! publishes through the shared [`BridgeSink`] trait; this struct owns
//! the per-variant `seq` counter, the `timestamp_ns` clock, and the
//! per-subscriber fan-out into the outbound WS connections.
//!
//! Blocks do their own payload encoding (IQ → interleaved LE f32,
//! FftU8 → raw bytes) before wrapping in a [`Frame`]. The sink fills
//! in `seq` and `timestamp_ns` — both zero when the block emitted the
//! frame — before serializing. `seq` wraps at `u32`; keying by
//! `(variant, stream_id)` keeps IQ and FFT streams from interleaving
//! their counters when they share a stream id.

use std::{
    collections::HashMap,
    mem::Discriminant,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use ferrite_blocks::{frame::Frame, ws_bridge::BridgeSink};

use crate::frame_bus::FrameBus;

/// Adapter from the runtime's transport-free [`BridgeSink`] trait to
/// the server's shared [`FrameBus`] fan-out. One instance serves
/// every bridge-Tx block in a preset, regardless of port type.
///
/// Cloneable by `Arc`: wrap in `Arc::new(BroadcastSink::new(bus))` and
/// hand that arc to every Tx block's `attach_sink`.
pub struct BroadcastSink {
    bus: FrameBus,
    /// Per-`(variant, stream_id)` monotonic seq counter. Keyed by the
    /// pair so two streams that share a stream id but differ in Frame
    /// variant each keep their own sequence — a subscriber that filters
    /// by stream id (and variant) sees a clean monotonic series.
    seqs: Mutex<HashMap<(Discriminant<Frame>, u16), u32>>,
    /// Graph epoch stamped on every frame — shared with `AppState`, which
    /// bumps it once per topology rebuild. Stream ids are positional and
    /// reused across rebuilds; the epoch is what lets a subscriber tell
    /// "stream 1000 of the old graph" from "stream 1000 of the new one".
    epoch: Arc<AtomicU64>,
}

impl BroadcastSink {
    /// A sink with its own (never-bumped) epoch — tests and harnesses.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(bus: FrameBus) -> Self {
        Self::with_epoch(bus, Arc::new(AtomicU64::new(0)))
    }

    /// The production constructor: share the daemon's epoch counter.
    #[must_use]
    pub fn with_epoch(bus: FrameBus, epoch: Arc<AtomicU64>) -> Self {
        Self {
            bus,
            seqs: Mutex::new(HashMap::new()),
            epoch,
        }
    }

    fn next_seq(&self, key: (Discriminant<Frame>, u16)) -> u32 {
        // Poison-recovery (mirrors FrameBus): a panic while holding this
        // lock leaves no broken invariant — worst case a per-stream seq
        // resumes from a stale value — so recover the guard rather than
        // propagating the poison and wedging every subsequent frame.
        let mut map = self.seqs.lock().unwrap_or_else(|e| e.into_inner());
        let slot = map.entry(key).or_insert(0);
        let out = *slot;
        *slot = slot.wrapping_add(1);
        out
    }
}

impl BridgeSink for BroadcastSink {
    fn push(&self, mut frame: Frame) {
        // Blocks still hand us frames when their payload is empty
        // (e.g. a tick produced zero samples). Drop those silently so
        // the wire doesn't carry no-op frames — subscribers count
        // `seq`s to detect loss and an empty payload looks like packet
        // loss to them.
        if frame_payload_is_empty(&frame) {
            return;
        }
        let seq = self.next_seq(frame.stream_key());
        frame.set_envelope(seq, now_ns());
        #[allow(clippy::cast_possible_truncation)]
        frame.set_epoch(self.epoch.load(Ordering::Relaxed) as u32);
        let bytes = match frame.to_postcard() {
            Ok(b) => b,
            Err(err) => {
                tracing::error!(?err, "frame postcard encode");
                return;
            }
        };
        // Per-subscriber try_send inside the bus — a full queue drops
        // only that subscriber's copy; a missing subscriber is a
        // silent no-op. Sync by design: the scheduler thread must
        // never await here.
        self.bus.send(std::sync::Arc::new(bytes));
    }
}

const fn frame_payload_is_empty(f: &Frame) -> bool {
    match f {
        Frame::IqF32 { payload, .. }
        | Frame::F32 { payload, .. }
        | Frame::FftU8 { payload, .. }
        | Frame::JsonEvent { payload, .. } => payload.is_empty(),
    }
}

fn now_ns() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    #[allow(clippy::cast_possible_truncation)]
    {
        dur.as_nanos() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::BroadcastSink;
    use crate::frame_bus::FrameBus;
    use ferrite_blocks::{frame::Frame, ws_bridge::BridgeSink};
    use std::sync::Arc;

    // Regression: poisoning the seq lock (a panic while it's held) must
    // not wedge the sink — `next_seq` recovers the guard. Without the fix
    // (`.expect("seq lock poisoned")`), the push below panics.
    #[tokio::test]
    async fn survives_a_poisoned_seq_lock() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(8);
        let sink = Arc::new(BroadcastSink::new(bus));
        let s2 = Arc::clone(&sink);
        let poisoned = std::thread::spawn(move || {
            let _g = s2.seqs.lock().unwrap();
            panic!("poison the seq lock");
        })
        .join();
        assert!(poisoned.is_err(), "the helper thread must have panicked");

        // A push after poisoning still emits a frame (non-empty payload —
        // `push` drops empty ones by design).
        let payload: Vec<u8> = [1.0_f32, 2.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload,
            epoch: 0,
        });
        let bytes = rx.recv().await.unwrap();
        assert!(Frame::from_postcard(&bytes).is_ok());
    }

    /// The sink stamps the *shared* epoch on every frame it emits — the
    /// value current at push time, so a bump between two pushes shows up
    /// on the second frame and a subscriber can tell the graphs apart
    /// even though the stream id is reused.
    #[tokio::test]
    async fn push_stamps_the_shared_epoch() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(8);
        let epoch = Arc::new(AtomicU64::new(7));
        let sink = BroadcastSink::with_epoch(bus, Arc::clone(&epoch));
        let mk = || Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![0; 8],
            epoch: 0,
        };
        sink.push(mk());
        epoch.store(8, Ordering::Relaxed);
        sink.push(mk());
        let first = Frame::from_postcard(&rx.recv().await.unwrap()).unwrap();
        let second = Frame::from_postcard(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(first.epoch(), 7);
        assert_eq!(second.epoch(), 8);
    }

    #[tokio::test]
    async fn iq_push_encodes_as_iq_f32_frame() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(8);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        let payload: Vec<u8> = [1.0_f32, 2.0, 3.0, 4.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: payload.clone(),
            epoch: 0,
        });
        let bytes = rx.recv().await.unwrap();
        let frame = Frame::from_postcard(&bytes).unwrap();
        match frame {
            Frame::IqF32 {
                stream_id,
                seq,
                payload: got,
                ..
            } => {
                assert_eq!(stream_id, 1000);
                assert_eq!(seq, 0);
                assert_eq!(got, payload);
            }
            other => panic!("expected IqF32, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fft_push_encodes_as_fft_u8_frame() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(8);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        let bins = vec![0u8, 1, 2, 3, 255];
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: bins.clone(),
            epoch: 0,
        });
        let bytes = rx.recv().await.unwrap();
        let frame = Frame::from_postcard(&bytes).unwrap();
        match frame {
            Frame::FftU8 {
                stream_id,
                seq,
                payload,
                ..
            } => {
                assert_eq!(stream_id, 1);
                assert_eq!(seq, 0);
                assert_eq!(payload, bins);
            }
            other => panic!("expected FftU8, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn seq_counter_is_per_stream() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        let sample: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let mk = |stream_id: u16| Frame::IqF32 {
            stream_id,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: sample.clone(),
            epoch: 0,
        };
        sink.push(mk(1000));
        sink.push(mk(1001));
        sink.push(mk(1000));
        sink.push(mk(1001));
        let mut seen: Vec<(u16, u32)> = Vec::new();
        for _ in 0..4 {
            let bytes = rx.recv().await.unwrap();
            match Frame::from_postcard(&bytes).unwrap() {
                Frame::IqF32 { stream_id, seq, .. } => seen.push((stream_id, seq)),
                other => panic!("expected IqF32, got {other:?}"),
            }
        }
        assert_eq!(
            seen,
            vec![(1000, 0), (1001, 0), (1000, 1), (1001, 1)],
            "each stream_id gets its own monotonic seq",
        );
    }

    #[tokio::test]
    async fn seq_is_independent_per_variant() {
        // Two Frame variants on the same stream id keep independent
        // sequence counters — a subscriber filtering by variant sees a
        // clean 0,1,2,… even when both variants share a stream id.
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        let iq: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(Frame::IqF32 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: iq,
            epoch: 0,
        });
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![0, 1, 2],
            epoch: 0,
        });
        let f0 = Frame::from_postcard(&rx.recv().await.unwrap()).unwrap();
        let f1 = Frame::from_postcard(&rx.recv().await.unwrap()).unwrap();
        assert!(matches!(
            f0,
            Frame::IqF32 {
                stream_id: 1,
                seq: 0,
                ..
            }
        ));
        assert!(matches!(
            f1,
            Frame::FftU8 {
                stream_id: 1,
                seq: 0,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn empty_push_sends_nothing() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(4);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![],
            epoch: 0,
        });
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![],
            epoch: 0,
        });
        assert!(
            rx.try_recv().is_err(),
            "empty push should not enqueue a frame",
        );
    }

    #[test]
    fn send_without_receivers_does_not_panic() {
        let bus = FrameBus::new();
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(bus));
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![1, 2, 3, 4],
            epoch: 0,
        });
    }
}
