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
