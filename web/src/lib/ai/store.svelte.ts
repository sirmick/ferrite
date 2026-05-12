// Chat store for the ferrite-ai sidecar. Holds the running conversation
// (user turns + assistant turns), the active mode, and the WebSocket
// connection state. Each WS message from the server is fed through
// `ingestEvent` which decides whether it's a new chunk in the current
// assistant turn (text delta, tool use, tool result) or an envelope
// signal (turn done, error, hello).
//
// Reload persistence: the Claude Agent SDK's session lives on the
// sidecar's filesystem (`.claude/sessions/<id>`), so once we have a
// `session_id` we can resume the conversation across browser reloads
// just by passing it back as `resume:` on the next query. We persist
// the session id + transcript + mode in localStorage; on store
// construction we restore everything, and on connect we send the
// stored id in the WS hello so the next user turn picks up where the
// previous browser session left off.

export type AiMode = 'explorer' | 'decoder' | 'diagnose' | 'chat';
export const AI_MODES: AiMode[] = ['explorer', 'decoder', 'diagnose', 'chat'];

export type ConnectionState = 'idle' | 'connecting' | 'connected' | 'closed' | 'error';

export type Chunk =
  | { kind: 'text'; text: string }
  /** A tool call the AI made. `input` is the JSON the SDK streams for
   *  the tool's arguments — accumulated across `input_json_delta`
   *  events. `result` is filled in when the matching `tool_result`
   *  arrives in a later message. */
  | { kind: 'tool'; id: string; name: string; input: string; result?: string; error?: boolean }
  /** Out-of-band envelope signals — `done`, `error`, debug. Rendered
   *  in muted text in the transcript. */
  | { kind: 'meta'; label: string };

export interface UserTurn {
  id: number;
  role: 'user';
  text: string;
  t: number;
}

export interface AssistantTurn {
  id: number;
  role: 'assistant';
  t: number;
  chunks: Chunk[];
  status: 'streaming' | 'complete' | 'error';
  errorMessage?: string;
}

export type Turn = UserTurn | AssistantTurn;

/** Storage keys, namespaced so they don't collide with the app's
 *  other localStorage entries (preset filters, panel splits, etc). */
const LS_TURNS = 'ferrite-ai.turns';
const LS_MODE = 'ferrite-ai.mode';
const LS_SESSION = 'ferrite-ai.session_id';
const LS_NEXT_ID = 'ferrite-ai.next_id';
/** Cap persisted history at 200 turns (~50–100 KB worst-case) so the
 *  ~5 MB localStorage budget stays comfortable even for marathon
 *  sessions. The Claude Agent SDK's actual conversation context is
 *  what continues across reloads via `resume`; this cap is purely a
 *  display ring. */
const MAX_PERSISTED_TURNS = 200;

class AiStore {
  turns = $state<Turn[]>([]);
  mode = $state<AiMode>('explorer');
  connection = $state<ConnectionState>('idle');
  lastError = $state<string | null>(null);
  /** Available modes from the server's hello message. Defaults to the
   *  client-side list so the dropdown renders immediately on first
   *  load before the WS handshake finishes. */
  availableModes = $state<AiMode[]>(AI_MODES);
  /** Sidecar-issued session id. Captured from the `system/init` event
   *  on the first turn of each conversation; sent back to the sidecar
   *  on subsequent turns so the Agent SDK can `resume:` instead of
   *  starting fresh. Null when no conversation has happened yet. */
  sessionId = $state<string | null>(null);

  private nextId = 1;
  private ws: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private retryCount = 0;
  private url: string | null = null;
  private persistDebounce: ReturnType<typeof setTimeout> | null = null;

  constructor() {
    this.restoreFromStorage();
  }

