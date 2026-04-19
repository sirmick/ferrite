//! `BroadcastIqSink` — transport hook that turns `IqBridgeSink` pushes
//! from a runtime's `WsBridgeTx` into framed WebSocket frames on the
//! session's broadcast channel.
//!
//! The Rust runtime instantiates a `WsBridgeTx` for each cross-env
//! wire in the node half of a preset. The bridge publishes IQ samples
//! through its `IqBridgeSink` handle — this module owns the framing,
//! per-stream `seq` counter, and the broadcast-channel hop into the
//! outbound WS fanout.
//!
//! Payload encoding matches `docs/02-protocol.md`: `payload_type =
//! IqF32`, interleaved I,Q little-endian floats (LE is asserted on the
//! wire for IQ — see §"Endianness"). Per-stream `seq` wraps at `u32`.

// The preset-driven pipeline that instantiates this sink lands in the
// next commit; until then only the module's tests exercise it.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use ferrite_blocks::ws_bridge::IqBridgeSink;
use num_complex::Complex;

use crate::{
    session::FrameTx,
    ws_frame::{encode, FrameHeader, PayloadType, PROTOCOL_VERSION},
};

/// Adapter from the runtime's transport-free `IqBridgeSink` trait to
/// the session's `FrameTx` broadcast channel.
///
/// Cloneable by `Arc`: wrap in `Arc::new(BroadcastIqSink::new(tx))`
/// and hand that arc to `WsBridgeTx::attach_sink`.
pub struct BroadcastIqSink {
    tx: FrameTx,
    /// Per-stream monotonic seq counter. Separate per stream so each
    /// bridge-pair's `seq` survives a gap on any other stream.
    seqs: Mutex<HashMap<u16, u32>>,
}

impl BroadcastIqSink {
    #[must_use]
    pub fn new(tx: FrameTx) -> Self {
        Self {
            tx,
            seqs: Mutex::new(HashMap::new()),
        }
    }

    fn next_seq(&self, stream_id: u16) -> u32 {
        let mut map = self.seqs.lock().expect("seq lock poisoned");
        let slot = map.entry(stream_id).or_insert(0);
        let out = *slot;
        *slot = slot.wrapping_add(1);
        out
    }
}

impl IqBridgeSink for BroadcastIqSink {
    fn push_iq_f32(&self, stream_id: u32, samples: &[Complex<f32>]) {
        if samples.is_empty() {
            return;
        }
        // The wire format has a 16-bit stream_id; bridge ids live in the
        // `CROSS_ENV_STREAM_BASE` (1000+) range which fits easily. Clamp
        // as a defensive measure so a bug upstream doesn't panic here.
        let stream_id_u16 = u16::try_from(stream_id).unwrap_or(u16::MAX);
        let seq = self.next_seq(stream_id_u16);
        let header = FrameHeader {
            version: PROTOCOL_VERSION,
            payload_type: PayloadType::IqF32,
            stream_id: stream_id_u16,
            seq,
            timestamp_ns: now_ns(),
        };
        let mut payload = Vec::with_capacity(samples.len() * 8);
        for c in samples {
            payload.extend_from_slice(&c.re.to_le_bytes());
            payload.extend_from_slice(&c.im.to_le_bytes());
        }
        let frame = encode(&header, &payload);
        // send() only fails when there are no live receivers; drop
        // silently so a pre-subscribe startup doesn't log-spam.
        let _ = self.tx.send(std::sync::Arc::new(frame));
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
    use super::BroadcastIqSink;
    use crate::ws_frame::{decode, PayloadType};
    use ferrite_blocks::ws_bridge::IqBridgeSink;
    use num_complex::Complex;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn pushes_encode_as_iq_f32_frames() {
        let (tx, mut rx) = broadcast::channel(8);
        let sink: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(tx));
        let samples = vec![Complex::new(1.0_f32, 2.0), Complex::new(3.0, 4.0)];
        sink.push_iq_f32(1000, &samples);
        let bytes = rx.recv().await.unwrap();
        let (header, payload) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::IqF32);
        assert_eq!(header.stream_id, 1000);
        assert_eq!(header.seq, 0);
        assert_eq!(payload.len(), 16);
        // Little-endian interleaved I,Q.
        assert_eq!(&payload[0..4], &1.0_f32.to_le_bytes());
        assert_eq!(&payload[4..8], &2.0_f32.to_le_bytes());
        assert_eq!(&payload[8..12], &3.0_f32.to_le_bytes());
        assert_eq!(&payload[12..16], &4.0_f32.to_le_bytes());
    }

    #[tokio::test]
    async fn seq_counter_is_per_stream() {
        let (tx, mut rx) = broadcast::channel(16);
        let sink: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(tx));
        let s = vec![Complex::new(0.0_f32, 0.0)];
        sink.push_iq_f32(1000, &s);
        sink.push_iq_f32(1001, &s);
        sink.push_iq_f32(1000, &s);
        sink.push_iq_f32(1001, &s);
        let mut seen: Vec<(u16, u32)> = Vec::new();
        for _ in 0..4 {
            let bytes = rx.recv().await.unwrap();
            let (h, _) = decode(&bytes).unwrap();
            seen.push((h.stream_id, h.seq));
        }
        assert_eq!(
            seen,
            vec![(1000, 0), (1001, 0), (1000, 1), (1001, 1)],
            "each stream_id gets its own monotonic seq",
        );
    }

    #[tokio::test]
    async fn empty_push_sends_nothing() {
        let (tx, mut rx) = broadcast::channel(4);
        let sink: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(tx));
        sink.push_iq_f32(1000, &[]);
        assert!(
            rx.try_recv().is_err(),
            "empty push should not enqueue a frame",
        );
    }

    #[test]
    fn send_without_receivers_does_not_panic() {
        let (tx, rx) = broadcast::channel::<crate::session::FrameBytes>(4);
        let sink: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(tx));
        // Drop the receiver so the broadcast channel has no subscribers.
        drop(rx);
        sink.push_iq_f32(1000, &[Complex::new(1.0, 1.0)]);
        // No panic = pass.
    }
}
