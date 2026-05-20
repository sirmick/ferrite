// Client-only control store — one JSON blob in localStorage, keyed by
// canonical `client.<scope>.<param>` path. Every knob whose effect
// stays on the browser (display toggles, worklet gain, layout hints)
// routes through here; the server never sees it.
//
// Why one blob instead of one key per control: it's trivial to dump
// the whole state in devtools, easy to wipe (`localStorage.removeItem`),
// and the write cost is a single stringify per change. Small enough
// that per-key keys would just be ceremony.
//
// Reads are reactive — components can `$derived(clientControls.get(...))`
// and re-render on changes. Writes go through `.set(path, value)` (or
// via `applyControl` once the dispatcher lands).

const STORAGE_KEY = 'ferrite.clientControls';

/** Which view occupies the workspace's main column (the toolbar's
 *  Main: dropdown). Exported so the dispatcher / consumers can refer
 *  to the union by name. */
export type WorkspaceMainPane = 'wide' | 'advanced';

/** Full list of known client paths and their defaults. The defaults
 *  object also declares the *type* of each knob (by its JavaScript
 *  value shape); anything persisted that doesn't match is dropped on
 *  load. Adding a control is a single line here. */
export const CLIENT_DEFAULTS = {
  // Spectrum plot flags
  'client.spectrum.fade': true,
  'client.spectrum.maxHold': false,
  'client.spectrum.autoScale': false,
  'client.spectrum.bandPlan': true,
  // Display-only horizontal zoom / pan applied to the FFT and waterfall
  // panes. The server still streams full-span FFT rows; these knobs only
  // control which slice the renderers paint. `viewZoom` is a linear
  // magnification factor (1 = full span, 16 = 1/16th of the span shown
  // edge-to-edge); `viewPan` is the fractional position of the view's
  // *center* within the available pan range — 0 = view flush to the
  // left edge of the full span, 1 = flush right, 0.5 = centred.
  'client.spectrum.viewZoom': 1,
  'client.spectrum.viewPan': 0.5,
  // Client-side display range for the FFT line + waterfall colormap.
  // Server quantises to a fixed [−160, 0] dBFS window; the display
  // range is a purely visual slice on top of those bytes — the
  // backend never sees these values. Default −140/−20 leaves
  // headroom on both ends so a typical broadcast or HF signal
  // sits comfortably mid-graph without immediately needing a
  // manual or auto-scale adjustment.
  'client.spectrum.displayFloorDbfs': -140,
  'client.spectrum.displayCeilDbfs': -20,

  // Waterfall contrast knobs. When `autoContrast` is on, the renderer
  // computes P5/P98 of recent FFT rows and stretches that to the full
  // palette (noise floor in dark blue, strong carriers in bright
  // white — self-adjusts as you tune across bands). When off, the
  // manual `contrastFloorDbfs` / `contrastCeilDbfs` window is used
  // directly. Same dBFS units as the spectrum range, on the same
  // [−160, 0] server quantisation surface — so a slider feels
  // familiar.
  'client.waterfall.autoContrast': true,
  'client.waterfall.contrastFloorDbfs': -110,
  'client.waterfall.contrastCeilDbfs': -30,

  // Tuning step / snap, one control. `'off'` = free tuning (arrows
  // still step by the band-natural amount, but no grid snap); `'auto'`
  // = band-plan-derived step + snap; a fixed step in Hz as a string
  // ('1000', '12500', '200000', …) = that raster + snap. String-typed
  // so the one-blob type-guard on load stays simple.
  'client.tuning.stepMode': 'auto',

  // Audio playback
  'client.audio.volume': 1.0,
  'client.audio.muted': false,
  // Noise-reduction preset for `audio_nr` — see presets/nrPresets.ts.
  // 'auto' = leave the preset's authored NR untouched (default).
  // Re-applied to the browser block after every flowgraph (re)load so
  // it survives the transcribe-triggered re-compose.
  'client.audio.nrPreset': 'auto',

  // Workspace layout — when the active preset has a Channelizer the
  // runtime auto-injects a `ui:fft_narrow` tap (see
  // `inject_narrow_fft.rs`), which the workspace renders as a second
  // Spectrum+Waterfall column to the right of the wide view. The user
  // can collapse that column to reclaim the screen real estate; the
  // setting persists. Has no effect on bareband presets — the column
  // is hidden automatically when no `fft_narrow` sink is present.
  'client.workspace.narrowVisible': true,

  // What occupies the main (wide) workspace column: `"wide"` shows the
  // wide Spectrum+Waterfall; `"advanced"` shows the active preset's
  // mode-specific view (FT8 spots / ADS-B map / APRS map / Journal /
  // Transcript — whatever `activeAdvancedView` returns). The toolbar
  // exposes this as a two-option dropdown. Defaults to `"wide"` and
  // resets to `"wide"` on every preset load (`pipeline.loadPreset`)
  // so a stale "advanced" selection from a previous preset never
  // surprises you on the next one.
  'client.workspace.mainPane': 'wide' as WorkspaceMainPane,

  // Operator's own Maidenhead grid locator (e.g. "FN42" / "IO91wm").
  // Not in any decode stream — it's the origin of the station-map
  // contact lines and the "home" marker. Empty = unknown; the map
  // then just shows decoded stations with no home/lines.
  'client.station.grid': '',

  // whisper `initial_prompt` override for the VoiceTranscribe worker.
  // Empty = use the built-in dense ham corpus (hamPrompt.ts). Edited
  // from the Transcript tab; fanned out live to the worker by
  // `applyControl` (see dispatch.ts), same pattern as audio volume.
  'client.transcribe.prompt': '',
} as const;

