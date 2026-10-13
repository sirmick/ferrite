// Running Rx text for the fldigi Journal.
//
// fldigi-family blocks emit one `fldigi` record per decoded *character*
// (per word only for CW), so the per-record console shows a vertical
// column of single letters — unreadable for a NAVTEX forecast or an RTTY
// over. This joins consecutive records into one continuous, wrapping
// text buffer (fldigi's own Rx pane), kept *alongside* the record list.
//
// Pure: `(cursor, records) → (cursor, appended text)` so it is unit-
// tested; the store owns the buffer and the reactive read. The buffer is
// accumulated incrementally so it is not bounded by the mirror's
// 512-record ring (≈ 512 characters for a per-char mode); it has its own
// character cap instead.

import type { DecoderRecord } from '$lib/decoders/store.svelte';

/** Where the accumulator has read up to: the last record folded in. */
export interface TextCursor {
  epoch: number;
  seq: number;
  /** `at_ms` of the last folded record (for the gap break). */
  atMs: number;
  /** Mode label of the last folded record (for the mode break). */
  mode: string;
}

export const EMPTY_CURSOR: TextCursor = { epoch: -1, seq: -1, atMs: 0, mode: '' };

/** A silence this long between characters starts a new paragraph —
 *  one NAVTEX slot / RTTY over per block, instead of them running
 *  together on one line. */
export const GAP_BREAK_MS = 20_000;

/** Keep at most this many characters; older text is dropped from the
 *  front at a line boundary. */
export const TEXT_CAP = 32_768;

interface FldigiRec {
  mode?: string;
  text?: string;
}

const isNewer = (r: DecoderRecord, c: TextCursor): boolean => {
  const epoch = r.epoch ?? 0;
  return epoch > c.epoch || (epoch === c.epoch && r.seq > c.seq);
};

/** Fold every record newer than `cursor` into `buffer`. Returns the new
 *  buffer and cursor; the inputs are not mutated. `\r` is dropped,
 *  `\n` is kept, a gap of `GAP_BREAK_MS` or a mode change forces a line
 *  break (a blank line for the gap). */
export function foldRunningText(
  buffer: string,
  cursor: TextCursor,
  records: readonly DecoderRecord[],
): { buffer: string; cursor: TextCursor } {
  let out = buffer;
  let cur = cursor;
  for (const r of records) {
    if (!isNewer(r, cur)) continue;
    const d = r.data as FldigiRec | undefined;
    const text = typeof d?.text === 'string' ? d.text.replace(/\r/g, '') : '';
    const mode = typeof d?.mode === 'string' ? d.mode : cur.mode;
    if (text) {
      if (out.length > 0) {
        const gap = cur.atMs > 0 && r.at_ms - cur.atMs >= GAP_BREAK_MS;
        const modeChange = cur.mode !== '' && mode !== cur.mode;
        if (gap) out = `${out.replace(/\n*$/, '')}\n\n`;
        else if (modeChange && !out.endsWith('\n')) out += '\n';
      }
      out += text;
    }
    cur = { epoch: r.epoch ?? 0, seq: r.seq, atMs: r.at_ms, mode };
  }
  if (out.length > TEXT_CAP) {
    const cut = out.indexOf('\n', out.length - TEXT_CAP);
    out = cut >= 0 ? out.slice(cut + 1) : out.slice(out.length - TEXT_CAP);
  }
  return { buffer: out, cursor: cur };
}
