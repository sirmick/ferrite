// WebSocket binary frame decoder — thin TS wrapper over the Rust
// `decodeFrame` wasm-bindgen export in `ferrite-blocks`.
//
// The wire format is a postcard-serialized `Frame` enum defined in
// `blocks/src/frame.rs`; both the server and this decoder import that
// schema, so version drift is a build break rather than a silent
// mis-decode. `initFrameDecoder()` must resolve before the first
// `decodeFrame()` call — `SessionStore.open()` awaits it.

import initWasm, { decodeFrame as wasmDecodeFrame } from '../wasm/blocks/ferrite_blocks';

export const CONTROL_STREAM = 0;
export const FFT_STREAM = 1;
export const VFO_STREAM_BASE = 2;

export const PayloadType = {
  FftU8: 0x01,
  IqF32: 0x11,
  JsonEvent: 0x80,
} as const;

export type PayloadTypeValue = (typeof PayloadType)[keyof typeof PayloadType];

export interface FrameHeader {
  readonly payloadType: PayloadTypeValue;
  readonly streamId: number;
  readonly seq: number;
  readonly timestampNs: bigint;
}

export interface ParsedFrame {
  readonly header: FrameHeader;
  /** Payload bytes — a view owned by the caller; safe to hold. */
  readonly payload: Uint8Array;
}

export class FrameDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'FrameDecodeError';
  }
}

let initPromise: Promise<void> | undefined;
let ready = false;

/**
 * Idempotent WASM initialization. Safe to call concurrently; the first
 * caller kicks off the fetch, later callers await the same promise.
 */
export async function initFrameDecoder(): Promise<void> {
  if (!initPromise) {
    initPromise = (async () => {
      await initWasm();
      ready = true;
    })();
  }
  await initPromise;
}

interface WasmFrame {
  payload_type: number;
  stream_id: number;
  seq: number;
  timestamp_ns: bigint;
  payload: Uint8Array;
}

export function decodeFrame(input: ArrayBuffer | Uint8Array): ParsedFrame {
  if (!ready) {
    throw new FrameDecodeError(
      'frame decoder not initialized — await initFrameDecoder() before decoding',
    );
  }
  const u8 = input instanceof Uint8Array ? input : new Uint8Array(input);
  let out: WasmFrame;
  try {
    out = wasmDecodeFrame(u8) as WasmFrame;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    throw new FrameDecodeError(msg);
  }
  return {
    header: {
      payloadType: out.payload_type as PayloadTypeValue,
      streamId: out.stream_id,
      seq: out.seq,
      timestampNs: out.timestamp_ns,
    },
    payload: out.payload,
  };
}
