//! `BroadcastSink` — the one transport hook the runtime's bridge-Tx
//! blocks push through. Stamps the envelope on every incoming [`Frame`]
//! and pipes the postcard-serialized bytes onto the session's broadcast
//! channel.
//!
//! The runtime instantiates a `WsBridgeTx` or `WsBridgeTxFftU8` for
//! each cross-env wire in the node half of a preset. Every Tx block
//! publishes through the shared [`BridgeSink`] trait; this struct owns
//! the per-variant `seq` counter, the `timestamp_ns` clock, and the
//! broadcast-channel hop into the outbound WS fanout.
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
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use ferrite_blocks::{frame::Frame, ws_bridge::BridgeSink};

use crate::app_state::FrameTx;

/// Adapter from the runtime's transport-free [`BridgeSink`] trait to
/// the session's [`FrameTx`] broadcast channel. One instance serves
/// every bridge-Tx block in a preset, regardless of port type.
///
/// Cloneable by `Arc`: wrap in `Arc::new(BroadcastSink::new(tx))` and
/// hand that arc to every Tx block's `attach_sink`.
pub struct BroadcastSink {
    tx: FrameTx,
    /// Per-`(variant, stream_id)` monotonic seq counter. Keyed by the
    /// pair so two streams that share a stream id but differ in Frame
    /// variant each keep their own sequence — a subscriber that filters
    /// by stream id (and variant) sees a clean monotonic series.
    seqs: Mutex<HashMap<(Discriminant<Frame>, u16), u32>>,
}

impl BroadcastSink {
    #[must_use]
    pub fn new(tx: FrameTx) -> Self {
        Self {
            tx,
            seqs: Mutex::new(HashMap::new()),
        }
    }

    fn next_seq(&self, key: (Discriminant<Frame>, u16)) -> u32 {
        let mut map = self.seqs.lock().expect("seq lock poisoned");
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
        let bytes = match frame.to_postcard() {
            Ok(b) => b,
            Err(err) => {
                tracing::error!(?err, "frame postcard encode");
                return;
            }
        };
        // send() only fails when there are no live receivers; drop
        // silently so a pre-subscribe startup doesn't log-spam.
        let _ = self.tx.send(std::sync::Arc::new(bytes));
    }
}

const fn frame_payload_is_empty(f: &Frame) -> bool {
    match f {
        Frame::IqF32 { payload, .. }
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
    use ferrite_blocks::{frame::Frame, ws_bridge::BridgeSink};
    use std::sync::Arc;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn iq_push_encodes_as_iq_f32_frame() {
        let (tx, mut rx) = broadcast::channel(8);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let payload: Vec<u8> = [1.0_f32, 2.0, 3.0, 4.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            payload: payload.clone(),
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
        let (tx, mut rx) = broadcast::channel(8);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let bins = vec![0u8, 1, 2, 3, 255];
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: bins.clone(),
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
        let (tx, mut rx) = broadcast::channel(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let sample: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let mk = |stream_id: u16| Frame::IqF32 {
            stream_id,
            seq: 0,
            timestamp_ns: 0,
            payload: sample.clone(),
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
        let (tx, mut rx) = broadcast::channel(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let iq: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(Frame::IqF32 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: iq,
        });
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![0, 1, 2],
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
        let (tx, mut rx) = broadcast::channel(4);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![],
        });
        sink.push(Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![],
        });
        assert!(
            rx.try_recv().is_err(),
            "empty push should not enqueue a frame",
        );
    }

    #[test]
    fn send_without_receivers_does_not_panic() {
        let (tx, rx) = broadcast::channel::<crate::app_state::FrameBytes>(4);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        drop(rx);
        sink.push(Frame::IqF32 {
            stream_id: 1000,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![1, 2, 3, 4],
        });
    }
}
