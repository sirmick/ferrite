// Connect the AI panel to the ferrite-ai sidecar. The browser only ever
// talks to ferrited's port: ferrited proxies `/api/ai/*` to the sidecar
// (default `http://127.0.0.1:10002`, override via `FERRITE_AI_URL` on
// ferrited), and vite proxies `/api` to ferrited in dev. Control is
// plain REST; the conversation arrives on the event plane (`ai` kind).

import { ai } from './store.svelte';

export function connectAi(): () => void {
  ai.connect('/api/ai');
  return () => ai.disconnect();
}
