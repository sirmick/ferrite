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
// `/ws/chat` (proxied to the sidecar) is the control channel: send a
// prompt, stop, reset the reasoning context, and the sidecar's hello
// (modes / models) + session binding (`system/init`) for `resume`.

import DOMPurify from 'dompurify';
import { marked } from 'marked';

import { pipeline } from '$lib/pipeline.svelte';
import { aiOperatorNotesForDriver } from '$lib/controls/optionsModel';
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
const LS_SESSION = 'ferrite-ai.session_id';
const LS_SETUP = 'ferrite-ai.setup_description';
/** Pre-bus transcript cache keys — removed on load (the server keeps it). */
const LS_LEGACY = ['ferrite-ai.turns', 'ferrite-ai.next_id'];

/** A streaming turn with no record for this long ended unobserved
 *  (the sidecar died mid-turn); stop showing it as live. */
const STALE_STREAM_MS = 10 * 60_000;

const str = (v: unknown): string => (typeof v === 'string' ? v : v == null ? '' : String(v));

export function resetBannerLabel(reason: string): string {
  // After a clear there is no history above to preserve.
  if (reason === 'clear') return '── cleared — new conversation';
  return `── reasoning context reset (${reason}) — history preserved, assistant won't remember the above`;
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
      };
      out.push(a);
      open.set(key, a);
    }
    lastAt.set(a, rec.at_ms);
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
        if (tool) {
          tool.result = result;
          if (images.length) tool.resultImages = images;
          tool.error = Boolean(d['is_error']);
        } else {
          a.chunks.push({
            kind: 'tool',
            id,
            name: '(result)',
            input: '',
            result,
            resultImages: images.length ? images : undefined,
            error: Boolean(d['is_error']),
          });
        }
        break;
      }
      case 'turn_end': {
        const a = open.get(key) ?? assistantFor(rec, d);
        const status = str(d['status']);
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
    const landed = records.some(
      (r) =>
        r.data?.['type'] === 'user' &&
        str(r.data['text']) === pending.text &&
        r.at_ms >= pending.t - 5_000,
    );
    if (!landed) {
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
  mode = $state<AiMode>('explorer');
  connection = $state<ConnectionState>('idle');
  lastError = $state<string | null>(null);
  /** Available modes from the server's hello message. Defaults to the
   *  client-side list so the dropdown renders immediately on first
   *  load before the WS handshake finishes. */
  availableModes = $state<AiMode[]>(AI_MODES);
  /** Operator's model override for the next turns, or `null` for the
   *  sidecar's per-mode default. Sent as `model` on each turn; the
   *  sidecar ignores ids outside its `model_options`. */
  model = $state<string | null>(null);
  /** Model ids the sidecar offers (from `ferrite_ai_hello`). Empty
   *  until the handshake — the picker then shows only "default". */
  modelOptions = $state<string[]>([]);
  /** Per-mode default model the sidecar reports (after its env
   *  overrides), so the picker can label the default option. */
  modelDefaults = $state<Record<string, string>>({});
  /** This tab's sidecar session (from `system/init`), sent back as
   *  `resume_session_id` so the conversation continues across reloads.
   *  Null when no conversation has happened yet. */
  sessionId = $state<string | null>(null);
  /** User-supplied description of the physical radio setup — antennas,
   *  noise environment, station notes. Sent on every turn so the
   *  sidecar appends it to the mode's system prompt. Set via
   *  `setSetupDescription`, which resets the SDK session because the
   *  system prompt changed and a resume would re-prime under the old
   *  description. Persisted across reloads. */
  setupDescription = $state<string>('');

  /** Tab-local notices (slash-command output, offline banners). */
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

  private ws: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private retryCount = 0;
  private url: string | null = null;

  constructor() {
    this.restoreFromStorage();
  }

  private restoreFromStorage(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      for (const k of LS_LEGACY) localStorage.removeItem(k);
      const rawMode = localStorage.getItem(LS_MODE);
      if (rawMode && (AI_MODES as string[]).includes(rawMode)) {
        this.mode = rawMode as AiMode;
      }
      const rawModel = localStorage.getItem(LS_MODEL);
      if (rawModel) this.model = rawModel;
      const rawSession = localStorage.getItem(LS_SESSION);
      if (rawSession) this.sessionId = rawSession;
      const rawSetup = localStorage.getItem(LS_SETUP);
      if (typeof rawSetup === 'string') this.setupDescription = rawSetup;
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
      if (this.sessionId) localStorage.setItem(LS_SESSION, this.sessionId);
      else localStorage.removeItem(LS_SESSION);
      if (this.setupDescription) localStorage.setItem(LS_SETUP, this.setupDescription);
      else localStorage.removeItem(LS_SETUP);
    } catch {
      /* quota / private mode / etc — silently degrade to non-persistent */
    }
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

  /** Single authoritative reset path: tell the sidecar (the authority)
   *  to drop the SDK session binding so the LLM context resets *with*
   *  the transcript, never independently. The sidecar publishes a
   *  `reset` record (the banner every panel shows). Returns false when
   *  offline so the caller can fall back to a local notice. */
  private requestSidecarReset(reason: string): boolean {
    this.sessionId = null;
    if (typeof localStorage !== 'undefined') localStorage.removeItem(LS_SESSION);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'reset_session', reason }));
      return true;
    }
    return false;
  }

  /** Append a tab-local notice, skipping an identical consecutive one
   *  (reconnect storms must not stack banners). */
  private note(label: string): void {
    if (this.notes.at(-1)?.label === label) return;
    this.notes = [...this.notes, { t: Date.now(), label }];
  }

  /** Pick a model override (`null` = sidecar default). Takes effect on
   *  the next turn; no session reset — the SDK resumes a conversation
   *  under a different model fine (the system prompt is unchanged). */
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

  setMode(mode: AiMode): void {
    if (this.mode === mode) return;
    this.mode = mode;
    // A different mode is a different system prompt — resuming would
    // re-prime under the wrong context, so the sidecar drops the session.
    if (!this.requestSidecarReset(`mode → ${mode}`)) this.note(resetBannerLabel(`mode → ${mode}`));
    this.persist();
  }

  /** Edit the operator-supplied radio setup description. Same
   *  reasoning as `setMode`: the system prompt changes, so the sidecar
   *  drops the session binding. */
  setSetupDescription(text: string): void {
    if (text === this.setupDescription) return;
    this.setupDescription = text;
    const reason = text.trim() ? 'radio setup updated' : 'radio setup cleared';
    if (!this.requestSidecarReset(reason)) this.note(resetBannerLabel(reason));
    this.persist();
  }

  send(text: string): void {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this.lastError = 'not connected';
      return;
    }
    // Optimistic: show the prompt + a working assistant turn now; the
    // sidecar's `user` record replaces it once the session is known.
    this.pending = { text: trimmed, t: Date.now() };
    // `resume_session_id` lets the sidecar resume the prior Claude
    // Agent SDK session if we still have its id from a previous
    // exchange — the same conversation continues across browser
    // reloads and WS reconnects.
    const payload: Record<string, unknown> = { text: trimmed, mode: this.mode };
    if (this.model) payload.model = this.model;
    if (this.sessionId) payload.resume_session_id = this.sessionId;
    if (this.setupDescription.trim()) payload.setup_description = this.setupDescription;
    // Hand the AI the operator's local clock + IANA timezone on each
    // turn. The TZ identifier (e.g. `America/Los_Angeles`) gives
    // continent + region — good enough for propagation / band-plan
    // reasoning ("PST → US west coast → trans-Pacific HF in the
    // evening"). The ISO timestamp carries an explicit UTC offset so
    // the AI can compute relative times without re-resolving the TZ.
    const now = new Date();
    payload.local_time_iso = now.toISOString();
    payload.local_time_human = now.toLocaleString(undefined, {
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      hour12: false,
      timeZoneName: 'short',
    });
    payload.time_zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    // Pull driver-specific operator notes from the active source's
    // SDR preset (sdr-presets/<driver>.json `ai_operator_notes`). The
    // sidecar appends these to the mode prompt under a "driver-specific
    // operator notes" heading so the AI knows antenna mapping, gain
    // semantics, AGC interaction, etc. without us duplicating those
    // facts in the prompt files. Skipped for non-hardware sources
    // (file, sine) — they don't have a SoapySDR driver_key.
    const caps = pipeline.sourceCaps;
    if (caps && caps.kind === 'hardware') {
      const notes = aiOperatorNotesForDriver(caps.capabilities.driver_key);
      if (notes.trim()) payload.driver_notes = notes;
    }
    // Active preset's prose note (FlowgraphDoc.ai_notes) — when the AI
    // would pick this preset, what signal it expects, gotchas. Empty
    // during the schema rollout (see docs/13-ai-notes-migration.md).
    const presetNotes = (pipeline.flowgraph?.ai_notes ?? '').trim();
    if (presetNotes) payload.preset_notes = presetNotes;
    // Live source state — hand the AI ground truth for centre, rate,
    // gain, antenna, AGC at the moment this turn starts. Replaces the
    // need for the AI to call `ferrite-ctl status` mid-conversation to
    // re-learn what ferrited is actually doing; a preset reload or
    // another caller can mutate source state between turns and this
    // section reflects the post-mutation reality.
    const src = pipeline.source;
    if (src) {
      const p = (src.params ?? {}) as Record<string, unknown>;
      const curr: Record<string, unknown> = { type: src.type };
      for (const k of [
        'center_freq_hz',
        'sample_rate_hz',
        'bandwidth_hz',
        'antenna',
        'gain_db',
        'agc',
      ] as const) {
        if (p[k] !== undefined) curr[k] = p[k];
      }
      const caps = pipeline.sourceCaps;
      if (caps && caps.kind === 'hardware') {
        curr.device_label = caps.capabilities.info.label;
      }
      payload.current_source = curr;
    }
    // Per-block notes for blocks currently in the pipeline. Empty
    // blocks (notes-not-yet-authored) are filtered out by the sidecar's
    // renderer so this list can include everything without bloating
    // the prompt.
    const blockNotes = Object.values(pipeline.blocks)
      .map((b) => ({
        id: b.id,
        type_name: b.type_name,
        ai_notes: b.spec?.ai_notes ?? '',
        params: (b.spec?.params ?? []).map((p) => ({
          key: p.key,
          label: p.label,
          ai_notes: p.ai_notes ?? '',
        })),
      }))
      .filter((b) => b.ai_notes.trim() || b.params.some((p) => p.ai_notes.trim()));
    if (blockNotes.length > 0) payload.block_notes = blockNotes;
    this.ws.send(JSON.stringify(payload));
  }

  /** Hard clear: empty the AI log for *every* panel (the server's `ai`
   *  kind, which leaves a `clear` marker the panel uses to hide older
   *  activity / analysis) and drop the SDK context. `/reset` keeps the
   *  history; this wipes it.
   *
   *  The sidecar reset goes out only after the store reset lands: its
   *  "context reset" banner is an `ai` record, and a reset that arrived
   *  after it would wipe the banner too. */
  async clear(): Promise<void> {
    this.notes = [];
    this.pending = null;
    this.persist();
    const reset = (kind: string) =>
      fetch(`/api/state/${kind}/reset`, { method: 'POST' }).catch(() => undefined);
    await Promise.all([reset('ai'), reset('ai_live')]);
    this.requestSidecarReset('clear');
    this.persist();
  }

  /** `/reset` (and `/clear`, `/new`): drop the SDK context but keep
   *  the visible history. Offline we add the banner locally. */
  resetSession(opts: { reason?: string } = {}): void {
    const reason = opts.reason?.trim() || 'reset';
    if (!this.requestSidecarReset(reason)) this.note(resetBannerLabel(reason));
    this.persist();
  }

  /** Ask the sidecar to abort the in-flight turn. The `turn_end`
   *  (`stopped`) record closes it on every panel. No-op when idle. */
  stop(): void {
    if (!this.isStreaming()) return;
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'stop' }));
    }
  }

  /** True while this tab's own conversation has a turn in flight —
   *  gates the Stop button (another session's turn isn't ours to stop). */
  isStreaming(): boolean {
    if (this.pending) return true;
    for (let i = this.turns.length - 1; i >= 0; i--) {
      const t = this.turns[i];
      if (t.role !== 'assistant' || !t.chunks || (t.source && t.source !== 'ferrite-ai')) continue;
      if (this.sessionId && t.session && t.session !== this.sessionId) continue;
      return t.status === 'streaming';
    }
    return false;
  }

  /** A UI-side notice (slash-command errors, help) — tab-local. */
  pushMeta(label: string): void {
    this.notes = [...this.notes, { t: Date.now(), label }];
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
      // Ask the sidecar whether the session we think we're continuing
      // is still resumable; it answers `conversation_snapshot` (yes —
      // adopt it) or `session_reset` (no — drop the binding). The
      // transcript itself comes from the bus either way.
      if (this.sessionId && this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: 'request_snapshot', session_id: this.sessionId }));
      }
    });
    ws.addEventListener('message', (e) => {
      if (typeof e.data !== 'string') return;
      try {
        this.ingestEvent(JSON.parse(e.data) as Record<string, unknown>);
      } catch {
        /* non-JSON debug frames: ignore */
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

  /** Control-channel messages from the sidecar. The conversation itself
   *  (thinking, text, tool calls) renders from the bus, not from here. */
  ingestEvent(event: Record<string, unknown>): void {
    const t = event['type'];
    if (t === 'system' && event['subtype'] === 'init') {
      const sid = event['session_id'];
      if (typeof sid === 'string' && sid !== this.sessionId) {
        this.sessionId = sid;
        this.persist();
      }
      return;
    }
    if (t === 'conversation_snapshot') {
      const sid = event['session_id'];
      if (typeof sid === 'string') this.sessionId = sid;
      this.persist();
      return;
    }
    if (t === 'session_reset') {
      this.sessionId = null;
      if (typeof localStorage !== 'undefined') localStorage.removeItem(LS_SESSION);
      // A reset this tab asked for is published on the bus (`on_bus`);
      // one the sidecar decided alone (session unresumable) is ours to show.
      if (!event['on_bus']) {
        const reason = typeof event['reason'] === 'string' ? event['reason'] : 'reset';
        this.note(resetBannerLabel(reason));
      }
      this.persist();
      return;
    }
    if (t === 'ferrite_ai_hello') {
      const modes = event['modes'];
      if (Array.isArray(modes)) {
        const valid = modes.filter((m): m is AiMode => AI_MODES.includes(m as AiMode));
        if (valid.length) this.availableModes = valid;
      }
      const options = event['model_options'];
      if (Array.isArray(options)) {
        this.modelOptions = options.filter((o): o is string => typeof o === 'string');
        // A persisted override the sidecar no longer offers falls back
        // to the default rather than 400ing every turn.
        if (this.model && !this.modelOptions.includes(this.model)) this.model = null;
      }
      const defaults = event['models'];
      if (defaults && typeof defaults === 'object') {
        const out: Record<string, string> = {};
        for (const [k, v] of Object.entries(defaults as Record<string, unknown>)) {
          if (typeof v === 'string') out[k] = v;
        }
        this.modelDefaults = out;
      }
      return;
    }
    if (t === 'ferrite_ai_done' || t === 'ferrite_ai_stopped') {
      this.pending = null;
      return;
    }
    if (t === 'ferrite_ai_error') {
      // A turn failure also lands as a `turn_end` record; this covers
      // the control channel itself (sidecar unreachable, bad envelope).
      this.pending = null;
      this.lastError = String(event['message'] ?? 'unknown error');
      this.note(`── ${this.lastError}`);
    }
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
