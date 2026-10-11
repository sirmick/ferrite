// AI panel store — a view of the AI session bus.
//
// Every conversation, whichever client drives it (this panel via the
// ferrite-ai sidecar, another tab, a Claude Code session's hooks), is
// the server's `ai` store kind: one record per user prompt / finished
// text block / tool use / tool result / turn end / reset, tagged with
// `session` and `source`. The mirror (`decoders`) replicates it over the
// event plane and the server persists it, so `turns` is *derived* from
// those records (plus the `ai_live` partial reply and a few tab-local
// notices). Nothing about the transcript lives only in this tab.
//
// Control is plain REST to the sidecar through ferrited (`/api/ai/*`):
// prompt, cancel, reset, station notes, hello — the same front door the
// CLI uses. The tab holds no session: each mode has one current
// conversation, shared by every tab and driver.

import DOMPurify from 'dompurify';
import { marked } from 'marked';

import { decoders } from '$lib/decoders/store.svelte';

// Markdown configuration. `gfm: true` enables GitHub-Flavoured Markdown
// (tables, strikethrough, autolinks, code fences with language).
// `breaks: false` keeps a single newline as plain whitespace rather
// than `<br>` — Claude emits real `\n\n` between paragraphs, so the
// stricter rule produces cleaner output. Synchronous mode so we can
// render inline in a $derived.
marked.setOptions({ gfm: true, breaks: false, async: false });

export type AiMode = 'explorer' | 'decoder' | 'diagnose' | 'chat';
export const AI_MODES: AiMode[] = ['explorer', 'decoder', 'diagnose', 'chat'];

export type ConnectionState = 'idle' | 'connecting' | 'connected' | 'closed' | 'error';

export type Chunk =
  | { kind: 'text'; text: string }
  /** The model's (summarised) thinking — collapsed by default. */
  | { kind: 'thought'; text: string }
  /** The agent's todo plan; a newer `plan` record replaces it. */
  | { kind: 'plan'; entries: PlanEntry[] }
  /** A tool call. `result` fills in when the matching `tool_result`
   *  record arrives; `resultImages` are `data:` URLs (only the newest
   *  few records keep them — older ones carry a count). */
  | {
      kind: 'tool';
      id: string;
      name: string;
      input: string;
      result?: string;
      resultImages?: string[];
      error?: boolean;
      /** The `analysis` record this call drew (spectrum, sigwiki card,
       *  spot map…) — rendered inside the tool card. */
      analysisId?: string;
      /** The turn ended (stopped / failed) before this call returned. */
      interrupted?: boolean;
    }
  /** Out-of-band notices — resets, stops, errors. Muted in the panel. */
  | { kind: 'meta'; label: string };

export interface PlanEntry {
  content: string;
  status: 'pending' | 'in_progress' | 'completed' | string;
  priority?: string;
}

export interface UserTurn {
  id: number;
  role: 'user';
  text: string;
  t: number;
  /** Who drove this turn (`ferrite-ai` = this panel's sidecar). */
  source?: string;
  session?: string | null;
}

export interface AssistantTurn {
  id: number;
  role: 'assistant';
  t: number;
  chunks: Chunk[];
  status: 'streaming' | 'complete' | 'error';
  errorMessage?: string;
  source?: string;
  session?: string | null;
  mode?: string;
  model?: string;
}

export type Turn = UserTurn | AssistantTurn;

/** One `ai` record as the mirror holds it. */
export interface AiRecord {
  seq: number;
  at_ms: number;
  data: Record<string, unknown>;
}

/** A tab-local notice (slash-command output, offline banner). */
export interface LocalNote {
  t: number;
  label: string;
}

const LS_MODE = 'ferrite-ai.mode';
const LS_MODEL = 'ferrite-ai.model';
/** Keys from older builds — removed on load: the transcript, the
 *  session binding and the station notes all live server-side now. */
const LS_LEGACY = [
  'ferrite-ai.turns',
  'ferrite-ai.next_id',
  'ferrite-ai.session_id',
  'ferrite-ai.setup_description',
];

/** A streaming turn with no record for this long ended unobserved
 *  (the sidecar died mid-turn); stop showing it as live. */
const STALE_STREAM_MS = 10 * 60_000;

const str = (v: unknown): string => (typeof v === 'string' ? v : v == null ? '' : String(v));

export function resetBannerLabel(reason: string): string {
  // After a clear there is no history above to preserve.
  if (reason === 'clear') return '── cleared — new conversation';
  return `── new conversation (${reason}) — the AI won't remember the above`;
}

