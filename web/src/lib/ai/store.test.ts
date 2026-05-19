// Contract tests for the conversation-state single-authority fold.
// The sidecar is the authority; this store is a pure view whose
// localStorage is a first-paint cache the authoritative snapshot
// overwrites. These pin: (1) a `conversation_snapshot` *replaces* a
// stale cached transcript and adopts the session id, reconstructing
// turns through the existing reducer (incl. `ferrite_ai_user_turn`);
// (2) a `session_reset` clears coherently with one honest banner.

import { beforeEach, describe, expect, it } from 'vitest';

import { AiStore } from './store.svelte';

const LS_TURNS = 'ferrite-ai.turns';
const LS_SESSION = 'ferrite-ai.session_id';

/** Attach a fake OPEN socket so the unified reset path takes its
 *  online branch (sends a `reset_session` control instead of the
 *  offline local-banner fallback). */
function attachFakeSocket(store: AiStore): { sent: string[] } {
  const sent: string[] = [];
  (store as unknown as { ws: unknown }).ws = {
    readyState: WebSocket.OPEN,
    send: (m: string) => sent.push(m),
  };
  return { sent };
}

beforeEach(() => {
  localStorage.clear();
});

describe('conversation-state single authority', () => {
  it('snapshot replaces the stale localStorage cache and adopts the session id', () => {
    // First-paint cache: a stale prior transcript + session id.
    localStorage.setItem(
      LS_TURNS,
      JSON.stringify([{ id: 1, role: 'user', text: 'STALE cached question', t: 1 }]),
    );
    localStorage.setItem(LS_SESSION, 'old-session');

    const store = new AiStore();
    expect(store.turns).toHaveLength(1);
    expect((store.turns[0] as { text: string }).text).toBe('STALE cached question');

    // Authoritative replay from the sidecar — same reducer, replace.
    store.ingestEvent({
      type: 'conversation_snapshot',
      session_id: 'sess-A',
      events: [
        { type: 'ferrite_ai_user_turn', text: 'real question', t: 111 },
        {
          type: 'stream_event',
          event: {
            type: 'content_block_delta',
            delta: { type: 'text_delta', text: 'real answer' },
          },
        },
        { type: 'ferrite_ai_done', mode: 'explorer' },
      ],
    });

    expect(store.turns).toHaveLength(2);
    const [u, a] = store.turns;
    expect(u.role).toBe('user');
    expect((u as { text: string }).text).toBe('real question');
    expect(a.role).toBe('assistant');
    const asst = a as { status: string; chunks: Array<{ kind: string; text?: string }> };
    expect(asst.status).toBe('complete');
    expect(asst.chunks).toEqual([{ kind: 'text', text: 'real answer' }]);
    expect(store.sessionId).toBe('sess-A');
  });

  it('session_reset preserves visible history, drops the binding, appends one banner', () => {
    const store = new AiStore();
    // A real prior conversation the user can still see.
    store.ingestEvent({ type: 'ferrite_ai_user_turn', text: 'q', t: 1 });
    const before = store.turns.length;
    expect(before).toBeGreaterThan(0);
    store.sessionId = 'doomed';

    store.ingestEvent({ type: 'session_reset', reason: 'clear' });

    // History is the user's record — it must NOT be wiped (that looked
    // like the panel vanished). Binding dropped + one honest banner.
    expect(store.sessionId).toBeNull();
    expect(store.turns).toHaveLength(before + 1);
    const banner = store.turns[store.turns.length - 1] as {
      role: string;
      chunks: Array<{ kind: string; label?: string }>;
    };
    expect(banner.role).toBe('assistant');
    expect(banner.chunks).toHaveLength(1);
    expect(banner.chunks[0].kind).toBe('meta');
    expect(banner.chunks[0].label).toContain('reasoning context reset');
    expect(banner.chunks[0].label).toContain('history preserved');
    expect(banner.chunks[0].label).toContain('clear');

    // Reconnect storms must not stack banners.
    store.ingestEvent({ type: 'session_reset', reason: 'clear' });
    expect(store.turns).toHaveLength(before + 1);
  });

  it('an empty snapshot just clears (no prior turns survive)', () => {
    localStorage.setItem(LS_TURNS, JSON.stringify([{ id: 1, role: 'user', text: 'stale', t: 1 }]));
    const store = new AiStore();
    expect(store.turns).toHaveLength(1);

    store.ingestEvent({ type: 'conversation_snapshot', session_id: 'sess-B', events: [] });

    expect(store.turns).toHaveLength(0);
    expect(store.sessionId).toBe('sess-B');
  });

  it('/reset, Clear, setMode all drop the sidecar binding (unified path)', () => {
    // Online: every reset entry point must send the authoritative
    // `reset_session` control — nulling only the browser id would
    // leave the sidecar resuming the old context (the bug).
    const store = new AiStore();
    const { sent } = attachFakeSocket(store);
    store.sessionId = 'live';

    store.resetSession({ reason: 'reset' });
    store.clear();
    store.setMode('decoder');

    const reasons = sent
      .map((m) => JSON.parse(m))
      .filter((m) => m.type === 'reset_session')
      .map((m) => m.reason as string);
    expect(reasons).toHaveLength(3);
    expect(reasons[0]).toBe('reset');
    expect(reasons[1]).toBe('clear');
    expect(reasons[2]).toContain('decoder');
    expect(store.sessionId).toBeNull();
  });

  it('/reset offline keeps history, drops binding, appends one banner', () => {
    // No socket → local fallback. The visible transcript is the
    // user's record; it must survive (this is the regression that
    // made the panel look gone).
    const store = new AiStore();
    store.ingestEvent({ type: 'ferrite_ai_user_turn', text: 'q', t: 1 });
    const before = store.turns.length;
    store.sessionId = 'doomed';

    store.resetSession({ reason: 'reset' });

    expect(store.sessionId).toBeNull();
    expect(store.turns).toHaveLength(before + 1);
    const banner = store.turns[store.turns.length - 1] as {
      chunks: Array<{ kind: string; label?: string }>;
    };
    expect(banner.chunks[0].kind).toBe('meta');
    expect(banner.chunks[0].label).toContain('reasoning context reset');
    expect(banner.chunks[0].label).toContain('reset');
  });
});
