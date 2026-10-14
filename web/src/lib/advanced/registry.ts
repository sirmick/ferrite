// Registry of mode-specific "advanced" workspace views.
//
// Each entry maps a runtime UI-sink name (`ui:<sink>`) to the
// component that replaces the wide FFT/waterfall column when the
// operator toggles "Advanced". The toolbar button is enabled — and
// labelled — only when the active preset advertises a registered
// sink, so the button is self-documenting ("FT8", later "ADS-B")
// and absent on presets with no advanced view.
//
// Adding a view is one line here plus the component + its `ui:<sink>`
// store/wire. FT8/FT4/WSPR share one component (Ft8View); ADS-B will
// be a second entry.
//
// Most views key off a `ui:<sink>` stream. A few don't: transcription
// is fed by the Worker (no `ui:` stream), so it supplies an explicit
// `present()` predicate instead — availability is "the browser runtime
// has a VoiceTranscribe tap" (`browserRuntime.voiceTranscribeIds`),
// true on every audio preset (the block is auto-injected before each
// AudioSink). That tap list is the only client-readable signal: the
// block is server-injected then env-split to the browser, so it's in
// neither `pipeline.flowgraph` nor `pipeline.blocks` (node-only).

import type { Component } from 'svelte';
import Ft8View from './Ft8View.svelte';
import AdsbView from './AdsbView.svelte';
import AprsView from './AprsView.svelte';
import FldigiView from './FldigiView.svelte';
import JournalView from './JournalView.svelte';
import TranscriptView from './TranscriptView.svelte';
import CallLogView from './CallLogView.svelte';
import {
  VIEW_SPECS,
  defaultRightOf,
  paneViewsOf,
  resolveOf,
  type AdvancedCtx,
  type Pane,
  type ViewSpec,
} from './viewSpecs';

export type { AdvancedCtx, Pane } from './viewSpecs';

export interface AdvancedView extends ViewSpec {
  // Views take different prop shapes (most none, the Journal a `kind`);
  // the workspace spreads `props` into whichever it mounts.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  component: Component<any>;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function componentFor(id: string): Component<any> {
  if (id.startsWith('journal:')) return JournalView;
  switch (id) {
    case 'ft8':
      return Ft8View;
    case 'adsb':
      return AdsbView;
    case 'aprs':
      return AprsView;
    case 'fldigi':
      return FldigiView;
    case 'calls:dsd':
      return CallLogView;
    case 'transcript':
      return TranscriptView;
    default:
      throw new Error(`advanced registry: no component for view ${id}`);
  }
}

export const ADVANCED_VIEWS: AdvancedView[] = VIEW_SPECS.map((s) => ({
  ...s,
  component: componentFor(s.id),
}));

/** Every view the active preset offers in `pane`, in registry order. */
export function paneViews(ctx: AdvancedCtx, pane: Pane): AdvancedView[] {
  return paneViewsOf(ADVANCED_VIEWS, ctx, pane);
}

/** The view a pane value selects, or null — see `resolveOf`. */
export function resolvePaneView(ctx: AdvancedCtx, pane: Pane, value: string): AdvancedView | null {
  return resolveOf(ADVANCED_VIEWS, ctx, pane, value);
}

/** The right-pane view a preset selects on load, if it has one. */
export function defaultRightView(ctx: AdvancedCtx): AdvancedView | null {
  return defaultRightOf(ADVANCED_VIEWS, ctx);
}

/** The preset's first left-pane view, or null (kept for callers that
 *  predate panes). */
export function activeAdvancedView(ctx: AdvancedCtx): AdvancedView | null {
  return paneViews(ctx, 'left')[0] ?? null;
}
