//! `Frame` — the on-wire envelope every WS message flows through.
//!
//! Server and browser share this single schema. Serialization is
//! [`postcard`](https://docs.rs/postcard) — compact, self-framed,
//! varint-encoded. The variant tag is the protocol discriminator; a
//! version bump is a schema change (add/remove/reshape a variant).
//!
//! The block-side producer builds a [`Frame`] with `seq = 0` and
//! `timestamp_ns = 0`; the sink overwrites both from its own counters
//! before serializing. That keeps `seq` monotonic without threading a
//! mutable counter into every block.
//!
//! # Per-variant fields
//!
//! Envelope fields (`stream_id`, `seq`, `timestamp_ns`) are duplicated
//! across variants rather than pulled into a wrapper struct. The
//! rationale is decode-side readability: a `match frame { Frame::IqF32
//! { stream_id, samples, .. } => … }` reads cleaner than the
//! equivalent with a detached header.
//!
//! # Payload bytes
//!
//! The block crate keeps payloads as raw bytes tagged by the variant
//! (IQ = interleaved LE f32, `FftU8` = raw bins, `JsonEvent` = UTF-8
//! JSON). Each block owns its encoding; the transport layer stays out
//! of sample math.

use serde::{Deserialize, Serialize};

/// `stream_id = 0` is reserved for the control stream (JSON events).
pub const CONTROL_STREAM: u16 = 0;
/// Conventional `stream_id` for the session's waterfall FFT output.
pub const FFT_STREAM: u16 = 1;
/// First `stream_id` the server allocates for a legacy-mode VFO.
/// VFOs are `>= 2` per `docs/02-protocol.md` §"Stream IDs".
pub const VFO_STREAM_BASE: u16 = FFT_STREAM + 1;

/// Single wire envelope carried over every WebSocket connection.
///
/// The variant tag identifies the payload kind; the envelope fields
/// (`stream_id`, `seq`, `timestamp_ns`) are duplicated per variant on
/// purpose — see the module-level docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Baseband IQ samples. `payload` is interleaved little-endian
    /// `f32` pairs, `[re0, im0, re1, im1, …]`, 8 bytes per sample.
    ///
    /// `sample_rate_hz` is the declared rate at the producer's output
    /// port, rounded to the nearest integer Hz. `0` means "unknown /
    /// legacy frame"; the consumer should fall back to whatever its
    /// config or a previous frame told it. Non-zero lets a receiver
    /// pick up the actual runtime rate rather than a preset-time guess
    /// — essential when the producer's rate depends on the actual SDR
    /// ladder (e.g. a channelizer at `round(source_rate/target)`).
    IqF32 {
        stream_id: u16,
        seq: u32,
        timestamp_ns: u64,
        sample_rate_hz: u32,
        payload: Vec<u8>,
    },
    /// Real audio samples. `payload` is little-endian `f32`, 4 bytes per
    /// sample. `sample_rate_hz` follows the same contract as `IqF32` —
    /// the producer's declared output rate, `0` for unknown.
    F32 {
        stream_id: u16,
        seq: u32,
        timestamp_ns: u64,
        sample_rate_hz: u32,
        payload: Vec<u8>,
    },
    /// Log-magnitude FFT bins, one `u8` per bin.
    FftU8 {
        stream_id: u16,
        seq: u32,
        timestamp_ns: u64,
        payload: Vec<u8>,
    },
    /// UTF-8 JSON body — session events, reconfigure notices, warnings.
    JsonEvent {
        stream_id: u16,
        seq: u32,
        timestamp_ns: u64,
        payload: Vec<u8>,
    },
}

impl Frame {
    /// Addressing key: `(variant discriminant, stream_id)`. The sink
    /// uses this to keep an independent `seq` counter per kind per
    /// stream, so an `FftU8` and an `IqF32` that share a stream id don't
    /// interleave their sequences.
    #[must_use]
    pub fn stream_key(&self) -> (std::mem::Discriminant<Self>, u16) {
        (std::mem::discriminant(self), self.stream_id())
    }

