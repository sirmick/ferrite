// The decode Journal: one text line per record of a text-producing
// decoder kind (pager, EAS, rtl_433, …), read straight out of the
// decoder mirror. The fldigi Journal (`$lib/fldigi/store.svelte`) is
// the same idea for the keyboard modes; this one covers every other
// append-log kind whose records carry a message, so a preset that
// wires `ui:<kind>` gets a main-pane view of its decodes instead of
// falling through to the voice Transcript.
//
// `journalLine` is pure (record data → text) so it stays unit-tested;
// `journalLines` is the reactive read the view renders.

import { decoders } from '$lib/decoders/store.svelte';
import type { ConsoleLine } from '$lib/viz/TextConsole.svelte';

type Rec = Record<string, unknown>;

const str = (v: unknown): string | undefined => (typeof v === 'string' ? v : undefined);
const num = (v: unknown): number | undefined => (typeof v === 'number' ? v : undefined);

/** `{proto, protocol, capcode|address, format, frag, message, text}`
 *  (blocks/src/pager.rs + decode_parse.rs) →
 *  `FLEX 1234567 K  From: EVS@kp.org …`. Tone-only pages say so; a
 *  record the parser didn't recognise shows its raw line. */
function pagerLine(d: Rec): string {
  const proto = str(d.protocol) ?? str(d.proto);
  const id = num(d.capcode) ?? num(d.address);
  const format = str(d.format);
  const message = str(d.message);
  if (!proto || message === undefined) return str(d.text) ?? JSON.stringify(d);
  const head = [proto.replace('FLEX_NEXT', 'FLEX'), id !== undefined ? String(id) : '-'];
  const frag = str(d.frag);
  if (frag && frag !== 'K') head.push(frag === 'C' ? 'part 1' : 'cont.');
  if (format === 'tone') return `${head.join(' ')}  (tone only)`;
  if (format === 'numeric') head.push('num');
  if (format === 'unknown' || format === 'binary') head.push(format);
  return `${head.join(' ')}  ${message}`;
}

/** EAS/SAME header (decode_parse.rs `parse_same`) → one line. */
function easLine(d: Rec): string {
  if (d.type === 'end_of_message') return 'EAS end of message';
  const event = str(d.event_name) ?? str(d.event);
  if (!event) return str(d.text) ?? JSON.stringify(d);
  const parts = [`EAS ${event}`];
  const from = str(d.originator_name) ?? str(d.originator);
  if (from) parts.push(from);
  const sender = str(d.sender);
  if (sender) parts.push(sender);
  const locs = Array.isArray(d.locations) ? d.locations.filter((l) => typeof l === 'string') : [];
  if (locs.length) parts.push(`areas ${locs.join(',')}`);
  const purge = num(d.purge_minutes);
  if (purge !== undefined) parts.push(`${purge} min`);
  return parts.join(' · ');
}

/** The text for one record of `kind`. Kinds without a formatter show
 *  their `text` / `message` field, else the record compactly. */
export function journalLine(kind: string, data: unknown): string {
  const d = data && typeof data === 'object' && !Array.isArray(data) ? (data as Rec) : null;
  if (!d) return typeof data === 'string' ? data : JSON.stringify(data);
  switch (kind) {
    case 'pager':
      return pagerLine(d);
    case 'eas':
      return easLine(d);
    default: {
      const t = str(d.text) ?? str(d.message);
      return t ?? JSON.stringify(d);
    }
  }
}

/** The kind's append log as console lines, newest last. */
export function journalLines(kind: string): ConsoleLine[] {
  const out: ConsoleLine[] = [];
  for (const r of decoders.kind(kind).recent) {
    const text = journalLine(kind, r.data);
    if (!text) continue;
    out.push({ id: String(r.seq), ts: Math.floor(r.at_ms / 1000), text });
  }
  return out;
}

/** Operator Reset — clears the kind server-side; the mirror follows. */
export function resetJournal(kind: string): void {
  void fetch(`/api/state/${kind}/reset`, { method: 'POST' }).catch(() => {});
}
