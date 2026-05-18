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

  it('session_reset clears coherently with a single honest banner', () => {
    const store = new AiStore();
    store.ingestEvent({ type: 'ferrite_ai_user_turn', text: 'q', t: 1 });
    expect(store.turns.length).toBeGreaterThan(0);
    store.sessionId = 'doomed';

    store.ingestEvent({ type: 'session_reset', reason: 'clear' });

    expect(store.sessionId).toBeNull();
    expect(store.turns).toHaveLength(1);
    const only = store.turns[0] as {
      role: string;
      chunks: Array<{ kind: string; label?: string }>;
    };
    expect(only.role).toBe('assistant');
    expect(only.chunks).toHaveLength(1);
    expect(only.chunks[0].kind).toBe('meta');
    expect(only.chunks[0].label).toContain('reasoning context reset');
    expect(only.chunks[0].label).toContain('clear');
  });

  it('an empty snapshot just clears (no prior turns survive)', () => {
    localStorage.setItem(LS_TURNS, JSON.stringify([{ id: 1, role: 'user', text: 'stale', t: 1 }]));
    const store = new AiStore();
    expect(store.turns).toHaveLength(1);

    store.ingestEvent({ type: 'conversation_snapshot', session_id: 'sess-B', events: [] });

    expect(store.turns).toHaveLength(0);
    expect(store.sessionId).toBe('sess-B');
  });
});
