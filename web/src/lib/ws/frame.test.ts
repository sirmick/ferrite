import { describe, expect, it } from 'vitest';
import {
  CONTROL_STREAM,
  FFT_STREAM,
  FrameDecodeError,
  HEADER_LEN,
  PROTOCOL_VERSION,
  PayloadType,
  decodeFrame,
} from './frame';

function buildFrame(opts: {
  version?: number;
  payloadType?: number;
  streamId?: number;
  seq?: number;
  timestampNs?: bigint;
  payload?: Uint8Array;
}): Uint8Array {
  const payload = opts.payload ?? new Uint8Array();
  const buf = new Uint8Array(HEADER_LEN + payload.length);
  const dv = new DataView(buf.buffer);
  dv.setUint8(0, opts.version ?? PROTOCOL_VERSION);
  dv.setUint8(1, opts.payloadType ?? PayloadType.FftU8);
  dv.setUint16(2, opts.streamId ?? FFT_STREAM, false);
  dv.setUint32(4, opts.seq ?? 0, false);
  dv.setBigUint64(8, opts.timestampNs ?? 0n, false);
  buf.set(payload, HEADER_LEN);
  return buf;
}

describe('decodeFrame', () => {
  it('parses a fft_u8 frame with payload', () => {
    const payload = new Uint8Array([1, 2, 3, 4]);
    const bytes = buildFrame({
      payloadType: PayloadType.FftU8,
      streamId: FFT_STREAM,
      seq: 0xdead_beef,
      timestampNs: 0x0123_4567_89ab_cdefn,
      payload,
    });
    const { header, payload: out } = decodeFrame(bytes);
    expect(header.version).toBe(PROTOCOL_VERSION);
    expect(header.payloadType).toBe(PayloadType.FftU8);
    expect(header.streamId).toBe(FFT_STREAM);
    expect(header.seq).toBe(0xdead_beef);
    expect(header.timestampNs).toBe(0x0123_4567_89ab_cdefn);
    expect(Array.from(out)).toEqual([1, 2, 3, 4]);
  });

  it('accepts json_event on the control stream', () => {
    const text = new TextEncoder().encode('{"type":"warning"}');
    const bytes = buildFrame({
      payloadType: PayloadType.JsonEvent,
      streamId: CONTROL_STREAM,
      payload: text,
    });
    const { header, payload } = decodeFrame(bytes);
    expect(header.payloadType).toBe(PayloadType.JsonEvent);
    expect(header.streamId).toBe(CONTROL_STREAM);
    expect(new TextDecoder().decode(payload)).toBe('{"type":"warning"}');
  });

  it('rejects too-short bytes', () => {
    expect(() => decodeFrame(new Uint8Array(8))).toThrow(FrameDecodeError);
  });

  it('rejects unsupported version', () => {
    expect(() => decodeFrame(buildFrame({ version: 0x02 }))).toThrow(/version/);
  });

  it('rejects unknown payload_type', () => {
    expect(() => decodeFrame(buildFrame({ payloadType: 0x42 }))).toThrow(/payload_type/);
  });

  it('accepts an ArrayBuffer input', () => {
    const bytes = buildFrame({});
    const ab = bytes.buffer as ArrayBuffer;
    const { header } = decodeFrame(ab);
    expect(header.version).toBe(PROTOCOL_VERSION);
  });
});
