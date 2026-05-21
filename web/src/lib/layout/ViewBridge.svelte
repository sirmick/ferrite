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

  // Same-origin WS — vite proxies `/ws` to ferrited (HTTP dev cert
  // also covers WSS), production mounts ferrited under the same host.
  function wsUrl(): string {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${location.host}/ws/ui-views`;
  }

  type WireRequest = { type: 'view_request'; req_id: number; pane: PaneName };

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
      // Only text frames carry view_request; binaries are unexpected.
      if (typeof ev.data !== 'string') return;
      let req: WireRequest;
      try {
        req = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (req.type !== 'view_request' || typeof req.req_id !== 'number') return;
      const { png_b64, error } = snapshotView(req.pane);
      // Tag the reply so the server-side `WireInbound` enum dispatches
      // it as a `view_response` (vs. `view_state`, vs. future
      // `view_command_result`).
      const resp = JSON.stringify({
        type: 'view_response',
        req_id: req.req_id,
        png_b64,
        error,
      });
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(resp);
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
