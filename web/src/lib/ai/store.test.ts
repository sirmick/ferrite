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

/** Stub fetch: record every call, answer from `route` (default `{}` 200). */
function stubFetch(
  route: (
    url: string,
    body: unknown,
  ) =>
    | { status?: number; body?: unknown }
    | Promise<{ status?: number; body?: unknown }> = () => ({}),
): { calls: { url: string; body: unknown }[] } {
  const calls: { url: string; body: unknown }[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: { body?: string }) => {
      const body = init?.body ? JSON.parse(init.body) : undefined;
      calls.push({ url, body });
      const r = await route(url, body);
      return new Response(JSON.stringify(r.body ?? {}), { status: r.status ?? 200 });
    }),
  );
  return { calls };
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

describe('control — REST to the sidecar', () => {
  afterEach(() => vi.unstubAllGlobals());

  it("a prompt goes to this mode with the tab's time zone; the sidecar builds the rest", async () => {
    const { calls } = stubFetch();
    const store = new AiStore();
    store.setMode('decoder');
    await store.send('  what is on 40m?  ');
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('/api/ai/prompt');
    const body = calls[0].body as Record<string, unknown>;
    expect(body.text).toBe('what is on 40m?');
    expect(body.mode).toBe('decoder');
    expect(typeof body.time_zone).toBe('string');
    // No session binding and no browser-assembled context.
    expect(Object.keys(body).sort()).toEqual(['mode', 'text', 'time_zone']);
    expect(store.isStreaming()).toBe(true);
  });

  it('a refused prompt drops the optimistic turn and says why', async () => {
    stubFetch(() => ({
      status: 409,
      body: { error: 'the explorer conversation is busy with a turn' },
    }));
    const store = new AiStore();
    await store.send('hello');
    expect(store.isStreaming()).toBe(false);
    const last = store.turns.at(-1) as AssistantTurn;
    expect(last.chunks[0]).toEqual({
      kind: 'meta',
      label: '── the explorer conversation is busy with a turn',
    });
  });

  it('/reset starts this mode over; Clear resets the store first, then every mode', async () => {
    const order: string[] = [];
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const { calls } = stubFetch(async (url) => {
      order.push(url);
      if (url.startsWith('/api/state/')) await gate;
      return {};
    });
    const store = new AiStore();
    await store.resetSession({ reason: 'fresh look' });
    expect(calls[0]).toEqual({
      url: '/api/ai/reset',
      body: { mode: 'explorer', reason: 'fresh look' },
    });

    const done = store.clear();
    await Promise.resolve();
    // The banner record must not race the store reset that would wipe it.
    expect(order.slice(1)).toEqual(['/api/state/ai/reset', '/api/state/ai_live/reset']);
    release();
    await done;
    expect(calls.at(-1)).toEqual({ url: '/api/ai/reset', body: { reason: 'clear' } });
  });

  it('switching mode is not a reset — each mode keeps its own conversation', () => {
    const { calls } = stubFetch();
    const store = new AiStore();
    store.setMode('chat');
    expect(store.mode).toBe('chat');
    expect(calls).toHaveLength(0);
  });

  it('hello sets modes and models; station notes come from the sidecar', async () => {
    stubFetch((url) =>
      url.endsWith('/hello')
        ? {
            body: {
              modes: ['explorer', 'chat'],
              models: { explorer: 'claude-opus-5-5' },
              model_options: ['claude-opus-5-5'],
            },
          }
        : url.endsWith('/setup')
          ? { body: { text: 'Antenna C: longwire' } }
          : {},
    );
    const store = new AiStore();
    store.connect('/api/ai');
    await vi.waitFor(() => expect(store.connection).toBe('connected'));
    store.disconnect();
    expect(store.availableModes).toEqual(['explorer', 'chat']);
    expect(store.defaultModelFor('explorer')).toBe('claude-opus-5-5');
    expect(store.setupDescription).toBe('Antenna C: longwire');
  });

  it('an unreachable sidecar shows offline', async () => {
    stubFetch(() => ({ status: 502, body: { error: 'ferrite-ai sidecar unreachable' } }));
    const store = new AiStore();
    store.connect('/api/ai');
    await vi.waitFor(() => expect(store.connection).toBe('error'));
    store.disconnect();
    expect(store.lastError).toContain('unreachable');
  });

  it('old localStorage keys (session, station notes) are dropped on load', () => {
    localStorage.setItem('ferrite-ai.session_id', 'old');
    localStorage.setItem('ferrite-ai.setup_description', 'old notes');
    new AiStore();
    expect(localStorage.getItem('ferrite-ai.session_id')).toBeNull();
    expect(localStorage.getItem('ferrite-ai.setup_description')).toBeNull();
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
