// WebSocket binary frame parser — mirror of `server/src/ws_frame.rs`.
//
// Wire format per `docs/02-protocol.md`: 16-byte header (version,
// payload_type, big-endian stream_id/seq/timestamp_ns) followed by an
// arbitrary payload. Keep the two implementations in lockstep — bumping
// `PROTOCOL_VERSION` here without bumping the server (or vice versa) is
// the kind of bug we want to scream about at runtime, not silently mangle.

export const PROTOCOL_VERSION = 0x01;
export const HEADER_LEN = 16;
export const CONTROL_STREAM = 0;
export const FFT_STREAM = 1;

export const PayloadType = {
  FftU8: 0x01,
  FftF16: 0x02,
  IqS16: 0x10,
  IqF32: 0x11,
  AudioF32: 0x20,
  JsonEvent: 0x80,
  JsonControl: 0x81,
  Error: 0xff,
} as const;

export type PayloadTypeValue = (typeof PayloadType)[keyof typeof PayloadType];

const KNOWN_PAYLOAD_TYPES = new Set<number>(Object.values(PayloadType));

export interface FrameHeader {
  readonly version: number;
  readonly payloadType: PayloadTypeValue;
  readonly streamId: number;
  readonly seq: number;
  readonly timestampNs: bigint;
}

export interface ParsedFrame {
  readonly header: FrameHeader;
  /** Payload bytes — a view into the source buffer; do NOT mutate. */
  readonly payload: Uint8Array;
}

export class FrameDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'FrameDecodeError';
  }
}

export function decodeFrame(input: ArrayBuffer | Uint8Array): ParsedFrame {
  const u8 = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (u8.byteLength < HEADER_LEN) {
    throw new FrameDecodeError(`frame too short: ${u8.byteLength} < ${HEADER_LEN}`);
  }
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const version = dv.getUint8(0);
  if (version !== PROTOCOL_VERSION) {
    throw new FrameDecodeError(
      `unsupported protocol version 0x${version.toString(16).padStart(2, '0')}`,
    );
  }
  const payloadByte = dv.getUint8(1);
  if (!KNOWN_PAYLOAD_TYPES.has(payloadByte)) {
    throw new FrameDecodeError(
      `unknown payload_type 0x${payloadByte.toString(16).padStart(2, '0')}`,
    );
  }
  const header: FrameHeader = {
    version,
    payloadType: payloadByte as PayloadTypeValue,
    streamId: dv.getUint16(2, false),
    seq: dv.getUint32(4, false),
    timestampNs: dv.getBigUint64(8, false),
  };
  return { header, payload: u8.subarray(HEADER_LEN) };
}
