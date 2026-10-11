// Contract tests for the AI panel as a view of the session bus.
//
// The conversation is the server's `ai` store kind (records published
// by the ferrite-ai sidecar and by headless drivers' hooks); `turns` is
// derived from those records by `buildTurns`. These pin the fold
// (grouping by session, tool cards, turn end, resets, the live partial,
// the optimistic pending prompt) and the control channel (every reset
// entry point tells the sidecar; banners aren't doubled).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  AiStore,
  buildTurns,
  clearedAtMs,
  type AiRecord,
  type AssistantTurn,
  type UserTurn,
} from './store.svelte';

let seq = 0;
function rec(data: Record<string, unknown>, at = 1000 + seq): AiRecord {
  seq += 1;
  return { seq, at_ms: at, data };
}

beforeEach(() => {
  seq = 0;
  localStorage.clear();
});

/** Attach a fake OPEN socket so reset paths take their online branch. */
function attachFakeSocket(store: AiStore): { sent: string[] } {
  const sent: string[] = [];
  (store as unknown as { ws: unknown }).ws = {
    readyState: WebSocket.OPEN,
    send: (m: string) => sent.push(m),
  };
  return { sent };
}

describe('buildTurns — the session-bus fold', () => {
  it('folds one turn: prompt, text, tool use + result, turn end', () => {
    const s = { session: 'A', source: 'ferrite-ai' };
    const turns = buildTurns(
      [
        rec({ ...s, type: 'user', text: 'what is on 40m?' }),
        rec({ ...s, type: 'text', text: 'Checking.' }),
        rec({ ...s, type: 'tool_use', tool_id: 't1', name: 'tune', input: '{"freq_hz":7074000}' }),
        rec({ ...s, type: 'tool_result', tool_id: 't1', text: 'ok', is_error: false }),
        rec({ ...s, type: 'text', text: 'FT8 is busy.' }),
        rec({ ...s, type: 'turn_end', status: 'done' }),
      ],
      {},
      [],
      null,
    );
    expect(turns).toHaveLength(2);
    const [u, a] = turns as [UserTurn, AssistantTurn];
    expect(u.text).toBe('what is on 40m?');
    expect(a.status).toBe('complete');
    expect(a.chunks.map((c) => c.kind)).toEqual(['text', 'tool', 'text']);
    const tool = a.chunks[1] as { result?: string; name: string };
    expect(tool.name).toBe('tune');
    expect(tool.result).toBe('ok');
  });

  it('keeps interleaved sessions apart and labels their source', () => {
    const turns = buildTurns(
      [
        rec({ session: 'A', source: 'ferrite-ai', type: 'user', text: 'panel q' }),
        rec({ session: 'B', source: 'claude-code', type: 'user', text: 'headless q' }),
        rec({ session: 'A', source: 'ferrite-ai', type: 'text', text: 'panel a' }),
        rec({ session: 'B', source: 'claude-code', type: 'text', text: 'headless a' }),
        rec({ session: 'B', source: 'claude-code', type: 'turn_end', status: 'done' }),
      ],
      {},
      [],
      null,
      2000,
    );
    const assistants = turns.filter((t) => t.role === 'assistant') as AssistantTurn[];
    const bySession = Object.fromEntries(assistants.map((a) => [a.session, a]));
    expect((bySession['A'].chunks[0] as { text: string }).text).toBe('panel a');
    expect(bySession['A'].status).toBe('streaming');
    expect((bySession['B'].chunks[0] as { text: string }).text).toBe('headless a');
    expect(bySession['B'].status).toBe('complete');
    expect(bySession['B'].source).toBe('claude-code');
  });

  it('folds thinking and keeps only the newest plan', () => {
    const s = { session: 'A', source: 'ferrite-ai' };
    const turns = buildTurns(
      [
        rec({ ...s, type: 'user', text: 'find CW' }),
        rec({ ...s, type: 'thought', text: 'Snapshot first, then spectrum.' }),
        rec({ ...s, type: 'plan', entries: [{ content: 'snapshot', status: 'in_progress' }] }),
        rec({ ...s, type: 'tool_use', tool_id: 't1', name: 'snapshot', input: '{}' }),
        rec({ ...s, type: 'plan', entries: [{ content: 'snapshot', status: 'completed' }] }),
      ],
      { A: { data: { session: 'A', kind: 'thought', text: 'Now the env' } } },
      [],
      null,
    );
    const a = turns[1] as AssistantTurn;
    expect(a.chunks.map((c) => c.kind)).toEqual(['thought', 'tool', 'plan', 'thought']);
    expect((a.chunks[2] as { entries: { status: string }[] }).entries[0].status).toBe('completed');
    expect(a.chunks[3]).toEqual({ kind: 'thought', text: 'Now the env' });
  });

  it('shows the live partial on the open turn only', () => {
    const turns = buildTurns(
      [rec({ session: 'A', type: 'user', text: 'q' })],
      { A: { data: { session: 'A', text: 'The band is op' } } },
      [],
      null,
    );
    const a = turns[1] as AssistantTurn;
    expect(a.status).toBe('streaming');
    expect(a.chunks).toEqual([{ kind: 'text', text: 'The band is op' }]);
  });

  it('errors and stops close the turn honestly', () => {
    const turns = buildTurns(
      [
        rec({ session: 'A', type: 'user', text: 'q1' }),
        rec({ session: 'A', type: 'turn_end', status: 'error', error: 'boom' }),
        rec({ session: 'A', type: 'user', text: 'q2' }),
        rec({ session: 'A', type: 'turn_end', status: 'stopped' }),
      ],
      {},
      [],
      null,
    );
    const [, e, , s] = turns as AssistantTurn[];
    expect(e.status).toBe('error');
    expect(e.errorMessage).toBe('boom');
    expect(s.status).toBe('complete');
    expect(s.chunks.at(-1)).toEqual({ kind: 'meta', label: '── stopped' });
  });

  it('a reset record is a banner; history before it stays', () => {
    const turns = buildTurns(
      [
        rec({ session: 'A', type: 'user', text: 'q' }),
        rec({ session: 'A', type: 'turn_end', status: 'done' }),
        rec({ session: 'A', type: 'reset', reason: 'mode → decoder' }),
      ],
      {},
      [],
      null,
    );
    expect(turns).toHaveLength(3);
    const banner = turns[2] as AssistantTurn;
    expect(banner.chunks[0]).toMatchObject({ kind: 'meta' });
    expect((banner.chunks[0] as { label: string }).label).toContain('mode → decoder');
  });

  it('the optimistic prompt shows until its user record lands', () => {
    const pending = { text: 'hello', t: 5000 };
    const before = buildTurns([], {}, [], pending);
    expect(before.map((t) => t.role)).toEqual(['user', 'assistant']);
    expect((before[1] as AssistantTurn).status).toBe('streaming');
    const after = buildTurns(
      [rec({ session: 'A', type: 'user', text: 'hello' }, 5100)],
      {},
      [],
      pending,
    );
    expect(after.filter((t) => t.role === 'user')).toHaveLength(1);
  });

  it('a stripped image count is surfaced on the result', () => {
    const turns = buildTurns(
      [
        rec({ session: 'A', type: 'user', text: 'q' }),
        rec({ session: 'A', type: 'tool_use', tool_id: 't', name: 'Read', input: '' }),
        rec({ session: 'A', type: 'tool_result', tool_id: 't', text: '', images_stripped: 1 }),
      ],
      {},
      [],
      null,
    );
    const tool = (turns[1] as AssistantTurn).chunks[0] as { result?: string };
    expect(tool.result).toContain('1 image(s)');
  });
});