  /** Pull turns + mode + sessionId out of localStorage, tolerating
   *  partial / corrupt entries by skipping. Streaming turns from a
   *  previous session are downgraded to `complete` so the UI doesn't
   *  show a perpetual cursor on something that ended unobserved. */
  private restoreFromStorage(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      const rawTurns = localStorage.getItem(LS_TURNS);
      if (rawTurns) {
        const parsed = JSON.parse(rawTurns);
        if (Array.isArray(parsed)) {
          this.turns = parsed.map((t) => {
            if (t && t.role === 'assistant' && t.status === 'streaming') {
              return { ...t, status: 'complete' };
            }
            return t;
          }) as Turn[];
        }
      }
      const rawMode = localStorage.getItem(LS_MODE);
      if (rawMode && (AI_MODES as string[]).includes(rawMode)) {
        this.mode = rawMode as AiMode;
      }
      const rawSession = localStorage.getItem(LS_SESSION);
      if (rawSession) this.sessionId = rawSession;
      const rawNext = localStorage.getItem(LS_NEXT_ID);
      const n = rawNext ? parseInt(rawNext, 10) : NaN;
      if (Number.isFinite(n) && n > 0) this.nextId = n;
    } catch {
      /* corrupt storage; start fresh */
    }
  }

  /** Write turns + mode + sessionId back to localStorage. Debounced
   *  so a long stream of token deltas doesn't thrash the disk. */
  private persist(): void {
    if (typeof localStorage === 'undefined') return;
    if (this.persistDebounce) clearTimeout(this.persistDebounce);
    this.persistDebounce = setTimeout(() => {
      this.persistDebounce = null;
      try {
        const trimmed = this.turns.slice(-MAX_PERSISTED_TURNS);
        localStorage.setItem(LS_TURNS, JSON.stringify(trimmed));
        localStorage.setItem(LS_MODE, this.mode);
        localStorage.setItem(LS_NEXT_ID, String(this.nextId));
        if (this.sessionId) localStorage.setItem(LS_SESSION, this.sessionId);
        else localStorage.removeItem(LS_SESSION);
      } catch {
        /* quota / private mode / etc — silently degrade to non-persistent */
      }
    }, 250);
  }

  connect(url: string): void {
    this.url = url;
    this.openSocket();
  }

  disconnect(): void {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.close();
      this.ws = null;
    }
    this.connection = 'closed';
  }

  setMode(mode: AiMode): void {
    if (this.mode === mode) return;
    this.mode = mode;
    // Mode switch resets server-side conversation state — different
    // system prompt means the old conversation history would re-prime
    // under wrong context. Drop the sessionId too so the next turn
    // starts fresh rather than trying to resume the wrong-mode
    // conversation.
    this.sessionId = null;
    this.pushTurn({
      id: this.nextId++,
      role: 'assistant',
      t: Date.now(),
      chunks: [{ kind: 'meta', label: `mode → ${mode} (conversation reset)` }],
      status: 'complete',
    });
    this.persist();
  }

  send(text: string): void {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this.lastError = 'not connected';
      return;
    }
    this.pushTurn({
      id: this.nextId++,
      role: 'user',
      text: trimmed,
      t: Date.now(),
    });
    // Open a placeholder assistant turn so the UI immediately shows
    // "..." while the first delta is in flight.
    this.pushTurn({
      id: this.nextId++,
      role: 'assistant',
      t: Date.now(),
      chunks: [],
      status: 'streaming',
    });
    // `resume_session_id` lets the sidecar resume the prior Claude
    // Agent SDK session if we still have its id from a previous
    // exchange — the same conversation continues across browser
    // reloads and WS reconnects.
    const payload: Record<string, unknown> = { text: trimmed, mode: this.mode };
    if (this.sessionId) payload.resume_session_id = this.sessionId;
    this.ws.send(JSON.stringify(payload));
  }

  clear(): void {
    this.turns = [];
    this.sessionId = null;
    this.persist();
    if (typeof localStorage !== 'undefined') {
      localStorage.removeItem(LS_TURNS);
      localStorage.removeItem(LS_SESSION);
      localStorage.removeItem(LS_NEXT_ID);
    }
  }

  private openSocket(): void {
    if (!this.url) return;
    this.connection = 'connecting';
    this.lastError = null;
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch (e) {
      this.connection = 'error';
      this.lastError = String(e);
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.addEventListener('open', () => {
      this.retryCount = 0;
      this.connection = 'connected';
    });
    ws.addEventListener('message', (e) => {
      if (typeof e.data !== 'string') return;
      try {
        const parsed = JSON.parse(e.data) as Record<string, unknown>;
        this.ingestEvent(parsed);
      } catch {
        // Non-JSON messages are envelope debug — render as meta.
        this.appendChunkToCurrentTurn({ kind: 'meta', label: String(e.data) });
      }
    });
    ws.addEventListener('close', () => {
      this.ws = null;
      // If the user explicitly disconnected (`disconnect()`), connection
      // would be 'closed' already — don't auto-reconnect on top of that.
      if (this.connection !== 'closed') {
        this.connection = 'error';
        this.scheduleReconnect();
      }
    });
    ws.addEventListener('error', () => {
      // close fires next; reconnect logic lives there.
    });
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer) return;
    const delay = Math.min(1000 + this.retryCount * 500, 5000);
    this.retryCount += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.openSocket();
    }, delay);
  }

  private pushTurn(turn: Turn): void {
    this.turns = [...this.turns, turn];
    this.persist();
  }

  /** Mutate the most-recent assistant turn in place — Svelte's
   *  reactivity tracks the list reference, so we re-assign `turns` to
   *  trigger an update after each chunk change. */
  private updateLatestAssistant(updater: (a: AssistantTurn) => void): void {
    for (let i = this.turns.length - 1; i >= 0; i--) {
      const t = this.turns[i];
      if (t.role === 'assistant') {
        updater(t);
        // Slice + reassign so $state notices.
        this.turns = [...this.turns];
        this.persist();
        return;
      }
    }
  }

  private appendChunkToCurrentTurn(chunk: Chunk): void {
    this.updateLatestAssistant((a) => {
      a.chunks.push(chunk);
    });
  }

  ingestEvent(event: Record<string, unknown>): void {
    const t = event['type'];

    // The Agent SDK emits `system` with `subtype: 'init'` as the first
    // event of every turn, carrying the session id we need to `resume`
    // on subsequent turns. Capture + persist so a browser reload keeps
    // the conversation.
    if (t === 'system' && event['subtype'] === 'init') {
      const sid = event['session_id'];
      if (typeof sid === 'string' && sid !== this.sessionId) {
        this.sessionId = sid;
        this.persist();
      }
      return;
    }

    // Envelope signals first — short-circuit before walking SDK shape.
    if (t === 'ferrite_ai_hello') {
      const modes = event['modes'];
      if (Array.isArray(modes)) {
        const valid = modes.filter((m): m is AiMode => AI_MODES.includes(m as AiMode));
        if (valid.length) this.availableModes = valid;
      }
      return;
    }
    if (t === 'ferrite_ai_done') {
      this.updateLatestAssistant((a) => {
        if (a.status === 'streaming') a.status = 'complete';
      });
      return;
    }
    if (t === 'ferrite_ai_error') {
      const msg = String(event['message'] ?? 'unknown error');
      this.updateLatestAssistant((a) => {
        a.status = 'error';
        a.errorMessage = msg;
      });
      return;
    }

    // Agent SDK partial-message stream events. We unwrap the inner
    // event shape and pull out text deltas + tool calls for rendering;
    // anything else is dropped (the SDK's `stop_reason`, `usage`, etc
    // don't render in the chat directly).
    if (t === 'stream_event') {
      const inner = event['event'] as Record<string, unknown> | undefined;
      if (inner) this.ingestStreamEvent(inner);
      return;
    }

    // Full assistant message arrives at end-of-turn — useful when we
    // missed a partial (or `includePartialMessages` was off). Walk
    // content blocks; tool_result blocks here close out tool cards.
    if (t === 'assistant' || t === 'user') {
      const message = event['message'] as Record<string, unknown> | undefined;
      const content = message?.['content'];
      if (Array.isArray(content)) {
        for (const block of content) this.ingestContentBlock(block as Record<string, unknown>);
      }
      return;
    }

    // Anything else gets rendered as a muted meta line so the user can
    // see it for debugging without it disappearing silently.
    if (typeof t === 'string') {
      this.appendChunkToCurrentTurn({ kind: 'meta', label: t });
    }
  }

  private ingestStreamEvent(inner: Record<string, unknown>): void {
    const it = inner['type'];
    if (it === 'content_block_start') {
      const block = inner['content_block'] as Record<string, unknown> | undefined;
      if (!block) return;
      if (block['type'] === 'tool_use') {
        const id = String(block['id'] ?? '');
        const name = String(block['name'] ?? '?');
        this.appendChunkToCurrentTurn({ kind: 'tool', id, name, input: '' });
      }
      // text blocks just start an empty text chunk lazily on first delta
      return;
    }
    if (it === 'content_block_delta') {
      const delta = inner['delta'] as Record<string, unknown> | undefined;
      if (!delta) return;
      const dt = delta['type'];
      if (dt === 'text_delta') {
        const text = String(delta['text'] ?? '');
        this.appendTextDelta(text);
      } else if (dt === 'input_json_delta') {
        const partial = String(delta['partial_json'] ?? '');
        this.appendToolInput(partial);
      }
      return;
    }
    // content_block_stop — nothing to do; closing happens implicitly
    // when the next chunk type starts.
  }

  private ingestContentBlock(block: Record<string, unknown>): void {
    if (block['type'] === 'tool_result') {
      const id = String(block['tool_use_id'] ?? '');
      const content = block['content'];
      const errored = Boolean(block['is_error']);
      const text = renderToolResult(content);
      this.updateLatestAssistant((a) => {
        // Find matching tool chunk by id, otherwise append a synthetic
        // "orphan result" so the user sees it.
        for (let i = a.chunks.length - 1; i >= 0; i--) {
          const c = a.chunks[i];
          if (c.kind === 'tool' && c.id === id) {
            c.result = text;
            c.error = errored;
            return;
          }
        }
        a.chunks.push({
          kind: 'tool',
          id,
          name: '(orphan result)',
          input: '',
          result: text,
          error: errored,
        });
      });
    }
    // Full text blocks may also come on assistant messages at end of
    // turn — these would duplicate text we already accumulated from
    // partial deltas, so we ignore them here.
  }

  private appendTextDelta(text: string): void {
    if (!text) return;
    this.updateLatestAssistant((a) => {
      const last = a.chunks[a.chunks.length - 1];
      if (last && last.kind === 'text') {
        last.text += text;
      } else {
        a.chunks.push({ kind: 'text', text });
      }
    });
  }

  private appendToolInput(partial: string): void {
    if (!partial) return;
    this.updateLatestAssistant((a) => {
      // Walk back to the most-recent unfinished tool chunk.
      for (let i = a.chunks.length - 1; i >= 0; i--) {
        const c = a.chunks[i];
        if (c.kind === 'tool' && c.result === undefined) {
          c.input += partial;
          return;
        }
      }
    });
  }
}

function renderToolResult(content: unknown): string {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content
      .map((part: unknown) => {
        if (typeof part === 'string') return part;
        if (typeof part === 'object' && part !== null) {
          const text = (part as Record<string, unknown>)['text'];
          if (typeof text === 'string') return text;
        }
        return JSON.stringify(part);
      })
      .join('');
  }
  return JSON.stringify(content);
}

export const ai = new AiStore();
