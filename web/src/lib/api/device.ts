// REST helpers for `POST /api/device/open` and `POST /api/device/:id/close`.
//
// Mirrors the response shapes from `server/src/routes.rs`. Field names are
// snake_case to match the wire format (the server uses serde defaults).

export interface OpenDeviceRequest {
  sample_rate_hz?: number;
  center_freq_hz?: number;
  tone_freq_abs_hz?: number;
  amplitude?: number;
  fft_size?: number;
  fft_rate_hz?: number;
  floor_dbfs?: number;
  ceil_dbfs?: number;
  alpha?: number;
  /** SoapySDR device args (e.g. `driver=rtlsdr,serial=00000001`). */
  device_args?: string;
  antenna?: string;
  gain_db?: number;
  agc?: boolean;
  bandwidth_hz?: number;
}

export interface StreamDescriptor {
  stream_id: number;
  payload_type: string;
  size: number;
  rate_hz: number;
}

export interface OpenDeviceResponse {
  session_id: string;
  ws_url: string;
  fft: StreamDescriptor;
}

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

export async function openDevice(req: OpenDeviceRequest = {}): Promise<OpenDeviceResponse> {
  const r = await fetch('/api/device/open', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(req),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `open failed (${r.status}): ${text}`);
  }
  return (await r.json()) as OpenDeviceResponse;
}

export async function closeDevice(sessionId: string): Promise<void> {
  const r = await fetch(`/api/device/${encodeURIComponent(sessionId)}/close`, { method: 'POST' });
  // 404 is not exceptional during teardown — the session may have already
  // been evicted by a fresh open().
  if (!r.ok && r.status !== 404) {
    throw new ApiError(r.status, `close failed (${r.status})`);
  }
}

/** Build a same-origin `ws://` or `wss://` URL for a given REST `ws_url`. */
export function wsUrlFor(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}${path}`;
}