    #[must_use]
    pub const fn stream_id(&self) -> u16 {
        match self {
            Self::IqF32 { stream_id, .. }
            | Self::F32 { stream_id, .. }
            | Self::FftU8 { stream_id, .. }
            | Self::JsonEvent { stream_id, .. } => *stream_id,
        }
    }

    /// Overwrite the envelope's `seq` and `timestamp_ns`. Called by the
    /// sink after the producing block emitted a zero-filled envelope.
    pub const fn set_envelope(&mut self, new_seq: u32, new_ts_ns: u64) {
        match self {
            Self::IqF32 {
                seq, timestamp_ns, ..
            }
            | Self::F32 {
                seq, timestamp_ns, ..
            }
            | Self::FftU8 {
                seq, timestamp_ns, ..
            }
            | Self::JsonEvent {
                seq, timestamp_ns, ..
            } => {
                *seq = new_seq;
                *timestamp_ns = new_ts_ns;
            }
        }
    }

    /// Encode to a fresh `Vec<u8>` using postcard. Failure here means a
    /// serializer bug or OOM; callers currently `unwrap` since neither
    /// is recoverable on the data path.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Decode from a postcard byte slice.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// WASM bindings — `decodeFrame(bytes)` for the browser-side worker. Returns
// a JS object with a tagged-union shape matching the TS `ParsedFrame` type
// in `web/src/lib/ws/frame.ts`. Runs synchronously on the main thread after
// `init()` has resolved.
// ---------------------------------------------------------------------------

#[cfg(feature = "wasm")]
mod wasm_bindings {
    use super::Frame;
    use serde::Serialize;
    use wasm_bindgen::prelude::*;

    /// JS-shape of a decoded frame. The `payload_type` byte mirrors
    /// what the old 16-byte-header TS decoder surfaced, so existing
    /// consumers (`PayloadType.FftU8`, etc.) keep working. `payload`
    /// is a zero-copy `Uint8Array` slice of the inner bytes.
    #[derive(Serialize)]
    struct JsFrame<'a> {
        payload_type: u8,
        stream_id: u16,
        seq: u32,
        /// u64 timestamp — serialized as a BigInt by the configured
        /// serde-wasm-bindgen serializer.
        timestamp_ns: u64,
        /// Declared sample rate at the producer's output port. Non-zero
        /// for `IqF32` frames emitted by a `WsBridgeTx` that had its
        /// rate populated via `InitCtx.input_rate`; zero for `FftU8` /
        /// `JsonEvent` / legacy IQ frames where rate isn't meaningful.
        sample_rate_hz: u32,
        #[serde(with = "serde_bytes")]
        payload: &'a [u8],
    }

    /// Payload-type bytes — pinned at the values the old custom header
    /// used, so the existing `PayloadType` lookup table in
    /// `web/src/lib/ws/frame.ts` stays valid.
    const PT_IQ_F32: u8 = 0x11;
    const PT_F32: u8 = 0x12;
    const PT_FFT_U8: u8 = 0x01;
    const PT_JSON_EVENT: u8 = 0x80;

