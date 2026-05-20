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
import TranscriptView from './TranscriptView.svelte';

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

export interface AdvancedView {
  /** UI-sink name the runtime advertises (`pipeline.uiSinks[sink]`)
   *  when a preset using this view is active. Also the stable identity
   *  key for views with a custom `present` predicate. */
  sink: string;
  /** Toolbar button label — mode-named, not a generic "Advanced". */
  label: string;
  component: Component;
  /** Optional availability override. When set, decides presence
   *  instead of the default `uiSinks[sink]` lookup — for views whose
   *  data doesn't arrive via a `ui:<sink>` stream. */
  present?: (ctx: AdvancedCtx) => boolean;
}

export const ADVANCED_VIEWS: AdvancedView[] = [
  // FT8, FT4 and WSPR all wire `ui:ft8` and share Ft8View.
  { sink: 'ft8', label: 'FT8 / FT4 / WSPR', component: Ft8View },
  // ADS-B (Mode-S, 1090 MHz) — live aircraft map + table.
  { sink: 'adsb', label: 'ADS-B', component: AdsbView },
  // APRS (AX.25 packet) — station map + table + packet console.
  { sink: 'aprs', label: 'APRS', component: AprsView },
  // fldigi family (RTTY/PSK31/CW/MT63/Olivia/…/RSID) — RX-text Journal.
  // `sink` stays 'fldigi' (the ui:fldigi wire key); the user-facing
  // label is "Journal" — the streaming decoded-text view that every
  // streaming-digital preset falls into.
  { sink: 'fldigi', label: 'Journal', component: FldigiView },
  // Speech-to-text. Listed last so a mode-specific decoder view wins
  // if both ever co-exist. Advertised whenever the active preset has
  // an `AudioSink` (i.e. could be transcribed), NOT only when the
  // VoiceTranscribe block is already present — the toolbar dropdown
  // flips `profile.transcribe` on the moment the user picks this,
  // which re-composes and injects the tap. That keeps the
  // "every audio preset has an advanced view" promise without paying
  // the whisper-worker cost up front.
  {
    sink: 'transcribe',
    label: 'Transcript',
    component: TranscriptView,
    present: (ctx) => ctx.hasAudioSink,
  },
];

/** The advanced view for the active preset, or null when none match. */
export function activeAdvancedView(ctx: AdvancedCtx): AdvancedView | null {
  return (
    ADVANCED_VIEWS.find((v) => (v.present ? v.present(ctx) : ctx.uiSinks[v.sink] !== undefined)) ??
    null
  );
}