describe('control channel', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('/reset, Clear, setMode all drop the sidecar binding (unified path)', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('{}')),
    );
    const store = new AiStore();
    const { sent } = attachFakeSocket(store);
    store.sessionId = 'live';

    store.resetSession({ reason: 'reset' });
    await store.clear();
    store.setMode('decoder');

    const reasons = sent
      .map((m) => JSON.parse(m))
      .filter((m) => m.type === 'reset_session')
      .map((m) => m.reason as string);
    expect(reasons).toEqual(['reset', 'clear', expect.stringContaining('decoder')]);
    expect(store.sessionId).toBeNull();
  });

  it('Clear resets the store before asking the sidecar for its banner', async () => {
    const order: string[] = [];
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string) => {
        order.push(`fetch ${url}`);
        await gate;
        return new Response('{}');
      }),
    );
    const store = new AiStore();
    const fake = attachFakeSocket(store);
    const origPush = fake.sent.push.bind(fake.sent);
    fake.sent.push = (m: string) => {
      order.push(`ws ${JSON.parse(m).type}`);
      return origPush(m);
    };

    const done = store.clear();
    await Promise.resolve();
    // Resets in flight, sidecar not told yet: its banner record would
    // otherwise land first and be wiped by the reset.
    expect(order).toEqual(['fetch /api/state/ai/reset', 'fetch /api/state/ai_live/reset']);
    release();
    await done;
    expect(order.at(-1)).toBe('ws reset_session');
  });

  it('a reset the sidecar published adds no local banner; an unresumable session does (once)', () => {
    const store = new AiStore();
    store.sessionId = 'doomed';
    store.ingestEvent({ type: 'session_reset', reason: 'reset', on_bus: true });
    expect(store.sessionId).toBeNull();
    expect(store.turns).toHaveLength(0);

    store.ingestEvent({ type: 'session_reset', reason: 'no transcript on disk' });
    store.ingestEvent({ type: 'session_reset', reason: 'no transcript on disk' });
    expect(store.turns).toHaveLength(1);
    expect(((store.turns[0] as AssistantTurn).chunks[0] as { label: string }).label).toContain(
      'no transcript on disk',
    );
  });

  it('offline /reset keeps history and notes the reset locally', () => {
    const store = new AiStore();
    store.sessionId = 'doomed';
    store.resetSession({ reason: 'reset' });
    expect(store.sessionId).toBeNull();
    expect(store.turns).toHaveLength(1);
    expect(((store.turns[0] as AssistantTurn).chunks[0] as { label: string }).label).toContain(
      'history preserved',
    );
  });

  it('system/init binds the session for resume and persists it', () => {
    const store = new AiStore();
    store.ingestEvent({ type: 'system', subtype: 'init', session_id: 'sess-9' });
    expect(store.sessionId).toBe('sess-9');
    expect(localStorage.getItem('ferrite-ai.session_id')).toBe('sess-9');
  });
});

describe('clearedAtMs', () => {
  it('is the newest clear record, 0 when never cleared', () => {
    expect(clearedAtMs([])).toBe(0);
    expect(clearedAtMs([rec({ type: 'user', text: 'x' }, 5)])).toBe(0);
    expect(
      clearedAtMs([
        rec({ type: 'clear' }, 10),
        rec({ type: 'user', text: 'x' }, 20),
        rec({ type: 'clear' }, 30),
      ]),
    ).toBe(30);
  });

  it('a clear record adds no turn', () => {
    expect(buildTurns([rec({ type: 'clear' })], {}, [], null)).toHaveLength(0);
  });
});
