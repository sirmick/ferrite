<script lang="ts">
  // Browser side of the `/api/ui-views/:pane` snapshot path.
  //
  // Opens `/ws/ui-views`, listens for `{type: "view_request", req_id,
  // pane}` from ferrited, looks the pane up in `viewRegistry`, calls
  // the registered snapshot fn, sends back `{req_id, png_b64, error}`.
  //
  // The HTTP side (`GET /api/ui-views/:pane`) waits on a oneshot keyed
  // by `req_id`; we don't care which req maps to which request, just
  // that we round-trip the id verbatim.
  //
  // Single-listener policy: ferrited's `ViewBridge::install_viewer`
  // replaces the previous subscriber on every connect. If two tabs are
  // open, the last to connect wins — matches D06 across the rest of
  // the WS surface (preset, logs).
  //
  // Mounted once at the Workspace level; the registry is module-global,
  // so the four canvas components register themselves independently
  // wherever they live in the tree.
  import { onMount, onDestroy } from 'svelte';
  import { snapshotView, type PaneName } from '$lib/viz/viewRegistry';
  import { viewState } from '$lib/viz/viewState.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { logs } from '$lib/logs/store.svelte';

  // Same-origin WS — vite proxies `/ws` to ferrited (HTTP dev cert
  // also covers WSS), production mounts ferrited under the same host.
  function wsUrl(): string {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${location.host}/ws/ui-views`;
  }

  type WireRequest = { type: 'view_request'; req_id: number; pane: PaneName };
  /** Server → browser chrome-state push (`POST /api/ui-view/set`). The
   *  fields are optional; the browser writes only the ones supplied.
   *  Source: `server/src/view_bridge.rs:UiViewStatePatch`. */
  type WireSetViewState = {
    type: 'set_view_state';
    state: {
      main_pane?: 'wide' | 'advanced' | string;
      channel_detail_visible?: boolean;
      left_tab?: string;
    };
  };
  type WireInbound = WireRequest | WireSetViewState;

  /** Apply a server-requested chrome-state patch. Each field maps to
   *  the matching `client.workspace.*` control store key; we route
   *  through `applyControl` so the same dispatcher path the UI uses
   *  validates + persists the value. Unknown fields are logged + dropped
   *  instead of silently swallowed — surfaces server-side bugs. */
  function applyViewStatePatch(state: WireSetViewState['state']): void {
    if (state.main_pane !== undefined) {
      if (state.main_pane === 'wide' || state.main_pane === 'advanced') {
        void applyControl('client.workspace.mainPane', state.main_pane);
      } else {
        logs.push('client', 'warn', `view-bridge: unknown main_pane=${state.main_pane}`);
      }
    }
    if (state.channel_detail_visible !== undefined) {
      void applyControl('client.workspace.narrowVisible', state.channel_detail_visible);
    }
    // `left_tab` arrives over the wire (server side carries it in
    // UiViewStatePatch) but isn't applied yet — the left-side tab
    // selector doesn't live in a single client-store key the way
    // mainPane / narrowVisible do. Drop silently rather than warn.
  }

  let ws: WebSocket | undefined;
  // Reconnect with exponential backoff so a transient ferrited
  // restart doesn't permanently disable the AI's view-grab path.
  let backoffMs = 500;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  // Track whether the socket is open so the `$effect` below can guard
  // its sends. Plain WebSocket events fire imperatively; this `$state`
  // surfaces "is the channel up" to the reactive graph.
  let wsOpen = $state(false);

  function connect(): void {
    if (closed) return;
    try {
      ws = new WebSocket(wsUrl());
    } catch {
      schedule();
      return;
    }
    ws.addEventListener('open', () => {
      backoffMs = 500;
      wsOpen = true;
    });
    ws.addEventListener('message', (ev) => {
      // Only text frames carry outbound messages; binaries are unexpected.
      if (typeof ev.data !== 'string') return;
      let msg: WireInbound;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (msg.type === 'view_request' && typeof msg.req_id === 'number') {
        const { png_b64, error } = snapshotView(msg.pane);
        // Tag the reply so the server-side `WireInbound` enum dispatches
        // it as a `view_response` (vs. `view_state`).
        const resp = JSON.stringify({
          type: 'view_response',
          req_id: msg.req_id,
          png_b64,
          error,
        });
        if (ws && ws.readyState === WebSocket.OPEN) ws.send(resp);
        return;
      }
      if (msg.type === 'set_view_state') {
        applyViewStatePatch(msg.state);
      }
    });
    ws.addEventListener('close', () => {
      ws = undefined;
      wsOpen = false;
      schedule();
    });
    ws.addEventListener('error', () => {
      // close fires too; let that branch reconnect.
    });
  }

  // Browser is the author for chrome state — push the cache to ferrited
  // on every change. Reading `viewState.snapshot()` re-tracks every
  // field; any flip (tab switch, zoom change, pause toggle…) re-fires
  // this effect and ships a fresh state. The cheap stringify is
  // ferrited's only window into "what is the operator looking at?"
  $effect(() => {
    const snapshot = viewState.snapshot();
    if (!wsOpen || !ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify({ type: 'view_state', state: snapshot }));
  });

  function schedule(): void {
    if (closed) return;
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined;
      backoffMs = Math.min(backoffMs * 2, 5000);
      connect();
    }, backoffMs);
  }

  onMount(() => {
    connect();
  });

  onDestroy(() => {
    closed = true;
    if (reconnectTimer) clearTimeout(reconnectTimer);
    ws?.close();
  });
</script>

<!-- Headless bridge — no DOM. -->
