//! `BroadcastSink` — the one transport hook the runtime's bridge-Tx
//! blocks push through. Turns pre-encoded payload bytes plus a
//! [`BridgePayloadType`] tag into framed WS frames on the session's
//! broadcast channel.
//!
//! The runtime instantiates a `WsBridgeTx` or `WsBridgeTxFftU8` for
//! each cross-env wire in the node half of a preset. Every Tx block
//! publishes through the shared [`BridgeSink`] trait; this struct owns
//! the framing, the per-(payload_type, stream_id) `seq` counter, and
//! the broadcast-channel hop into the outbound WS fanout.
//!
//! Payload encoding matches `docs/02-protocol.md`: blocks do their own
//! byte-level encoding (IQ → interleaved LE f32, FftU8 → raw bytes)
//! before pushing. The sink only tags and frames. `seq` wraps at
//! `u32`; keying by `(payload_type, stream_id)` keeps IQ and FFT
//! streams from interleaving their counters if they happen to share
//! the same wire stream id.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use ferrite_blocks::ws_bridge::{BridgePayloadType, BridgeSink};

use crate::{
    session::FrameTx,
    ws_frame::{encode, FrameHeader, PayloadType, PROTOCOL_VERSION},
};

/// Adapter from the runtime's transport-free [`BridgeSink`] trait to
/// the session's [`FrameTx`] broadcast channel. One instance serves
/// every bridge-Tx block in a preset, regardless of port type.
///
/// Cloneable by `Arc`: wrap in `Arc::new(BroadcastSink::new(tx))` and
/// hand that arc to every Tx block's `attach_sink`.
pub struct BroadcastSink {
    tx: FrameTx,
    /// Per-`(payload_type, stream_id)` monotonic seq counter. Keyed by
    /// the pair so two streams that share a stream id but differ in
    /// payload type each keep their own sequence — a subscriber that
    /// filters by stream id sees a clean monotonic series.
    seqs: Mutex<HashMap<(PayloadType, u16), u32>>,
}

impl BroadcastSink {
    #[must_use]
    pub fn new(tx: FrameTx) -> Self {
        Self {
            tx,
            seqs: Mutex::new(HashMap::new()),
        }
    }

    fn next_seq(&self, key: (PayloadType, u16)) -> u32 {
        let mut map = self.seqs.lock().expect("seq lock poisoned");
        let slot = map.entry(key).or_insert(0);
        let out = *slot;
        *slot = slot.wrapping_add(1);
        out
    }
}

impl BridgeSink for BroadcastSink {
    fn push(&self, stream_id: u32, payload_type: BridgePayloadType, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // The wire format has a 16-bit stream_id; bridge ids live in the
        // `CROSS_ENV_STREAM_BASE` (1000+) range which fits easily. Clamp
        // defensively so an upstream bug doesn't panic here.
        let stream_id_u16 = u16::try_from(stream_id).unwrap_or(u16::MAX);
        let wire_pt = map_payload_type(payload_type);
        let seq = self.next_seq((wire_pt, stream_id_u16));
        let header = FrameHeader {
            version: PROTOCOL_VERSION,
            payload_type: wire_pt,
            stream_id: stream_id_u16,
            seq,
            timestamp_ns: now_ns(),
        };
        let frame = encode(&header, bytes);
        // send() only fails when there are no live receivers; drop
        // silently so a pre-subscribe startup doesn't log-spam.
        let _ = self.tx.send(std::sync::Arc::new(frame));
    }
}

/// Translate the block-crate's payload tag to the wire protocol's.
/// Two parallel enums (instead of sharing one across crates) so the
/// block crate has no dependency on the server — the discriminants
/// match the wire bytes by convention, this mapping makes that
/// dependency explicit in one place.
const fn map_payload_type(pt: BridgePayloadType) -> PayloadType {
    match pt {
        BridgePayloadType::IqF32 => PayloadType::IqF32,
        BridgePayloadType::FftU8 => PayloadType::FftU8,
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
    use crate::ws_frame::{decode, PayloadType};
    use ferrite_blocks::ws_bridge::{BridgePayloadType, BridgeSink};
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
        sink.push(1000, BridgePayloadType::IqF32, &payload);
        let bytes = rx.recv().await.unwrap();
        let (header, body) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::IqF32);
        assert_eq!(header.stream_id, 1000);
        assert_eq!(header.seq, 0);
        assert_eq!(body, payload.as_slice());
    }

    #[tokio::test]
    async fn fft_push_encodes_as_fft_u8_frame() {
        let (tx, mut rx) = broadcast::channel(8);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let bins = vec![0u8, 1, 2, 3, 255];
        sink.push(1, BridgePayloadType::FftU8, &bins);
        let bytes = rx.recv().await.unwrap();
        let (header, payload) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::FftU8);
        assert_eq!(header.stream_id, 1);
        assert_eq!(header.seq, 0);
        assert_eq!(payload, bins.as_slice());
    }

    #[tokio::test]
    async fn seq_counter_is_per_stream() {
        let (tx, mut rx) = broadcast::channel(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let sample: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(1000, BridgePayloadType::IqF32, &sample);
        sink.push(1001, BridgePayloadType::IqF32, &sample);
        sink.push(1000, BridgePayloadType::IqF32, &sample);
        sink.push(1001, BridgePayloadType::IqF32, &sample);
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
    async fn seq_is_independent_per_payload_type() {
        // Two payload types on the same stream id keep independent
        // sequence counters — a subscriber filtering by payload_type
        // sees a clean 0,1,2,… even when both types share a stream id.
        let (tx, mut rx) = broadcast::channel(16);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        let iq: Vec<u8> = [0.0_f32, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        sink.push(1, BridgePayloadType::IqF32, &iq);
        sink.push(1, BridgePayloadType::FftU8, &[0, 1, 2]);
        let (h0, _) = decode(&rx.recv().await.unwrap()).unwrap();
        let (h1, _) = decode(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            (h0.payload_type, h0.stream_id, h0.seq),
            (PayloadType::IqF32, 1, 0)
        );
        assert_eq!(
            (h1.payload_type, h1.stream_id, h1.seq),
            (PayloadType::FftU8, 1, 0)
        );
    }

    #[tokio::test]
    async fn empty_push_sends_nothing() {
        let (tx, mut rx) = broadcast::channel(4);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        sink.push(1000, BridgePayloadType::IqF32, &[]);
        sink.push(1, BridgePayloadType::FftU8, &[]);
        assert!(
            rx.try_recv().is_err(),
            "empty push should not enqueue a frame",
        );
    }

    #[test]
    fn send_without_receivers_does_not_panic() {
        let (tx, rx) = broadcast::channel::<crate::session::FrameBytes>(4);
        let sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(tx));
        drop(rx);
        sink.push(1000, BridgePayloadType::IqF32, &[1, 2, 3, 4]);
    }
}
