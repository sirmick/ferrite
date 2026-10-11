// Shapes of the server's `analysis` records (server/src/analysis.rs,
// crate ferrite-analysis). Loosely typed: the panel draws what's there.

export interface Peak {
  freq_hz: number;
  avg_db: number;
  peak_db: number;
  duty: number;
  width_hz: number;
  keyed: boolean;
}

export interface SnapshotInfo {
  id: string;
  center_hz: number;
  rate_hz: number;
  start_ms: number;
  secs: number;
  tapped: string;
}

export interface AnalysisRecord {
  id: string;
  tool: 'spectrum' | 'spectrogram' | 'envelope' | string;
  by: 'ui' | 'cli' | 'mcp' | string;
  snapshot?: SnapshotInfo;
  params?: Record<string, number>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  result?: any;
  text?: string;
  cells_b64?: string;
}

/** base64 → bytes (spectrogram cells, u8 per cell). */
export function decodeCells(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** 0..255 → a dark-blue → cyan → yellow → white heat colour. */
export function heat(v: number): [number, number, number] {
  const t = v / 255;
  if (t < 0.33) {
    const k = t / 0.33;
    return [0, Math.round(20 + 80 * k), Math.round(40 + 140 * k)];
  }
  if (t < 0.66) {
    const k = (t - 0.33) / 0.33;
    return [Math.round(200 * k), Math.round(100 + 120 * k), Math.round(180 - 140 * k)];
  }
  const k = (t - 0.66) / 0.34;
  return [Math.round(200 + 55 * k), Math.round(220 + 35 * k), Math.round(40 + 215 * k)];
}
