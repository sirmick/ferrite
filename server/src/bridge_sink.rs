//! `BroadcastIqSink` / `BroadcastFftU8Sink` — transport hooks that turn
//! bridge-sink pushes from the Rust runtime into framed WebSocket frames
//! on the session's broadcast channel.
//!
//! The runtime instantiates a `WsBridgeTx`/`WsBridgeTxFftU8` for each
//! cross-env wire in the node half of a preset. Each bridge publishes
//! through a typed sink trait (`IqBridgeSink`, `FftU8BridgeSink`); the
//! corresponding struct here owns the framing, the per-stream `seq`
//! counter, and the broadcast-channel hop into the outbound WS fanout.
//!
//! Payload encoding matches `docs/02-protocol.md`: IQ frames are
//! interleaved I,Q little-endian floats tagged `IqF32`; FFT frames are
//! the log-magnitude byte slice tagged `FftU8`. Per-stream `seq` wraps
//! at `u32`; each payload type keeps its own seq map so a collision on
//! `stream_id` across types (which is a misuse) doesn't interleave
//! their counters.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use ferrite_blocks::ws_bridge::{FftU8BridgeSink, IqBridgeSink};
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

/// Sibling of [`BroadcastIqSink`] for the `FftU8` port type. Wraps
/// incoming log-magnitude byte slices as `PayloadType::FftU8` frames
/// and pushes them onto the same broadcast channel the IQ sink uses;
/// subscribers on `/ws/preset` demultiplex by `stream_id`.
pub struct BroadcastFftU8Sink {
    tx: FrameTx,
    /// Per-stream seq counter — independent from the IQ sink's map
    /// because the payload types are distinct channels on the wire.
    seqs: Mutex<HashMap<u16, u32>>,
}

impl BroadcastFftU8Sink {
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

impl FftU8BridgeSink for BroadcastFftU8Sink {
    fn push_fft_u8(&self, stream_id: u32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let stream_id_u16 = u16::try_from(stream_id).unwrap_or(u16::MAX);
        let seq = self.next_seq(stream_id_u16);
        let header = FrameHeader {
            version: PROTOCOL_VERSION,
            payload_type: PayloadType::FftU8,
            stream_id: stream_id_u16,
            seq,
            timestamp_ns: now_ns(),
        };
        let frame = encode(&header, bytes);
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

    use super::BroadcastFftU8Sink;
    use ferrite_blocks::ws_bridge::FftU8BridgeSink;

    #[tokio::test]
    async fn fft_push_encodes_as_fft_u8_frame() {
        let (tx, mut rx) = broadcast::channel(8);
        let sink: Arc<dyn FftU8BridgeSink> = Arc::new(BroadcastFftU8Sink::new(tx));
        let bins = vec![0u8, 1, 2, 3, 255];
        sink.push_fft_u8(1, &bins);
        let bytes = rx.recv().await.unwrap();
        let (header, payload) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::FftU8);
        assert_eq!(header.stream_id, 1);
        assert_eq!(header.seq, 0);
        assert_eq!(payload, bins.as_slice());
    }

    #[tokio::test]
    async fn fft_seq_counter_is_per_stream_and_independent_of_iq() {
        // Same FrameTx feeds both sinks. The FftU8 sink's seqs are
        // independent from the IQ sink's, so pushing on stream 1 from
        // each yields seq=0 for both (not seq=0 then seq=1).
        let (tx, mut rx) = broadcast::channel(16);
        let iq: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(tx.clone()));
        let fft: Arc<dyn FftU8BridgeSink> = Arc::new(BroadcastFftU8Sink::new(tx));
        iq.push_iq_f32(1, &[Complex::new(1.0, 0.0)]);
        fft.push_fft_u8(1, &[0, 1, 2]);
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
    async fn fft_empty_push_sends_nothing() {
        let (tx, mut rx) = broadcast::channel(4);
        let sink: Arc<dyn FftU8BridgeSink> = Arc::new(BroadcastFftU8Sink::new(tx));
        sink.push_fft_u8(1, &[]);
        assert!(rx.try_recv().is_err());
    }
}
