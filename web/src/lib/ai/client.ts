// WebSocket connector for the ferrite-ai sidecar. The browser only
// ever talks to ferrited's port — ferrited reverse-proxies `/ws/chat`
// to the sidecar (default `ws://127.0.0.1:10002/ws/chat`, override via
// `FERRITE_AI_URL` on ferrited). In dev, vite proxies `/ws` to
// ferrited (see web/vite.config.ts), so the same same-origin URL
// works in both dev and prod. Single-port from the browser's POV.
//
// If the sidecar isn't running, ferrited closes the WS after sending
// a `ferrite_ai_error` envelope — the chat store renders that in the
// red-banner path so the user sees a clean "ferrite-ai offline"
// instead of an opaque connection drop.

import { ai } from './store.svelte';
import { wsUrlFor } from '$lib/api/errors';

export function aiWsUrl(): string {
  return wsUrlFor('/ws/chat');
}

export function connectAi(): () => void {
  ai.connect(aiWsUrl());
  return () => ai.disconnect();
}
