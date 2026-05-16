// REST helper for `GET /api/captures` -- the sample browser behind the
// Source dialog's File tab. Mirrors `server::app_state::CaptureEntry`:
// the server walks `samples/`, classifies each clip (`kind`), and folds
// in the `<file>.json` sidecar so the picker can show *what* a sample
// is, not just its path.

export interface CaptureEntry {
  /** Absolute server path the source block opens. */
  path: string;
  /** `samples/`-relative path -- the fallback label. */
  rel: string;
  /** Sidecar `name`, if any -- the human label. */
  name: string | null;
  /** `"iq"` (-> FileIqSource/upmix) or `"audio"` (-> modulate). */
  kind: 'iq' | 'audio';
  sample_rate_hz: number | null;
  center_freq_hz: number | null;
  format: string | null;
  /** Sidecar `modulation` -- the am/fm/ssb carrier to replay audio on. */
  modulation: string | null;
}

export async function fetchCaptures(): Promise<CaptureEntry[]> {
  const r = await fetch('/api/captures');
  if (!r.ok) {
    throw new Error(`fetchCaptures failed: ${r.status} ${r.statusText}`);
  }
  return (await r.json()) as CaptureEntry[];
}