    /// Decode a postcard-serialized `Frame` into a JS object. Throws a
    /// `JsError` on malformed input — decoder callers catch and surface
    /// to the `FrameClient`'s `onDecodeError` hook.
    #[wasm_bindgen(js_name = decodeFrame)]
    pub fn decode_frame(bytes: &[u8]) -> Result<JsValue, JsError> {
        let frame = Frame::from_postcard(bytes)
            .map_err(|e| JsError::new(&format!("postcard decode: {e}")))?;
        let js = match &frame {
            Frame::IqF32 {
                stream_id,
                seq,
                timestamp_ns,
                sample_rate_hz,
                payload,
            } => JsFrame {
                payload_type: PT_IQ_F32,
                stream_id: *stream_id,
                seq: *seq,
                timestamp_ns: *timestamp_ns,
                sample_rate_hz: *sample_rate_hz,
                payload,
            },
            Frame::F32 {
                stream_id,
                seq,
                timestamp_ns,
                sample_rate_hz,
                payload,
            } => JsFrame {
                payload_type: PT_F32,
                stream_id: *stream_id,
                seq: *seq,
                timestamp_ns: *timestamp_ns,
                sample_rate_hz: *sample_rate_hz,
                payload,
            },
            Frame::FftU8 {
                stream_id,
                seq,
                timestamp_ns,
                payload,
            } => JsFrame {
                payload_type: PT_FFT_U8,
                stream_id: *stream_id,
                seq: *seq,
                timestamp_ns: *timestamp_ns,
                sample_rate_hz: 0,
                payload,
            },
            Frame::JsonEvent {
                stream_id,
                seq,
                timestamp_ns,
                payload,
            } => JsFrame {
                payload_type: PT_JSON_EVENT,
                stream_id: *stream_id,
                seq: *seq,
                timestamp_ns: *timestamp_ns,
                sample_rate_hz: 0,
                payload,
            },
        };
        // Configure the serializer to emit u64 as BigInt and `&[u8]` as
        // a Uint8Array (rather than a plain number array).
        let ser =
            serde_wasm_bindgen::Serializer::new().serialize_large_number_types_as_bigints(true);
        js.serialize(&ser)
            .map_err(|e| JsError::new(&format!("wasm_bindgen serialize: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, CONTROL_STREAM, FFT_STREAM, VFO_STREAM_BASE};

    #[test]
    fn stream_constants_preserve_ordering() {
        assert_eq!(CONTROL_STREAM, 0);
        assert_eq!(FFT_STREAM, 1);
        assert_eq!(VFO_STREAM_BASE, 2);
    }

    #[test]
    fn round_trip_iq_f32() {
        let bytes = {
            let samples: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
            samples.iter().flat_map(|x| x.to_le_bytes()).collect()
        };
        let f = Frame::IqF32 {
            stream_id: 1000,
            seq: 0xDEAD_BEEF,
            timestamp_ns: 0x0123_4567_89AB_CDEF,
            sample_rate_hz: 0,
            payload: bytes,
        };
        let wire = f.to_postcard().unwrap();
        let back = Frame::from_postcard(&wire).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn round_trip_f32() {
        let bytes = {
            let samples: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
            samples.iter().flat_map(|x| x.to_le_bytes()).collect()
        };
        let f = Frame::F32 {
            stream_id: 1001,
            seq: 0xDEAD_BEEF,
            timestamp_ns: 0x0123_4567_89AB_CDEF,
            sample_rate_hz: 48_000,
            payload: bytes,
        };
        let wire = f.to_postcard().unwrap();
        let back = Frame::from_postcard(&wire).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn round_trip_fft_u8() {
        let f = Frame::FftU8 {
            stream_id: FFT_STREAM,
            seq: 7,
            timestamp_ns: 42,
            payload: vec![0, 64, 128, 192, 255],
        };
        let wire = f.to_postcard().unwrap();
        let back = Frame::from_postcard(&wire).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn round_trip_json_event() {
        let f = Frame::JsonEvent {
            stream_id: CONTROL_STREAM,
            seq: 1,
            timestamp_ns: 99,
            payload: br#"{"type":"ping"}"#.to_vec(),
        };
        let wire = f.to_postcard().unwrap();
        let back = Frame::from_postcard(&wire).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn stream_key_differs_by_variant_and_id() {
        let a = Frame::IqF32 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![],
        };
        let b = Frame::FftU8 {
            stream_id: 1,
            seq: 0,
            timestamp_ns: 0,
            payload: vec![],
        };
        let c = Frame::IqF32 {
            stream_id: 2,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![],
        };
        assert_ne!(a.stream_key(), b.stream_key(), "variant tag differs");
        assert_ne!(a.stream_key(), c.stream_key(), "stream id differs");
    }

    #[test]
    fn set_envelope_writes_all_variants() {
        let mut iq = Frame::IqF32 {
            stream_id: 10,
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz: 0,
            payload: vec![],
        };
        iq.set_envelope(42, 9_000_000_000);
        let Frame::IqF32 {
            seq, timestamp_ns, ..
        } = iq
        else {
            panic!("variant changed");
        };
        assert_eq!((seq, timestamp_ns), (42, 9_000_000_000));
    }

    #[test]
    fn rejects_truncated_bytes() {
        assert!(Frame::from_postcard(&[]).is_err());
        assert!(Frame::from_postcard(&[0xFF]).is_err());
    }
}
