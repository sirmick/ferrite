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

import type { Component } from 'svelte';
import Ft8View from './Ft8View.svelte';
import AdsbView from './AdsbView.svelte';
import AprsView from './AprsView.svelte';

export interface AdvancedView {
  /** UI-sink name the runtime advertises (`pipeline.uiSinks[sink]`)
   *  when a preset using this view is active. */
  sink: string;
  /** Toolbar button label — mode-named, not a generic "Advanced". */
  label: string;
  component: Component;
}

export const ADVANCED_VIEWS: AdvancedView[] = [
  // FT8, FT4 and WSPR all wire `ui:ft8` and share Ft8View.
  { sink: 'ft8', label: 'FT8 / WSPR', component: Ft8View },
  // ADS-B (Mode-S, 1090 MHz) — live aircraft map + table.
  { sink: 'adsb', label: 'ADS-B', component: AdsbView },
  // APRS (AX.25 packet) — station map + table + packet console.
  { sink: 'aprs', label: 'APRS', component: AprsView },
];

/** The advanced view for the active preset, or null when none of the
 *  registered sinks are present. */
export function activeAdvancedView(uiSinks: Record<string, unknown>): AdvancedView | null {
  return ADVANCED_VIEWS.find((v) => uiSinks[v.sink] !== undefined) ?? null;
}
