// WebSocket client for the server `/ws/logs` text stream. Reconnects with
// linear backoff so a dev-server restart doesn't require a page reload.

import { logs, pushServerLine } from './store.svelte';

export function connectServerLogs(): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let retry = 0;

  const url = (() => {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    // In dev, vite serves the page on :5173 but ferrited is on :8088.
    // Use the current host in production builds (served by ferrited itself).
    const host = location.port === '5173' ? `${location.hostname}:8088` : location.host;
    return `${proto}//${host}/ws/logs`;
  })();

  const open = () => {
    if (closed) return;
    ws = new WebSocket(url);
    ws.addEventListener('open', () => {
      retry = 0;
      logs.push('client', 'info', 'server logs connected');
    });
    ws.addEventListener('message', (e) => {
      if (typeof e.data === 'string') pushServerLine(e.data);
    });
    ws.addEventListener('close', () => {
      if (closed) return;
      const delay = Math.min(1000 + retry * 500, 5000);
      retry += 1;
      setTimeout(open, delay);
    });
    ws.addEventListener('error', () => {
      // `close` fires right after — reconnect logic lives there.
    });
  };

  open();
  return () => {
    closed = true;
    ws?.close();
  };
}
