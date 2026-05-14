// GET /api/ui-sinks — enumerate the `ui:<name>` sinks in the current
// preset and the stream_id env_split allocated for each. The client uses
// this to subscribe to the right frame stream without reimplementing the
// allocator. Payload type tells the consumer which decoder to use.

import { ApiError } from '$lib/api/errors';

export type UiSinkPayload = 'IqF32' | 'F32' | 'FftU8' | 'JsonEvent';

export interface UiSink {
  name: string;
  stream_id: number;
  payload_type: UiSinkPayload;
}

export async function fetchUiSinks(): Promise<UiSink[]> {
  const r = await fetch('/api/ui-sinks');
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `ui-sinks fetch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as UiSink[];
}
