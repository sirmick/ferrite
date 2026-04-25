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

/** Full list of known client paths and their defaults. The defaults
 *  object also declares the *type* of each knob (by its JavaScript
 *  value shape); anything persisted that doesn't match is dropped on
 *  load. Adding a control is a single line here. */
export const CLIENT_DEFAULTS = {
  // Spectrum plot flags
  'client.spectrum.fade': true,
  'client.spectrum.maxHold': false,
  'client.spectrum.autoScale': false,
  // Client-side display range for the FFT line + waterfall colormap.
  // Server quantises to a fixed [−160, 0] dBFS window; the display
  // range is a purely visual slice on top of those bytes — the
  // backend never sees these values. Default −140/−20 leaves
  // headroom on both ends so a typical broadcast or HF signal
  // sits comfortably mid-graph without immediately needing a
  // manual or auto-scale adjustment.
  'client.spectrum.displayFloorDbfs': -140,
  'client.spectrum.displayCeilDbfs': -20,

  // Audio playback
  'client.audio.volume': 1.0,
  'client.audio.muted': false,
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
        if (typeof v === typeof def) this.state[k] = v;
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