/** Has the optimistic prompt's own `user` record reached the bus? */
export function pendingLanded(records: AiRecord[], pending: { text: string; t: number }): boolean {
  return records.some(
    (r) =>
      r.data?.['type'] === 'user' &&
      str(r.data['text']) === pending.text &&
      r.at_ms >= pending.t - 5_000,
  );
}

/** When the AI log was last cleared (the newest `clear` record the
 *  server leaves on an `ai` reset), or 0. The panel hides activity and
 *  analysis records older than this. */
export function clearedAtMs(records: { at_ms: number; data?: unknown }[]): number {
  let at = 0;
  for (const r of records) {
    const d = r.data as Record<string, unknown> | undefined;
    if (d?.['type'] === 'clear' && r.at_ms > at) at = r.at_ms;
  }
  return at;
}

/** Fold the `ai` session-bus records (+ live partials, local notes and
 *  an optimistic pending prompt) into display turns. Pure — the store's
 *  `turns` is `$derived` from it. A `user` record opens a turn for its
 *  session; that session's following records fill it until `turn_end`;
 *  sessions interleave freely. */
export function buildTurns(
  records: AiRecord[],
  live: Record<string, { data: Record<string, unknown> }>,
  notes: LocalNote[],
  pending: { text: string; t: number } | null,
  now: number = Date.now(),
): Turn[] {
  const out: Turn[] = [];
  const open = new Map<string, AssistantTurn>();
  const lastAt = new Map<AssistantTurn, number>();
  const sessionKey = (d: Record<string, unknown>) => str(d['session']) || '∅';

  const assistantFor = (rec: AiRecord, d: Record<string, unknown>): AssistantTurn => {
    const key = sessionKey(d);
    let a = open.get(key);
    if (!a) {
      a = {
        id: rec.seq * 2 + 1,
        role: 'assistant',
        t: rec.at_ms,
        chunks: [],
        status: 'streaming',
        source: str(d['source']) || undefined,
        session: (d['session'] as string | null | undefined) ?? null,
        mode: str(d['mode']) || undefined,
        model: str(d['model']) || undefined,
      };
      out.push(a);
      open.set(key, a);
    }
    lastAt.set(a, rec.at_ms);
    // A mid-conversation model switch: the newest model wins.
    if (str(d['model'])) a.model = str(d['model']);
    return a;
  };

  for (const rec of [...records].sort((x, y) => x.seq - y.seq)) {
    const d = rec.data ?? {};
    const key = sessionKey(d);
    switch (d['type']) {
      case 'user': {
        const prev = open.get(key);
        if (prev && prev.status === 'streaming') prev.status = 'complete';
        out.push({
          id: rec.seq * 2,
          role: 'user',
          text: str(d['text']),
          t: rec.at_ms,
          source: str(d['source']) || undefined,
          session: (d['session'] as string | null | undefined) ?? null,
        });
        open.delete(key);
        assistantFor(rec, d);
        break;
      }
      case 'text': {
        const a = assistantFor(rec, d);
        a.chunks.push({ kind: 'text', text: str(d['text']) });
        break;
      }
      case 'thought': {
        const a = assistantFor(rec, d);
        a.chunks.push({ kind: 'thought', text: str(d['text']) });
        break;
      }
      case 'plan': {
        const a = assistantFor(rec, d);
        const entries = (Array.isArray(d['entries']) ? d['entries'] : []) as PlanEntry[];
        const at = a.chunks.findIndex((c) => c.kind === 'plan');
        if (at >= 0) a.chunks.splice(at, 1);
        a.chunks.push({ kind: 'plan', entries });
        break;
      }
      case 'tool_use': {
        const a = assistantFor(rec, d);
        a.chunks.push({
          kind: 'tool',
          id: str(d['tool_id']),
          name: str(d['name']) || '?',
          input: str(d['input']),
        });
        break;
      }
      case 'tool_result': {
        const a = assistantFor(rec, d);
        const id = str(d['tool_id']);
        const images = Array.isArray(d['images']) ? (d['images'] as unknown[]).map(str) : [];
        const stripped =
          typeof d['images_stripped'] === 'number' ? (d['images_stripped'] as number) : 0;
        let result = str(d['text']);
        if (stripped)
          result += `${result ? '\n' : ''}[${stripped} image(s) — older results keep only the count]`;
        const tool = a.chunks.find((c) => c.kind === 'tool' && c.id === id) as
          | Extract<Chunk, { kind: 'tool' }>
          | undefined;
        const analysisId = str(d['analysis_id']) || undefined;
        if (tool) {
          tool.result = result;
          if (images.length) tool.resultImages = images;
          tool.error = Boolean(d['is_error']);
          tool.analysisId = analysisId;
        } else {
          a.chunks.push({
            kind: 'tool',
            id,
            name: '(result)',
            input: '',
            result,
            resultImages: images.length ? images : undefined,
            error: Boolean(d['is_error']),
            analysisId,
          });
        }
        break;
      }
      case 'turn_end': {
        const a = open.get(key) ?? assistantFor(rec, d);
        const status = str(d['status']);
        // A call still waiting on its result never gets one now.
        for (const c of a.chunks) {
          if (c.kind === 'tool' && c.result === undefined) c.interrupted = true;
        }
        if (status === 'error') {
          a.status = 'error';
          a.errorMessage = str(d['error']) || 'turn failed';
        } else {
          a.status = 'complete';
          if (status === 'stopped') a.chunks.push({ kind: 'meta', label: '── stopped' });
        }
        open.delete(key);
        break;
      }
      case 'reset': {
        const prev = open.get(key);
        if (prev && prev.status === 'streaming') prev.status = 'complete';
        open.delete(key);
        out.push({
          id: rec.seq * 2 + 1,
          role: 'assistant',
          t: rec.at_ms,
          chunks: [{ kind: 'meta', label: resetBannerLabel(str(d['reason']) || 'reset') }],
          status: 'complete',
          source: str(d['source']) || undefined,
          session: (d['session'] as string | null | undefined) ?? null,
          mode: str(d['mode']) || undefined,
        });
        break;
      }
      default:
        break;
    }
  }

  // The reply forming right now, and turns that ended unobserved.
  for (const [key, a] of open) {
    if (a.status !== 'streaming') continue;
    const partial = str(live[key]?.data?.['text']);
    const kind = live[key]?.data?.['kind'] === 'thought' ? 'thought' : 'text';
    if (partial) a.chunks = [...a.chunks, { kind, text: partial }];
    else if (now - (lastAt.get(a) ?? a.t) > STALE_STREAM_MS) a.status = 'complete';
  }

  // Tab-local notices, by time.
  let seq = -1;
  for (const n of notes) {
    out.push({
      id: seq--,
      role: 'assistant',
      t: n.t,
      chunks: [{ kind: 'meta', label: n.label }],
      status: 'complete',
    });
  }
  out.sort((x, y) => x.t - y.t || x.id - y.id);

  // Optimistic prompt until the sidecar's `user` record lands.
  if (pending) {
    if (!pendingLanded(records, pending)) {
      out.push({ id: seq--, role: 'user', text: pending.text, t: pending.t, source: 'ferrite-ai' });
      out.push({
        id: seq--,
        role: 'assistant',
        t: pending.t,
        chunks: [],
        status: 'streaming',
        source: 'ferrite-ai',
      });
    }
  }
  return out;
}

