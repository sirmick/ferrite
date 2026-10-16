// Tool registry — the top-level "what is this screen for" switch.
//
// A *radio* tool owns the SDR: only one runs at a time, and which one is
// derived from server state (the loaded preset), so every tab and the AI
// agree. Receiver = any streaming preset; Spectrum = the stepped
// `panorama` preset (the server remembers the receiver to return to).
// *Desk* tools (inspector, logbook, schedule — later) are per-tab views
// over whatever radio tool runs; none exist yet.

import { pipeline } from '$lib/pipeline.svelte';
import type { LeftTab } from '$lib/viz/viewState.svelte';

export type ToolId = 'receiver' | 'spectrum';

export interface ToolDef {
  id: ToolId;
  label: string;
  kind: 'radio' | 'desk';
  /** Sidebar tabs this tool shows, in order. */
  sidebarTabs: LeftTab[];
  /** Tooltip for the switcher. */
  hint: string;
}

export const TOOLS: readonly ToolDef[] = [
  {
    id: 'receiver',
    label: 'Receiver',
    kind: 'radio',
    sidebarTabs: ['bands', 'catalog', 'settings', 'logs', 'flow', 'ai'],
    hint: 'Receiver — tune, demodulate, decode, listen',
  },
  {
    id: 'spectrum',
    label: 'Spectrum',
    kind: 'radio',
    sidebarTabs: ['bands', 'settings', 'logs', 'ai'],
    hint: 'Spectrum analyzer — sweep a range wider than one span',
  },
];

/** The preset the Spectrum tool runs. */
export const SPECTRUM_PRESET = 'panorama';

/** True when a preset doc's `src` placeholder asks for a stepped source. */
export function isStepped(fg: { blocks?: Record<string, { params?: unknown }> } | null): boolean {
  const p = fg?.blocks?.src?.params as Record<string, unknown> | undefined;
  return p?.source_kind === 'stepped';
}

class ToolState {
  /** Active radio tool, derived from the server's loaded preset. */
  active = $derived<ToolId>(isStepped(pipeline.flowgraph) ? 'spectrum' : 'receiver');
  /** A switch is in flight (disables the switcher). */
  switching = $state(false);

  def(id: ToolId = this.active): ToolDef {
    return TOOLS.find((t) => t.id === id) ?? TOOLS[0];
  }

  /** Switch radio tools. Spectrum loads the stepped preset (and starts
   *  the pipeline if it was stopped); Receiver asks the server to put
   *  back the receiver preset it stashed. */
  async enter(id: ToolId): Promise<void> {
    if (id === this.active || this.switching) return;
    this.switching = true;
    try {
      if (id === 'spectrum') {
        await pipeline.loadPreset(SPECTRUM_PRESET);
      } else {
        await pipeline.returnPreset();
      }
      if (pipeline.status !== 'running') await pipeline.start();
    } finally {
      this.switching = false;
    }
  }
}

export const tools = new ToolState();
