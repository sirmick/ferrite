// Module-level registry of canvas snapshotters keyed by pane name.
//
// `ferrite-ctl view <pane>` → ferrited → `/ws/ui-views` →
// `ViewBridge.svelte` → this registry → calls the matching
// snapshot fn → base64 PNG → back across the WS → ferrited returns
// to ferrite-ctl as `image/png`.
//
// Each viz component (`Spectrum`, `Waterfall`, `NarrowSpectrum`,
// `NarrowWaterfall`) registers a snapshot fn on mount and unregisters
// on destroy. The fn returns base64 PNG (no `data:image/png;base64,`
// prefix — just the payload), which the bridge wraps into the
// `view_response` envelope.

export type PaneName =
  | 'wide-spectrum'
  | 'wide-waterfall'
  | 'channel-spectrum'
  | 'channel-waterfall';

/** Sync snapshot — `canvas.toDataURL('image/png')` style. Returns the
 *  base64 payload without the `data:image/png;base64,` prefix. Throws
 *  if the canvas isn't drawable (zero-size, detached, etc.). */
export type SnapshotFn = () => string;

const REGISTRY = new Map<PaneName, SnapshotFn>();

export function registerView(pane: PaneName, fn: SnapshotFn): void {
  REGISTRY.set(pane, fn);
}

export function unregisterView(pane: PaneName, fn: SnapshotFn): void {
  // Guard against a stale unregister overwriting a fresh registration
  // (e.g. component remount races). Only drop the entry if it's still
  // the same function pointer we installed.
  if (REGISTRY.get(pane) === fn) REGISTRY.delete(pane);
}

export function snapshotView(pane: PaneName): { png_b64: string; error: string } {
  const fn = REGISTRY.get(pane);
  if (!fn) {
    return {
      png_b64: '',
      error: `no canvas registered for pane "${pane}" — preset may lack a Channelizer (for channel-*) or the UI hasn't mounted yet`,
    };
  }
  try {
    return { png_b64: fn(), error: '' };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return { png_b64: '', error: msg };
  }
}

/** Strip the `data:image/png;base64,` prefix off a `canvas.toDataURL()`
 *  result. Helper for the four viz components so they don't all
 *  re-implement the slice. */
export function dataUrlToBase64(dataUrl: string): string {
  const comma = dataUrl.indexOf(',');
  return comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
}
