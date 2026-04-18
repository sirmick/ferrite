//! WebSocket binary frame codec — 16-byte header + payload.
//!
//! Wire format per `docs/02-protocol.md`:
//!
//! ```text
//! Offset  Size   Field
//! ------  -----  ------------
//!   0     1      version
//!   1     1      payload_type
//!   2     2      stream_id     (big-endian u16)
//!   4     4      seq           (big-endian u32)
//!   8     8      timestamp_ns  (big-endian u64)
//!  16     ...    payload
//! ```
//!
//! This module is intentionally dumb: it moves bytes. Payload-specific
//! framing (IQ samples, FFT bins, JSON events) is the caller's job.

// The scheduler that calls these helpers lands in the next commit; until
// then the encoder/decoder are exercised only by this module's tests.
#![allow(dead_code)]

use anyhow::{anyhow, Result};

pub const PROTOCOL_VERSION: u8 = 0x01;
pub const HEADER_LEN: usize = 16;

/// `stream_id = 0` is reserved for the control stream (JSON events).
pub const CONTROL_STREAM: u16 = 0;
/// Conventional `stream_id` for the session's waterfall FFT output.
pub const FFT_STREAM: u16 = 1;

/// Payload-type enum. Numeric values match the wire encoding — do not
/// reorder or reassign without bumping [`PROTOCOL_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PayloadType {
    FftU8 = 0x01,
    FftF16 = 0x02,
    IqS16 = 0x10,
    IqF32 = 0x11,
    AudioF32 = 0x20,
    JsonEvent = 0x80,
    JsonControl = 0x81,
    Error = 0xFF,
}

impl PayloadType {
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::FftU8),
            0x02 => Some(Self::FftF16),
            0x10 => Some(Self::IqS16),
            0x11 => Some(Self::IqF32),
            0x20 => Some(Self::AudioF32),
            0x80 => Some(Self::JsonEvent),
            0x81 => Some(Self::JsonControl),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub payload_type: PayloadType,
    pub stream_id: u16,
    pub seq: u32,
    pub timestamp_ns: u64,
}

impl FrameHeader {
    #[must_use]
    pub const fn new(payload_type: PayloadType, stream_id: u16) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            payload_type,
            stream_id,
            seq: 0,
            timestamp_ns: 0,
        }
    }

    pub fn write_into(&self, buf: &mut [u8; HEADER_LEN]) {
        buf[0] = self.version;
        buf[1] = self.payload_type.as_byte();
        buf[2..4].copy_from_slice(&self.stream_id.to_be_bytes());
        buf[4..8].copy_from_slice(&self.seq.to_be_bytes());
        buf[8..16].copy_from_slice(&self.timestamp_ns.to_be_bytes());
    }

    #[must_use]
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut buf = [0_u8; HEADER_LEN];
        self.write_into(&mut buf);
        buf
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(anyhow!(
                "frame too short for header: {} < {HEADER_LEN}",
                bytes.len(),
            ));
        }
        let version = bytes[0];
        if version != PROTOCOL_VERSION {
            return Err(anyhow!(
                "unsupported protocol version {version:#04x} (expected {PROTOCOL_VERSION:#04x})",
            ));
        }
        let payload_type = PayloadType::from_byte(bytes[1])
            .ok_or_else(|| anyhow!("unknown payload_type {:#04x}", bytes[1]))?;
        let stream_id = u16::from_be_bytes([bytes[2], bytes[3]]);
        let seq = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let timestamp_ns = u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        Ok(Self {
            version,
            payload_type,
            stream_id,
            seq,
            timestamp_ns,
        })
    }
}

/// Encode a frame into a freshly allocated `Vec<u8>`.
#[must_use]
pub fn encode(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    out
}

/// Append a frame to an existing buffer — reuse across loop iterations to
/// avoid per-frame allocation on the hot path.
pub fn encode_into(header: &FrameHeader, payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
}

