// Tiny shared store for the waterfall's display knobs.
//
// `paused` used to live as `$state` inside Waterfall.svelte, but the
// pause button now lives in DisplayControls (separate component), so
// the state moves out here so both ends share one source of truth.
// In-memory only — bringing the page back up paused would look broken.
// `resetMaxHold` is a function slot that Spectrum.svelte fills in on
// mount with a renderer-bound implementation; DisplayControls calls it.

class WaterfallStore {
  paused = $state(false);
  resetMaxHold: () => void = () => {};

  // Shared auto-contrast bounds published by the wide waterfall and
  // consumed by the narrow one, so both panes stretch their palette to
  // the same byte window. Without this, each renderer computes its own
  // P5/P98 from its own byte stream — the narrow FFT has finer bins
  // (lower noise per bin) so its noise floor sits at a different byte
  // value and the colours diverge for the same dB signal. `null` =
  // no publisher yet (e.g. cold start or autoContrast off);
  // consumers fall back to their own internal detector.
  sharedAutoFloor01 = $state<number | null>(null);
  sharedAutoCeil01 = $state<number | null>(null);
}

export const waterfallStore = new WaterfallStore();
