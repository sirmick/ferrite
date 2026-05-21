// Module-level store for *purely-visual* chrome state — the left-panel
// tab, channel-detail visibility, per-pane zoom + pause. The browser is
// the **author**; ferrited only caches what we push so `GET /api/view`
// (and downstream MCP resources) can answer without round-tripping.
//
// Why module-level rather than `$state` in `+page.svelte`: two consumers
// need the same source of truth — `+page.svelte` (the tab bar reads and
// writes it), and `ViewBridge.svelte` (the cross-process pusher reads
// it to ship the cache to ferrited on every change). Splitting into a
// store keeps both pinned to one snapshot and lets future writers
// (`view-set-tab` command, commit 2) reach in from anywhere.
//
// Scope rule: only put things here that another process might want to
// **read or write** (the AI, ferrite-ctl, MCP clients). Per-tab ephemera
// — scroll positions, hover state, cursor — stays local to its component.

export type LeftTab = 'bands' | 'catalog' | 'settings' | 'logs' | 'flow' | 'ai';

export type PaneName =
  | 'wide-spectrum'
  | 'wide-waterfall'
  | 'channel-spectrum'
  | 'channel-waterfall';

// `$state` reactivity needs class fields, not bare let bindings, so a
// `LeftTab | null` field is straightforward. The bridge reads through
// getters (which transparently re-track), and writers call setters that
// just assign — Svelte's effect graph handles the rest.
class ViewStateStore {
  // `null` is the "not yet known" sentinel — preserved across the wire
  // as `Option<String>` on the Rust side. The producer (page.svelte)
  // sets it once on mount; before then it's null so we don't ship a
  // bogus default to ferrited.
  leftTab = $state<LeftTab | null>(null);
  channelDetailVisible = $state<boolean | null>(null);
  // Per-pane maps; absent key = browser hasn't announced this pane's
  // state. Plain objects (not Maps) so reactivity tracks key-level
  // additions — Svelte 5's `$state` deep-tracks property writes.
  paneZoom = $state<Record<string, number>>({});
  panePaused = $state<Record<string, boolean>>({});

  /** Reactive snapshot for the bridge to ship. Reads every field, so
   *  any change re-fires the bridge's `$effect`. The serializer drops
   *  null fields by omitting them — matches the Rust struct's
   *  `Option<...>` / `HashMap::is_empty` skip rules. */
  snapshot(): ViewStateSnapshot {
    const out: ViewStateSnapshot = {};
    if (this.leftTab !== null) out.left_tab = this.leftTab;
    if (this.channelDetailVisible !== null) {
      out.channel_detail_visible = this.channelDetailVisible;
    }
    if (Object.keys(this.paneZoom).length > 0) out.pane_zoom = { ...this.paneZoom };
    if (Object.keys(this.panePaused).length > 0) {
      out.pane_paused = { ...this.panePaused };
    }
    return out;
  }
}

/** Wire shape — must match `UiViewState` on the Rust side. */
export interface ViewStateSnapshot {
  left_tab?: LeftTab;
  channel_detail_visible?: boolean;
  pane_zoom?: Record<string, number>;
  pane_paused?: Record<string, boolean>;
}

export const viewState = new ViewStateStore();