export class AiStore {
  /** This tab's mode and model picks (view preferences). Each mode has
   *  its own conversation, shared by every tab and the CLI. */
  mode = $state<AiMode>('explorer');
  /** Model override for the next turns, or `null` for the sidecar's
   *  per-mode default. */
  model = $state<string | null>(null);
  connection = $state<ConnectionState>('idle');
  lastError = $state<string | null>(null);
  /** From the sidecar's hello; client-side defaults until it answers. */
  availableModes = $state<AiMode[]>(AI_MODES);
  modelOptions = $state<string[]>([]);
  modelDefaults = $state<Record<string, string>>({});
  /** The station notes — one copy, kept by the sidecar and read by
   *  every session it starts. */
  setupDescription = $state<string>('');

  /** Tab-local notices (slash-command output, request errors). */
  private notes = $state<LocalNote[]>([]);
  /** The prompt this tab just sent, until its `user` record lands. */
  private pending = $state<{ text: string; t: number } | null>(null);

  /** Every session's turns, from the bus. */
  turns = $derived.by<Turn[]>(() =>
    buildTurns(
      decoders.kind('ai').recent as AiRecord[],
      decoders.kind('ai_live').current as Record<string, { data: Record<string, unknown> }>,
      this.notes,
      this.pending,
    ),
  );

