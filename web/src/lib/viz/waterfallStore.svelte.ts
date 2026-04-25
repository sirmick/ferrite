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
}

export const waterfallStore = new WaterfallStore();