export type ClientPath = keyof typeof CLIENT_DEFAULTS;
export type ClientValue<P extends ClientPath> = (typeof CLIENT_DEFAULTS)[P];

class ClientControlStore {
  /** Single reactive backing state. Indexed by path. */
  private state = $state<Record<string, unknown>>({ ...CLIENT_DEFAULTS });

  constructor() {
    // Late-bound so SSR doesn't choke on the localStorage reference.
    if (typeof localStorage === 'undefined') return;
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return;
    try {
      const parsed = JSON.parse(raw) as Record<string, unknown>;
      for (const [k, v] of Object.entries(parsed)) {
        // Only restore keys we still recognise and whose type matches
        // the default. Lets us rename/remove a knob without blowing up
        // on stored state from an older build.
        if (!(k in CLIENT_DEFAULTS)) continue;
        const def = CLIENT_DEFAULTS[k as ClientPath];
        if (typeof v !== typeof def) continue;
        // `typeof NaN === 'number'`, so a poisoned non-finite number
        // (e.g. a bad viewZoom from a division-by-zero) would pass the
        // type check and persist across reloads — and downstream it can
        // make the spectrum renderer's grid loop never terminate
        // (frozen tab). Drop non-finite numbers; fall back to default.
        if (typeof v === 'number' && !Number.isFinite(v)) continue;
        this.state[k] = v;
      }
    } catch {
      /* corrupt blob; fall back to defaults */
    }
  }

  /** Reactive read. Narrow at the call site. */
  get<P extends ClientPath>(path: P): ClientValue<P> {
    return this.state[path] as ClientValue<P>;
  }

  /** Write and persist. No-op when the value is unchanged. */
  set<P extends ClientPath>(path: P, value: ClientValue<P>): void {
    // Never let a non-finite number into the store (would persist and
    // can wedge the spectrum renderer). Drop the write silently.
    if (typeof value === 'number' && !Number.isFinite(value)) return;
    if (this.state[path] === value) return;
    this.state[path] = value;
    this.persist();
  }

  /** Reset every key to its default and wipe localStorage. Handy for
   *  "reset layout" / test hooks. */
  reset(): void {
    this.state = { ...CLIENT_DEFAULTS };
    if (typeof localStorage !== 'undefined') localStorage.removeItem(STORAGE_KEY);
  }

  private persist(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(this.state));
    } catch {
      /* quota or private-mode — tolerate silently */
    }
  }
}

export const clientControls = new ClientControlStore();
