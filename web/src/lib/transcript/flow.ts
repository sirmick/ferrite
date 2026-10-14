// Transcript layout model — the pure half of the reusable transcript
// widget (TranscriptFlow.svelte). Every place that shows speech as text
// (the analog-voice Transcript, the digital-voice Transcript, the AI
// panel's compact cards) normalises its records to `TLine` and lets
// `buildFlow` decide the shape:
//
//   - **Pauses.** Speech flows as prose: lines from one speaker join on
//     the same line; only a real pause (≥ PARA_MS) starts a paragraph,
//     and a long quiet stretch gets a time separator ("— 3 min —").
//     Recognisers close a segment only after ~1.2 s of trailing silence
//     (sherpa's endpoint), so a record boundary is *not* a sentence
//     break by itself — breaking on it put every phrase on its own line.
//   - **Known speakers form blocks.** Consecutive lines from one speaker
//     (same key and talkgroup) merge into one block with a single
//     coloured label; no divider needed between them.
//   - **Unknown speaker changes** (a diarisation turn with no identity)
//     get a neutral "speaker change" divider.
//
// Pure and unit-tested; the component only renders the result.

/** Who said a line. Assigned server-side (server/src/speakers.rs) for
 *  digital voice; absent for analog voice. */
export interface Speaker {
  key: string;
  label: string;
  id?: string;
  /** `radio` (digital voice ID), `diarised` (voice cluster). */
  source?: string;
  /** Talkgroup (or channel) the line was spoken on. */
  group?: string;
  /** Server-assigned palette index, stable across tabs and views. */
  colour?: number;
}

/** The newest label per speaker key, read from the `dsd` log (oldest →
 *  newest). A talker alias often arrives a few overs after a radio's
 *  first call, so its early records carry the bare ID; every record
 *  stamped after the alias carries the name, so the latest one wins and
 *  views relabel the earlier lines with it. */
export function latestLabels(
  records: ReadonlyArray<{ data: unknown }>,
): ReadonlyMap<string, string> {
  const out = new Map<string, string>();
  for (const r of records) {
    const sp = (r.data as { speaker?: Partial<Speaker> } | null)?.speaker;
    if (typeof sp?.key === 'string' && typeof sp.label === 'string') out.set(sp.key, sp.label);
  }
  return out;
}

/** `s` with its label brought up to date from `labels`. */
export function relabel<S extends Speaker | undefined>(
  s: S,
  labels: ReadonlyMap<string, string>,
): S {
  const label = s && labels.get(s.key);
  return s && label && label !== s.label ? { ...s, label } : s;
}

export interface TLine {
  id: string | number;
  /** Wall clock when the line closed (ms since epoch). */
  atMs: number;
  text: string;
  /** Silence before this line, ms (0 = continuous). */
  gapMs: number;
  /** Utterance duration in seconds (t1 − t0), when known. */
  dur?: number;
  tokens?: ReadonlyArray<{ text: string; p: number }>;
  rawText?: string;
  confidence?: number;
  vfoHz?: number | null;
  lane?: string;
  speaker?: Speaker;
  /** Diarisation turn flag (no identity) — draws the neutral divider. */
  speakerTurn?: boolean;
  /** `placeholder` = encrypted / no speech recognised: shown as a stub. */
  kind?: 'speech' | 'placeholder';
}

export type Sep = 'space' | 'para';

export interface Block {
  type: 'block';
  key: string;
  speaker?: Speaker;
  startMs: number;
  lines: Array<{ line: TLine; sep: Sep }>;
}
export type FlowItem =
  | Block
  | { type: 'turn'; key: string }
  | { type: 'gap'; key: string; label: string };

/** At or above this, a new paragraph (a real pause within an over).
 *  Above the recogniser's ~1.2 s endpoint silence, so ordinary
 *  segment boundaries keep flowing on the same line. */
export const PARA_MS = 3_000;
/** At or above this between lines, a time separator. */
export const GAP_MS = 30_000;

export function sepFor(gapMs: number): Sep {
  return gapMs >= PARA_MS ? 'para' : 'space';
}

export function gapLabel(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 90) return `— ${s} s —`;
  const m = Math.round(s / 60);
  if (m < 90) return `— ${m} min —`;
  return `— ${Math.round(m / 60)} h —`;
}

/** Silence between two lines: the line's own `gapMs`, or — when the
 *  producer doesn't track it across utterances — the wall-clock gap
 *  less the later line's duration. Without a duration the wall clock
 *  includes the speaking time, so it only counts for long quiet
 *  stretches (≥ GAP_MS), never for line/paragraph grading. */
function silenceBetween(prev: TLine, line: TLine): number {
  const diff = line.atMs - prev.atMs;
  const wall = line.dur !== undefined ? diff - line.dur * 1000 : diff >= GAP_MS ? diff : 0;
  return Math.max(line.gapMs, wall, 0);
}

const speakerKey = (l: TLine) => (l.speaker ? `${l.speaker.key}|${l.speaker.group ?? ''}` : '');

export function buildFlow(lines: ReadonlyArray<TLine>): FlowItem[] {
  const out: FlowItem[] = [];
  let block: Block | undefined;
  let prev: TLine | undefined;
  for (const line of lines) {
    const silence = prev ? silenceBetween(prev, line) : 0;
    if (prev && silence >= GAP_MS) {
      out.push({ type: 'gap', key: `gap-${line.id}`, label: gapLabel(silence) });
      block = undefined;
    }
    const sk = speakerKey(line);
    const sameSpeaker = block !== undefined && speakerKey(block.lines[0].line) === sk;
    if (!sameSpeaker || (line.speakerTurn && !line.speaker)) {
      if (block && line.speakerTurn && !line.speaker) {
        out.push({ type: 'turn', key: `turn-${line.id}` });
      }
      block = {
        type: 'block',
        key: `b-${line.id}`,
        speaker: line.speaker,
        startMs: line.atMs,
        lines: [],
      };
      out.push(block);
      block.lines.push({ line, sep: 'space' });
    } else {
      block!.lines.push({ line, sep: sepFor(silence) });
    }
    prev = line;
  }
  return out;
}

/** Plain-text rendering (copy / export / AI text): speaker label and
 *  talkgroup in brackets, paragraphs as blank lines. */
export function flowToText(items: ReadonlyArray<FlowItem>, time: (ms: number) => string): string {
  const parts: string[] = [];
  for (const it of items) {
    if (it.type === 'gap') parts.push(it.label);
    else if (it.type === 'turn') parts.push('— speaker change —');
    else {
      const who = it.speaker
        ? `[${it.speaker.label}${it.speaker.group ? ` TG${it.speaker.group}` : ''}] `
        : '';
      let body = '';
      for (const { line, sep } of it.lines) {
        const t = line.kind === 'placeholder' ? `(${line.text})` : line.text;
        body += body ? (sep === 'para' ? `\n\n${t}` : ` ${t}`) : t;
      }
      parts.push(`${time(it.startMs)} ${who}${body}`);
    }
  }
  return parts.join('\n\n');
}