/// Decode a frame into its header and a slice pointing at the payload.
pub fn decode(bytes: &[u8]) -> Result<(FrameHeader, &[u8])> {
    let header = FrameHeader::parse(bytes)?;
    Ok((header, &bytes[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::{
        decode, encode, encode_into, FrameHeader, PayloadType, CONTROL_STREAM, FFT_STREAM,
        HEADER_LEN, PROTOCOL_VERSION,
    };

    #[test]
    fn header_round_trip() {
        let h = FrameHeader {
            version: PROTOCOL_VERSION,
            payload_type: PayloadType::IqF32,
            stream_id: 0x1234,
            seq: 0xDEAD_BEEF,
            timestamp_ns: 0x0123_4567_89AB_CDEF,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        let parsed = FrameHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn wire_order_is_big_endian() {
        // Sanity check: a recognisable bit pattern lands at the expected
        // offsets. Catches accidental LE↔BE swaps during refactors.
        let h = FrameHeader {
            version: 0x01,
            payload_type: PayloadType::FftU8,
            stream_id: 0x00AB,
            seq: 0x0001_0203,
            timestamp_ns: 0x0102_0304_0506_0708,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..2], &[0x01, 0x01]);
        assert_eq!(&b[2..4], &[0x00, 0xAB]);
        assert_eq!(&b[4..8], &[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(&b[8..16], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn frame_round_trip_with_payload() {
        let h = FrameHeader::new(PayloadType::FftU8, FFT_STREAM);
        let payload = vec![0, 64, 128, 192, 255];
        let frame = encode(&h, &payload);
        assert_eq!(frame.len(), HEADER_LEN + payload.len());
        let (parsed_h, parsed_p) = decode(&frame).unwrap();
        assert_eq!(parsed_h, h);
        assert_eq!(parsed_p, payload.as_slice());
    }

    #[test]
    fn encode_into_appends_multiple() {
        let mut buf = Vec::new();
        let a = FrameHeader::new(PayloadType::JsonEvent, CONTROL_STREAM);
        let b = FrameHeader::new(PayloadType::FftU8, FFT_STREAM);
        encode_into(&a, b"{\"type\":\"ping\"}", &mut buf);
        encode_into(&b, &[7, 8, 9], &mut buf);
        // Two frames back-to-back with their payloads intact.
        assert_eq!(buf.len(), HEADER_LEN * 2 + 15 + 3);
        let (h0, p0) = decode(&buf[..HEADER_LEN + 15]).unwrap();
        assert_eq!(h0.payload_type, PayloadType::JsonEvent);
        assert_eq!(p0, b"{\"type\":\"ping\"}");
        let (h1, p1) = decode(&buf[HEADER_LEN + 15..]).unwrap();
        assert_eq!(h1.payload_type, PayloadType::FftU8);
        assert_eq!(p1, &[7, 8, 9]);
    }

    #[test]
    fn rejects_short_frame() {
        assert!(FrameHeader::parse(&[0_u8; HEADER_LEN - 1]).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let mut b = FrameHeader::new(PayloadType::FftU8, FFT_STREAM).to_bytes();
        b[0] = 0x02;
        assert!(FrameHeader::parse(&b).is_err());
    }

    #[test]
    fn rejects_unknown_payload_type() {
        let mut b = FrameHeader::new(PayloadType::FftU8, FFT_STREAM).to_bytes();
        b[1] = 0x42;
        assert!(FrameHeader::parse(&b).is_err());
    }

    #[test]
    fn payload_type_round_trip() {
        for pt in [
            PayloadType::FftU8,
            PayloadType::FftF16,
            PayloadType::IqS16,
            PayloadType::IqF32,
            PayloadType::AudioF32,
            PayloadType::JsonEvent,
            PayloadType::JsonControl,
            PayloadType::Error,
        ] {
            assert_eq!(PayloadType::from_byte(pt.as_byte()), Some(pt));
        }
    }
}
