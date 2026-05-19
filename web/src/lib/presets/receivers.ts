// Receiver recipes — what each radio-mode option does to the live
// flowgraph when the user picks it from the receivers pane.
//
// A recipe replaces the `demod` block (type + params) in the currently-
// applied FlowgraphDoc. The source block stays byte-identical so the
// reconfigure plan stays local to the browser half; no retune, no
// Soapy restart.

import type { FlowgraphDoc } from '$lib/flowgraph';

export type ReceiverId = 'fm' | 'nbfm' | 'am' | 'usb' | 'lsb';

export interface ReceiverRecipe {
  id: ReceiverId;
  label: string;
  blockType: string;
  params: Record<string, unknown>;
}

export const RECEIVERS: readonly ReceiverRecipe[] = [
  {
    id: 'fm',
    label: 'FM broadcast',
    blockType: 'FmDemod',
    params: { sample_rate_hz: 48000, max_deviation_hz: 75000 },
  },
  {
    id: 'nbfm',
    label: 'NBFM voice',
    blockType: 'FmDemod',
    params: { sample_rate_hz: 48000, max_deviation_hz: 5000 },
  },
  {
    id: 'am',
    label: 'AM broadcast',
    blockType: 'AmDemod',
    params: { sample_rate_hz: 48000, bias_tau_ms: 100 },
  },
  {
    id: 'usb',
    label: 'USB (upper sideband)',
    blockType: 'SsbDemod',
    params: { sample_rate_hz: 48000, sideband: 'usb', audio_gain: 30.0 },
  },
  {
    id: 'lsb',
    label: 'LSB (lower sideband)',
    blockType: 'SsbDemod',
    params: { sample_rate_hz: 48000, sideband: 'lsb', audio_gain: 30.0 },
  },
];

export function findRecipe(id: ReceiverId): ReceiverRecipe {
  const r = RECEIVERS.find((x) => x.id === id);
  if (!r) throw new Error(`unknown receiver id: ${id}`);
  return r;
}

/** Map a Bands-panel entry's decorative `mode` chip to the canonical
 *  receiver **preset slug** that demodulates it end-to-end, or `null`
 *  when no audio receiver applies (data-only signals: GMSK/AIS, ADS-B
 *  pulse, ISM OOK/FSK, or an unrecognised mode).
 *
 *  Why a preset slug and not an in-place `demod` swap: a recipe only
 *  rewrites the `demod` block, leaving the channelizer width and the
 *  downstream chain (RDS, decim, audio rates) shaped for whatever
 *  preset happened to be loaded — so a cross-mode swap tunes but
 *  produces broken or silent audio. The per-mode presets ship a
 *  coherent chain, so loading one is the only reliable "set receiver".
 *
 *  FT8 / FT4 / WSPR / APRS get their own `mode` in `bands.json` and
 *  map to the per-mode decoding presets — those ship a coherent
 *  demod chain *plus* the decoder and its advanced-view sink
 *  (`ui:ft8` / `ui:aprs`), so `+RX` on one of those band entries
 *  lands you on a working decoder + map/table, not bare audio. (APRS
 *  → `packet`, which is NBFM Bell-202; the others are USB.) The
 *  remaining HF data modes (RTTY, PSK31, Olivia, …) are still
 *  authored as `USB` and land on the generic SSB receiver — decoding
 *  those stays a Signal-Catalog/fldigi concern for now. Unknown modes
 *  return `null` rather than guessing a preset and serving the wrong
 *  audio. */
export function presetSlugForMode(mode: string | undefined): string | null {
  switch ((mode ?? '').toUpperCase()) {
    case 'WFM':
      return 'wbfm';
    case 'NBFM':
      return 'nbfm';
    case 'AM':
      return 'wbam';
    case 'USB':
      return 'usb';
    case 'LSB':
      return 'lsb';
    case 'CW':
      return 'cw';
    // FT8/FT4 folded into one variant-family preset (ft8.json); the
    // base isn't loadable bare (hard cut) — address the variants.
    case 'FT8':
      return 'ft8-ft8';
    case 'FT4':
      return 'ft8-ft4';
    case 'WSPR':
      return 'wspr';
    case 'APRS':
      return 'packet';
    default:
      return null;
  }
}

/** Given the live preset and a chosen recipe, build the doc to PATCH.
 * Returns `null` when the doc has no `demod` block (not a receivers-
 * style preset). Everything except the demod block stays verbatim — in
 * particular the source block is byte-identical, so the runtime's diff
 * plan contains only the demod swap (and any downstream re-init). */
export function applyRecipe(doc: FlowgraphDoc, recipe: ReceiverRecipe): FlowgraphDoc | null {
  const blocks = doc.blocks ?? {};
  const existing = blocks.demod;
  if (!existing) return null;
  const next = {
    ...blocks,
    demod: {
      type: recipe.blockType,
      ...(existing.placement ? { placement: existing.placement } : {}),
      params: recipe.params,
    },
  };
  return { ...doc, blocks: next } as FlowgraphDoc;
}

/** Guess which receiver id matches the live preset's demod block.
 * For FmDemod we also check `max_deviation_hz` so wide-FM and NBFM —
 * which share a block type — can be told apart. For SsbDemod we check
 * `sideband` to split USB vs LSB. */
export function detectReceiver(doc: FlowgraphDoc | null): ReceiverId | null {
  const demod = doc?.blocks?.demod;
  if (!demod) return null;
  if (demod.type === 'FmDemod') {
    const dev = (demod.params as { max_deviation_hz?: number } | undefined)?.max_deviation_hz;
    return dev != null && dev <= 25000 ? 'nbfm' : 'fm';
  }
  if (demod.type === 'SsbDemod') {
    const sb = (demod.params as { sideband?: string } | undefined)?.sideband;
    return sb === 'lsb' ? 'lsb' : 'usb';
  }
  return RECEIVERS.find((r) => r.blockType === demod.type)?.id ?? null;
}