  private timer: ReturnType<typeof setTimeout> | null = null;
  private base = '/api/ai';

  constructor() {
    this.restoreFromStorage();
  }

  private restoreFromStorage(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      for (const k of LS_LEGACY) localStorage.removeItem(k);
      const rawMode = localStorage.getItem(LS_MODE);
      if (rawMode && (AI_MODES as string[]).includes(rawMode)) this.mode = rawMode as AiMode;
      const rawModel = localStorage.getItem(LS_MODEL);
      if (rawModel) this.model = rawModel;
    } catch {
      /* corrupt storage; start fresh */
    }
  }

  private persist(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      localStorage.setItem(LS_MODE, this.mode);
      if (this.model) localStorage.setItem(LS_MODEL, this.model);
      else localStorage.removeItem(LS_MODEL);
    } catch {
      /* private mode etc. — non-persistent */
    }
  }

  /** POST JSON to the sidecar (via ferrited). Returns the parsed body,
   *  or throws with the sidecar's error text. */
  private async post(path: string, body: unknown): Promise<Record<string, unknown>> {
    const r = await fetch(`${this.base}${path}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const out = (await r.json().catch(() => ({}))) as Record<string, unknown>;
    if (!r.ok) throw new Error(str(out['error']) || `${path}: HTTP ${r.status}`);
    return out;
  }

  /** Start talking to the sidecar: hello (modes, models), the station
   *  notes, then a slow liveness poll. */
  connect(base = '/api/ai'): void {
    this.base = base;
    if (this.connection === 'idle' || this.connection === 'closed') this.connection = 'connecting';
    void this.poll(true);
  }

  disconnect(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.connection = 'closed';
  }

  private async poll(first = false): Promise<void> {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    try {
      const r = await fetch(`${this.base}/hello`);
      const hello = (await r.json()) as Record<string, unknown>;
      if (!r.ok) throw new Error(str(hello['error']) || `HTTP ${r.status}`);
      this.applyHello(hello);
      if (this.connection !== 'connected' || first) await this.loadSetup();
      this.connection = 'connected';
      this.lastError = null;
    } catch (e) {
      this.connection = 'error';
      this.lastError = `ferrite-ai offline (${e instanceof Error ? e.message : String(e)})`;
    }
    if ((this.connection as ConnectionState) === 'closed') return;
    this.timer = setTimeout(
      () => void this.poll(),
      this.connection === 'connected' ? 15_000 : 3_000,
    );
  }

  private applyHello(hello: Record<string, unknown>): void {
    const modes = hello['modes'];
    if (Array.isArray(modes)) {
      const valid = modes.filter((m): m is AiMode => AI_MODES.includes(m as AiMode));
      if (valid.length) this.availableModes = valid;
    }
    const options = hello['model_options'];
    if (Array.isArray(options)) {
      this.modelOptions = options.filter((o): o is string => typeof o === 'string');
      // A stored override the sidecar no longer offers falls back to default.
      if (this.model && !this.modelOptions.includes(this.model)) this.model = null;
    }
    const defaults = hello['models'];
    if (defaults && typeof defaults === 'object') {
      const out: Record<string, string> = {};
      for (const [k, v] of Object.entries(defaults as Record<string, unknown>)) {
        if (typeof v === 'string') out[k] = v;
      }
      this.modelDefaults = out;
    }
  }

  private async loadSetup(): Promise<void> {
    try {
      const r = await fetch(`${this.base}/setup`);
      if (r.ok) this.setupDescription = str(((await r.json()) as Record<string, unknown>)['text']);
    } catch {
      /* keep what we have */
    }
  }

  private fail(e: unknown): void {
    this.pending = null;
    this.lastError = e instanceof Error ? e.message : String(e);
    this.note(`── ${this.lastError}`);
  }

  /** Append a tab-local notice, skipping an identical consecutive one. */
  private note(label: string): void {
    if (this.notes.at(-1)?.label === label) return;
    this.notes = [...this.notes, { t: Date.now(), label }];
  }

  /** Pick a model override (`null` = sidecar default) for the next turns. */
  setModel(model: string | null): void {
    const next = model && this.modelOptions.includes(model) ? model : null;
    if (this.model === next) return;
    this.model = next;
    this.persist();
  }

  /** The sidecar's default model for `mode`, once known from hello. */
  defaultModelFor(mode: AiMode): string | null {
    return this.modelDefaults[mode] ?? null;
  }

  /** Switch mode: the next prompt continues *that* mode's conversation. */
  setMode(mode: AiMode): void {
    if (this.mode === mode) return;
    this.mode = mode;
    this.persist();
  }

  /** Replace the station notes. Every session's system prompt carries
   *  them, so the sidecar starts every mode over (one banner). */
  async setSetupDescription(text: string): Promise<void> {
    if (text === this.setupDescription) return;
    const prev = this.setupDescription;
    this.setupDescription = text;
    try {
      await this.post('/setup', { text });
    } catch (e) {
      this.setupDescription = prev;
      this.fail(e);
    }
  }

  /** Send a prompt to this mode's conversation. The sidecar builds the
   *  AI's context (radio state, notes, clock) itself — the same for a
   *  prompt from here, another tab or the CLI. */
  async send(text: string): Promise<void> {
    const trimmed = text.trim();
    if (!trimmed) return;
    this.pending = { text: trimmed, t: Date.now() };
    const body: Record<string, unknown> = {
      text: trimmed,
      mode: this.mode,
      time_zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    };
    if (this.model) body.model = this.model;
    try {
      await this.post('/prompt', body);
    } catch (e) {
      this.fail(e);
    }
  }

  /** Hard clear: empty the AI log for every panel (the server leaves a
   *  `clear` marker the panel uses to hide older activity / analysis)
   *  and start every mode's conversation over. The sidecar's reset goes
   *  out only after the store reset lands — its banner is an `ai`
   *  record, which a later reset would wipe. */
  async clear(): Promise<void> {
    this.notes = [];
    this.pending = null;
    const reset = (kind: string) =>
      fetch(`/api/state/${kind}/reset`, { method: 'POST' }).catch(() => undefined);
    await Promise.all([reset('ai'), reset('ai_live')]);
    try {
      await this.post('/reset', { reason: 'clear' });
    } catch (e) {
      this.fail(e);
    }
  }

  /** `/reset`: this mode's next prompt opens a fresh conversation; the
   *  history stays visible. */
  async resetSession(opts: { reason?: string } = {}): Promise<void> {
    const reason = opts.reason?.trim() || 'reset';
    try {
      await this.post('/reset', { mode: this.mode, reason });
    } catch (e) {
      this.fail(e);
    }
  }

  /** The panel's in-flight turn: this mode's newest streaming turn from
   *  the in-app AI (another driver's turn isn't ours to stop). */
  private streamingTurn(): AssistantTurn | null {
    for (let i = this.turns.length - 1; i >= 0; i--) {
      const t = this.turns[i];
      if (t.role !== 'assistant' || !t.chunks.length || t.source !== 'ferrite-ai') continue;
      if (t.mode && t.mode !== this.mode) continue;
      return t.status === 'streaming' ? t : null;
    }
    return null;
  }

  /** Stop this mode's running turn. Its `turn_end` (`stopped`) record
   *  closes it on every panel. */
  async stop(): Promise<void> {
    const t = this.streamingTurn();
    if (!t && !this.pending) return;
    try {
      await this.post('/cancel', t?.session ? { session_id: t.session } : {});
    } catch (e) {
      this.fail(e);
    }
  }

  /** True while this mode's conversation has a turn in flight (or this
   *  tab's prompt hasn't reached the bus yet) — gates the Stop button. */
  isStreaming(): boolean {
    if (this.pending && !pendingLanded(decoders.kind('ai').recent as AiRecord[], this.pending))
      return true;
    return this.streamingTurn() !== null;
  }

  /** A UI-side notice (slash-command errors, help) — tab-local. */
  pushMeta(label: string): void {
    this.notes = [...this.notes, { t: Date.now(), label }];
  }
}

export const ai = new AiStore();

/** Markdown → sanitized HTML for the AI's text chunks. The AI's
 *  output may contain Markdown (headings, lists, code blocks, bold);
 *  rendering it as plain `<div class="whitespace-pre-wrap">` produces
 *  a `**bold**` / `# heading` literal soup. Pipe through `marked`
 *  for HTML, then DOMPurify to scrub script tags / event handlers
 *  (defensive — AI output is mostly trusted but tool_result content
 *  can contain anything the AI happens to be reading).
 *
 *  Synchronous so callers can use it in derived state without
 *  flickering through "still rendering…" placeholders. */
export function renderMarkdown(text: string): string {
  if (!text) return '';
  const html = marked.parse(text) as string;
  return DOMPurify.sanitize(html);
}
