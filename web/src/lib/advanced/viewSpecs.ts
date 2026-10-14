// Workspace view metadata and pane rules — the pure half of the
// advanced-view registry (no Svelte component imports, so the rules are
// unit-testable). `registry.ts` attaches a component to each spec by id.

/** Which workspace column a view belongs in. Opinionated by width: the
 *  wide **left** column holds what you mainly watch or read (waterfall,
 *  transcript, a decoder whose output *is* the content); the narrow
 *  **right** column holds companions — the channel, the signal list,
 *  short one-line event logs like digital-voice calls. Each toolbar
 *  dropdown lists only its own pane's views. */
export type Pane = 'left' | 'right';

/** Inputs the availability check gets — a uiSink map plus the browser
 *  runtime's VoiceTranscribe tap ids (some views aren't fed by a
 *  `ui:` stream). */
export interface AdvancedCtx {
  uiSinks: Record<string, unknown>;
  voiceTranscribeIds: ReadonlyArray<string>;
  /** Whether the active preset declares an `AudioSink` block — i.e.
   *  is a voice/audio preset that *could* be transcribed. Lets the
   *  Transcript entry advertise itself before transcribe is actually
   *  on (the toolbar flips `profile.transcribe` the moment the user
   *  picks it). */
  hasAudioSink: boolean;
}

export interface ViewSpec {
  /** Stable id stored in `client.workspace.mainPane` / `rightPane` and
   *  accepted by the AI's `view_set` (`transcript`, `calls:dsd`, …). */
  id: string;
  pane: Pane;
  /** Right-pane views only: selected automatically when a preset that
   *  has it loads (DMR → the call log beside the waterfall/transcript). */
  defaultRight?: boolean;
  /** UI-sink name the runtime advertises (`pipeline.uiSinks[sink]`)
   *  when a preset using this view is active. */
  sink: string;
  /** Dropdown label — mode-named, not a generic "Advanced". */
  label: string;
  /** Props the workspace passes when mounting the component — lets
   *  one generic view (the decode Journal) serve several kinds. */
  props?: Record<string, unknown>;
  /** Optional availability override. When set, decides presence
   *  instead of the default `uiSinks[sink]` lookup — for views whose
   *  data doesn't arrive via a `ui:<sink>` stream. */
  present?: (ctx: AdvancedCtx) => boolean;
}

/** A decode Journal for a text-producing append-log kind: the preset
 *  wires `ui:<kind>`, the view lists its records as text lines. */
function journal(sink: string, label: string): ViewSpec {
  return { id: `journal:${sink}`, pane: 'left', sink, label, props: { kind: sink, label } };
}

export const VIEW_SPECS: ViewSpec[] = [
  // FT8, FT4 and WSPR all wire `ui:ft8` and share Ft8View.
  { id: 'ft8', pane: 'left', sink: 'ft8', label: 'FT8 / FT4 / WSPR' },
  // ADS-B (Mode-S, 1090 MHz) — live aircraft map + table.
  { id: 'adsb', pane: 'left', sink: 'adsb', label: 'ADS-B' },
  // APRS (AX.25 packet) — station map + table + packet console.
  { id: 'aprs', pane: 'left', sink: 'aprs', label: 'APRS' },
  // fldigi family (RTTY/PSK31/CW/MT63/Olivia/…/RSID) — RX-text Journal.
  { id: 'fldigi', pane: 'left', sink: 'fldigi', label: 'Journal' },
  // Text decoders that are not keyboard modes: one Journal each, so the
  // pager / NWR / rtl_433 presets (which also have an AudioSink, for
  // listening) show their decodes rather than the voice Transcript.
  journal('pager', 'Pager'),
  journal('eas', 'EAS'),
  journal('rtl_433', 'rtl_433'),
  // Digital-voice calls (DmrDecode / DsdHelper): one short line per
  // call — a right-pane companion to the Transcript, not the content.
  {
    id: 'calls:dsd',
    pane: 'right',
    defaultRight: true,
    sink: 'dsd',
    label: 'Calls',
    props: { kind: 'dsd', label: 'Calls' },
  },
  // Speech-to-text. Advertised whenever the preset has an AudioSink (it
  // *could* be transcribed); picking it turns transcription on. Offered
  // only when no *left-pane decoder view* applies (see `paneViewsOf`):
  // a pager / rtl_433 / fldigi preset keeps an AudioSink for listening,
  // but its decoder is the content. Digital voice qualifies — its call
  // log is a right-pane view — so DMR gets Transcript on the left.
  {
    id: 'transcript',
    pane: 'left',
    sink: 'transcribe',
    label: 'Transcript',
    present: (ctx) => ctx.hasAudioSink,
  },
];

function isPresent(v: ViewSpec, ctx: AdvancedCtx): boolean {
  return v.present ? v.present(ctx) : ctx.uiSinks[v.sink] !== undefined;
}

/** Every view in `list` the active preset offers in `pane`, in order. */
export function paneViewsOf<T extends ViewSpec>(
  list: ReadonlyArray<T>,
  ctx: AdvancedCtx,
  pane: Pane,
): T[] {
  const views = list.filter((v) => v.pane === pane && isPresent(v, ctx));
  if (pane === 'left' && views.some((v) => v.id !== 'transcript')) {
    return views.filter((v) => v.id !== 'transcript');
  }
  return views;
}

/** The view a pane value selects, or null (left: the wide FFT/waterfall;
 *  right: channel / signals / off). `advanced` (the pre-pane value,
 *  still sent by older callers) means the preset's first left view. */
export function resolveOf<T extends ViewSpec>(
  list: ReadonlyArray<T>,
  ctx: AdvancedCtx,
  pane: Pane,
  value: string,
): T | null {
  const views = paneViewsOf(list, ctx, pane);
  if (pane === 'left' && value === 'advanced') return views[0] ?? null;
  return views.find((v) => v.id === value) ?? null;
}

/** The right-pane view a preset selects on load, if it has one. */
export function defaultRightOf<T extends ViewSpec>(
  list: ReadonlyArray<T>,
  ctx: AdvancedCtx,
): T | null {
  return paneViewsOf(list, ctx, 'right').find((v) => v.defaultRight) ?? null;
}
