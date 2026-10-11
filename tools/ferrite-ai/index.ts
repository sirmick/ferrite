#!/usr/bin/env node
// Ferrite AI sidecar — runs Claude Code through the ACP adapter
// (`claude-agent-acp`, see acp.ts) and exposes:
//
//   /ws/chat       the AI panel's control channel (send a prompt, stop,
//                  reset; hello with modes/models; the session binding)
//   POST /prompt   start a turn over REST (ferrited proxies it as
//                  /api/ai/prompt; `ferrite-ctl ai-prompt`)
//   POST /cancel   stop a turn
//   GET  /sessions the live sessions
//
// The conversation itself — user prompts, thinking, reply text, tool
// calls and results, plans — is published to ferrited's event plane
// (bus.ts) and every AI panel renders from there, whoever started the
// turn.
//
// Auth: the local Claude Code login the adapter inherits from your
// `claude` CLI session — subscription billing, not per-token API.

import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { createServer, type IncomingMessage as HttpRequest, type ServerResponse } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocketServer, type WebSocket } from "ws";
import type * as acpTypes from "@agentclientprotocol/sdk";

import { AcpHost } from "./acp.js";
import { connectBus, publishReset, SessionPublisher } from "./bus.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PROMPTS_DIR = join(__dirname, "prompts");
const PORT = Number(process.env.FERRITE_AI_PORT ?? 10002);

// Resolve the Ferrite project root + the ferrite-ctl binary path so we
// can hand the AI absolute paths in its system prompt. Override either
// via `FERRITE_HOME` / `FERRITE_CTL` env vars; defaults assume this
// sidecar lives at `<repo>/tools/ferrite-ai/`.
const FERRITE_HOME = process.env.FERRITE_HOME
  ? resolve(process.env.FERRITE_HOME)
  : resolve(__dirname, "..", "..");
const FERRITE_CTL =
  process.env.FERRITE_CTL ?? resolve(FERRITE_HOME, "target/release/ferrite-ctl");

console.log(`[ferrite-ai] FERRITE_HOME=${FERRITE_HOME}`);
console.log(`[ferrite-ai] FERRITE_CTL=${FERRITE_CTL}`);

// State directory: the readable transcript log and the operator's
// station notes. run.sh points it at a repo-local, gitignored dir.
// (Conversations themselves live in Claude Code's session store, which
// the adapter resumes from, and on the event plane.)
const STATE_DIR = process.env.FERRITE_AI_STATE_DIR ?? "/tmp/ferrite-ai-state";
// Station notes the operator keeps on disk (antenna ports, local
// interference, operating rules). The browser sends its own
// `setup_description` from localStorage; a headless driver has none,
// so fall back to this file. Read per session so edits land without a
// sidecar restart.
const SETUP_FILE = process.env.FERRITE_AI_SETUP_FILE ?? join(STATE_DIR, "setup.md");
function defaultSetupDescription(): string | undefined {
  try {
    return existsSync(SETUP_FILE) ? readFileSync(SETUP_FILE, "utf-8") : undefined;
  } catch {
    return undefined;
  }
}
try {
  mkdirSync(STATE_DIR, { recursive: true });
} catch (e) {
  console.warn(`[ferrite-ai] could not create state dir ${STATE_DIR}:`, e);
}
console.log(`[ferrite-ai] state=${STATE_DIR}`);

// Human-readable record of what the AI said + what tools it ran, for
// grepping when the user reports "the AI did something weird." Tail with:
//   tail -F "$FERRITE_AI_STATE_DIR/transcript.log"
const TRANSCRIPT_PATH =
  process.env.FERRITE_AI_TRANSCRIPT ?? join(STATE_DIR, "transcript.log");
console.log(`[ferrite-ai] transcript=${TRANSCRIPT_PATH}`);

function logLine(line: string): void {
  const ts = new Date().toISOString().slice(11, 23);
  const stamped = `${ts} ${line}`;
  console.log(`[transcript] ${stamped}`);
  try {
    appendFileSync(TRANSCRIPT_PATH, stamped + "\n");
  } catch {
    // Filesystem unavailable / permission denied — stdout is enough,
    // don't crash the sidecar over a logging miss.
  }
}

function oneLine(s: string, max = 240): string {
  // Collapse newlines + runs of whitespace so each transcript line is
  // grep-friendly; the full text is on the bus.
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > max ? `${t.slice(0, max)}…` : t;
}

// Modes pre-loaded once; per-turn we pick by name. Adding a new mode
// is "drop a markdown file in prompts/ and add it to MODES."
const MODES = ["explorer", "decoder", "diagnose", "chat"] as const;
type Mode = (typeof MODES)[number];
const DEFAULT_MODE: Mode = "explorer";

const SYSTEM_PROMPTS: Record<Mode, string> = Object.fromEntries(
  MODES.map((m) => [m, loadPrompt(m)]),
) as Record<Mode, string>;

function loadPrompt(name: string): string {
  const path = join(PROMPTS_DIR, `${name}.md`);
  if (!existsSync(path)) {
    throw new Error(`prompt missing: ${path}`);
  }
  // Prompts are plain text: the AI has no shell or file access, so
  // they carry no paths (a leftover `{{PLACEHOLDER}}` is a bug).
  const text = readFileSync(path, "utf-8");
  const left = text.match(/\{\{[A-Z_]+\}\}/);
  if (left) throw new Error(`prompt ${name}.md: unresolved ${left[0]}`);
  return text;
}

// Per-mode model. Without an explicit `model` the Agent SDK falls back
// to the bundled CLI's default, which silently lagged a generation
// (sessions were running claude-opus-4-5 while Opus 5.5 was current).
// Tool-driving modes get the current Opus; tools-free chat gets Haiku
// (it's Q&A over a system prompt — latency matters more than depth).
// Override globally with FERRITE_AI_MODEL, or per mode with
// FERRITE_AI_MODEL_<MODE> (e.g. FERRITE_AI_MODEL_CHAT=claude-opus-5-5).
const MODEL_BY_MODE: Record<Mode, string> = {
  explorer: "claude-opus-5-5",
  decoder: "claude-opus-5-5",
  diagnose: "claude-opus-5-5",
  chat: "claude-haiku-5-5",
};
function modelFor(mode: Mode, override?: string): string {
  if (override && MODEL_OPTIONS.includes(override)) return override;
  return (
    process.env[`FERRITE_AI_MODEL_${mode.toUpperCase()}`] ??
    process.env.FERRITE_AI_MODEL ??
    MODEL_BY_MODE[mode]
  );
}
/** Ids the UI picker may select. An unknown override is ignored (the
 *  mode default applies) rather than forwarded — a typo'd id would
 *  otherwise 400 every turn. */
const MODEL_OPTIONS: string[] = [
  "claude-opus-5-5",
  "claude-sonnet-5-5",
  "claude-haiku-5-5",
];

// No built-in tools in any mode — no shell, no file access. The AI's
// whole surface is the `ferrite` MCP server (governed by `mcpServers`,
// which chat mode omits): every action shows in the operator's AI tab
// (tool cards, analysis cards, images), and the radio assistant can't
// touch the box it runs on.
//
// Two distinct Agent SDK knobs, easy to conflate:
//   `tools`        — the *base set* of built-in tools the model can
//                    see at all (`[]` = none). This is the restriction.
//   `allowedTools` — tools auto-approved without a permission prompt.
//                    Under `bypassPermissions` it's moot, but keep it
//                    explicit so a future switch to `default` mode
//                    doesn't start prompting.
// Passing only `allowedTools: []` (the pre-fix shape) restricted
// nothing: chat mode had Bash/Write/Edit under bypassPermissions.
const BUILTIN_TOOLS: string[] = [];
function allowedToolsFor(mode: Mode): string[] {
  return mode === "chat" ? [] : ["mcp__ferrite"];
}

// `ferrite-ctl mcp` config the Agent SDK spawns on session start. The
// subprocess hosts the same control surface the human-facing CLI does,
// over MCP JSON-RPC — so Claude calls `mcp__ferrite__status`,
// `mcp__ferrite__tune`, etc. directly instead of shell-execing
// `ferrite-ctl status --json` through `Bash`. Tool source:
// `tools/ferrite-ctl/src/mcp.rs`. The MCP subprocess proxies to the
// same daemon the human CLI talks to (default 127.0.0.1:10001),
// overridable via `FERRITE_CTL_CONNECT`.
const FERRITE_MCP_ARGS = ["mcp"];
const FERRITE_CTL_CONNECT =
  process.env.FERRITE_CTL_CONNECT ?? "http://127.0.0.1:10001";

// AI session bus: every turn is also published onto ferrited's event
// plane (`ai` kind) so all panels — and agents — see every session.
void connectBus(FERRITE_CTL_CONNECT);

function ferriteMcpServer(): acpTypes.McpServer {
  return {
    name: "ferrite",
    command: FERRITE_CTL,
    args: [...FERRITE_MCP_ARGS, "--connect", FERRITE_CTL_CONNECT],
    env: [{ name: "RUST_LOG", value: process.env.FERRITE_AI_MCP_LOG ?? "info" }],
  };
}

type IncomingMessage = {
  /** Control envelope shape. `{type: "stop"}` aborts the current
   *  turn; `{type: "request_snapshot", session_id?}` asks for the
   *  authoritative transcript replay; `{type: "reset_session"}` drops
   *  the SDK session binding and rolls a fresh transcript. Plain text
   *  messages have no `type` and use `text`. */
  type?: "stop" | "request_snapshot" | "reset_session" | string;
  text?: string;
  /** Per-turn model override (a Claude model id). Absent → the mode's
   *  default from `MODEL_BY_MODE` / env. The UI picker sends this. */
  model?: string;
  /** Session id the browser believes it is continuing — sent on
   *  `request_snapshot` (on WS connect) so the sidecar, which starts
   *  each connection with no session, knows which transcript to
   *  replay and adopt as the binding. */
  session_id?: string;
  /** Free-text reason for a `reset_session` (e.g. `clear`), echoed
   *  back on the `session_reset` envelope for the UI banner. */
  reason?: string;
  mode?: Mode | string;
  /** Caller-supplied resume id — when present, takes precedence over
   *  the per-connection `sessionId` the WS handler tracks. Used by
   *  the SvelteKit UI to continue a conversation across browser
   *  reloads (the connection is brand new but the SDK session is
   *  still on disk under that id). */
  resume_session_id?: string;
  /** User-supplied description of the physical radio setup (which
   *  antenna is hooked to what, where it's located, known interferers,
   *  etc.). Appended to the mode's system prompt so the AI doesn't
   *  have to guess what Antenna A is or why Antenna C "has nothing
   *  useful." Empty / absent = use the bare mode prompt. */
  setup_description?: string;
  /** Driver-specific operator notes pulled from the active SDR
   *  preset's `ai_operator_notes` field (e.g. SDRplay RSPdx antenna
   *  mapping + gain semantics + AGC interaction). The frontend reads
   *  this from `sdr-presets/<driver>.json` so it stays the same
   *  source of truth the UI tooltips use. Appended ahead of the
   *  operator-supplied setup so the AI sees device capabilities
   *  before reading what the operator did with them. */
  driver_notes?: string;
  /** Preset-level note from `flowgraphs/<name>.json::ai_notes` — when
   *  the AI would pick this preset, what signal it expects, gotchas.
   *  Empty / absent during the schema rollout (see
   *  `docs/13-ai-notes-migration.md`). */
  preset_notes?: string;
  /** Per-block notes for the blocks currently in the pipeline. The UI
   *  assembles this by joining `/api/pipeline/blocks` (which blocks
   *  are live) against `/api/blocks` (their schema + ai_notes), so
   *  the AI only sees notes for blocks it can actually tweak. Empty /
   *  absent during the schema rollout. */
  block_notes?: BlockNote[];
  /** Live snapshot of ferrited's source state at the moment the turn
   *  was issued — centre, rate, antenna, gain, AGC. Replaces the
   *  AI's need to call `ferrite-ctl status` to learn what the radio
   *  is actually doing; without this the AI was reasoning from
   *  memory of its last tune command, which got stale every time a
   *  preset reload or another caller changed source state between
   *  turns. */
  current_source?: CurrentSource;
  /** Operator's wall-clock time and IANA timezone, captured on each
   *  turn from the browser. Gives the AI a sense of local time (for
   *  band/propagation reasoning) and rough geographic region (via
   *  the TZ string, e.g. `America/Los_Angeles`). */
  local_time_iso?: string;
  local_time_human?: string;
  time_zone?: string;
};

/** One entry in the active-pipeline block-notes section. The UI sends
 *  these per turn; the sidecar pastes them into a heading + bullet
 *  list. `params` may be empty (utility blocks). */
type BlockNote = {
  id: string;
  type_name: string;
  ai_notes?: string;
  params?: Array<{ key: string; label?: string; ai_notes?: string }>;
};

/** Per-turn live snapshot of ferrited's `SourceConfig`. The UI curates
 *  this from `pipeline.source` + `pipeline.sourceCaps` — only the
 *  fields useful for AI reasoning, not the entire `params` dict
 *  (omits `args`, driver `settings`, etc.). */
type CurrentSource = {
  type: string;
  device_label?: string;
  center_freq_hz?: number;
  sample_rate_hz?: number;
  bandwidth_hz?: number;
  antenna?: string;
  gain_db?: number;
  agc?: boolean;
};

/** Render the per-block notes section. Blocks with both block-level
 *  prose and per-param prose get a full sub-section; blocks with only
 *  one or the other get a compact one-line entry. Empty notes
 *  (rollout state) are skipped entirely so the prompt stays clean. */
function renderBlockNotes(blocks: BlockNote[] | undefined): string {
  if (!blocks || blocks.length === 0) return "";
  const sections: string[] = [];
  for (const b of blocks) {
    const head = (b.ai_notes ?? "").trim();
    const params = (b.params ?? []).filter((p) => (p.ai_notes ?? "").trim());
    if (!head && params.length === 0) continue;
    let body = `### \`${b.id}\` — ${b.type_name}\n`;
    if (head) body += `\n${head}\n`;
    if (params.length) {
      body += `\nParams:\n`;
      for (const p of params) {
        const lbl = p.label ? ` (${p.label})` : "";
        body += `- \`${p.key}\`${lbl} — ${(p.ai_notes ?? "").trim()}\n`;
      }
    }
    sections.push(body);
  }
  if (sections.length === 0) return "";
  return `\n\n## Active pipeline — block notes\n\n${sections.join("\n")}`;
}

/** Render a "Current radio state" section from the per-turn source
 *  snapshot. Emitted between the driver-capability notes and the
 *  preset notes so the AI reads, in order: what the SDR *can* do →
 *  what the SDR *is currently doing* → what the active preset *is
 *  for*. Without this section the AI had to call `ferrite-ctl status`
 *  every time it wanted to know the current freq, and it routinely
 *  forgot — leading to stale-frequency reasoning across turns
 *  (e.g. issuing a "433 MHz capture" while the radio was on 915). */
function renderCurrentSource(s: CurrentSource | undefined): string {
  if (!s) return "";
  const lines: string[] = [];
  if (s.device_label) lines.push(`- Device: ${s.device_label}`);
  lines.push(`- Source type: \`${s.type}\``);
  if (s.center_freq_hz !== undefined) {
    lines.push(
      `- Centre frequency: **${(s.center_freq_hz / 1e6).toFixed(3)} MHz**`,
    );
  }
  if (s.sample_rate_hz !== undefined) {
    lines.push(
      `- Sample rate: ${(s.sample_rate_hz / 1e6).toFixed(3)} MS/s`,
    );
  }
  if (s.bandwidth_hz !== undefined) {
    lines.push(
      `- Analog bandwidth: ${(s.bandwidth_hz / 1e6).toFixed(3)} MHz`,
    );
  }
  if (s.antenna) lines.push(`- Antenna: ${s.antenna}`);
  if (s.gain_db !== undefined) lines.push(`- Gain: ${s.gain_db.toFixed(1)} dB`);
  if (s.agc !== undefined) {
    lines.push(
      `- AGC: ${s.agc ? "enabled (driver manages gain)" : "disabled (manual gain in effect)"}`,
    );
  }
  return (
    `\n\n## Current radio state — live snapshot from ferrited\n\n` +
    `This is the source state at the moment this turn was issued. ` +
    `Reason from these values rather than memory of recent tune ` +
    `commands — preset reloads or other callers can change source ` +
    `state between turns, and this section reflects the post-change ` +
    `reality.\n\n` +
    `${lines.join("\n")}\n`
  );
}
/** The session's standing instructions: mode prompt → driver notes
 *  (what the SDR can do) → the operator's station notes. Fixed for the
 *  session's life; a change starts a new session. */
function systemPromptFor(mode: Mode, driverNotes: string | undefined, setup: string | undefined): string {
  let out = SYSTEM_PROMPTS[mode];
  const driver = (driverNotes ?? "").trim();
  if (driver) out += `\n\n## Active SDR — driver-specific operator notes\n\n${driver}\n`;
  const op = (setup ?? "").trim();
  if (op) out += `\n\n## Operator-supplied radio setup\n\n${op}\n`;
  return out;
}

/** Per-turn context, sent as a text block ahead of the user's words:
 *  the live source state, the clock, and the preset / block notes when
 *  they changed since the session last saw them. */
function turnContext(m: IncomingMessage, seen: { notes?: string }): string {
  let out = renderCurrentSource(m.current_source);
  const notes =
    ((m.preset_notes ?? "").trim() ? `\n\n## Active preset — AI notes\n\n${m.preset_notes!.trim()}\n` : "") +
    renderBlockNotes(m.block_notes);
  if (notes && notes !== seen.notes) {
    out += notes;
    seen.notes = notes;
  }
  const tz = (m.time_zone ?? "").trim();
  const human = (m.local_time_human ?? "").trim();
  const iso = (m.local_time_iso ?? new Date().toISOString()).trim();
  out +=
    `\n\n## Local clock + region\n\n` +
    (human ? `- Operator's local time: ${human}\n` : "") +
    `- ISO (UTC): ${iso}\n` +
    (tz
      ? `- IANA timezone: ${tz} (gives approximate region — e.g. ` +
        `\`America/Los_Angeles\` → US west coast → use for propagation, ` +
        `MUF, broadcast-band makeup, and band-condition reasoning)\n`
      : "");
  return `<turn-context>\n${out.trim()}\n</turn-context>`;
}

// ── sessions ───────────────────────────────────────────────────────────

const host = new AcpHost();

interface Session {
  id: string;
  mode: Mode;
  model: string;
  pub: SessionPublisher;
  /** The running turn, if any (one at a time). */
  turn: Promise<TurnResult> | null;
  /** This turn's reply text, for REST callers that wait. */
  reply: string;
  seen: { notes?: string };
}

interface TurnResult {
  session_id: string;
  mode: Mode;
  model: string;
  status: "done" | "stopped" | "error";
  stop_reason?: string;
  error?: string;
  /** The reply text of the turn (all text blocks, joined). */
  text: string;
}

const sessions = new Map<string, Session>();
/** The session a caller with no binding continues, per mode — the last
 *  one used. REST prompts land here by default. */
const latest = new Map<Mode, string>();

function sessionSpec(mode: Mode, model: string, m: IncomingMessage) {
  const options: Record<string, unknown> = {
    model,
    tools: BUILTIN_TOOLS,
    allowedTools: allowedToolsFor(mode),
    // SDK isolation. The bundled CLI otherwise behaves like the
    // operator's interactive `claude`: it loads ~/.claude settings,
    // project .mcp.json, plugins — and (CLI ≥ 2.1) the account's
    // claude.ai cloud connectors. That handed a "tools-free" chat
    // session Google Drive write/share/trash tools under
    // bypassPermissions. The radio sidecar gets exactly what this
    // file declares and nothing else.
    settingSources: [],
    strictMcpConfig: true,
    settings: { disableClaudeAiConnectors: true },
    // Thinking on, summarised so it streams as readable text
    // (`agent_thought_chunk`) — the AI panel shows it.
    thinking: { type: "adaptive", display: "summarized" },
    ...(process.env.FERRITE_AI_EFFORT ? { effort: process.env.FERRITE_AI_EFFORT } : {}),
  };
  return {
    cwd: FERRITE_HOME,
    systemPrompt: systemPromptFor(
      mode,
      m.driver_notes,
      m.setup_description?.trim() || defaultSetupDescription(),
    ),
    options,
    // The local `ferrite` MCP server — the radio's control surface.
    // Skipped for chat mode (tools-free).
    mcpServers: mode === "chat" ? [] : [ferriteMcpServer()],
  };
}

function track(id: string, mode: Mode, model: string): Session {
  const s: Session = {
    id,
    mode,
    model,
    pub: new SessionPublisher(id, mode, model),
    turn: null,
    reply: "",
    seen: {},
  };
  sessions.set(id, s);
  latest.set(mode, id);
  return s;
}

/** Route a session's stream to its publisher + the transcript log. */
function listenerFor(id: string): (u: acpTypes.SessionNotification["update"]) => void {
  return (u) => {
    const s = sessions.get(id);
    if (!s) return;
    if ((u._meta as { ferriteAdapterExited?: boolean } | undefined)?.ferriteAdapterExited) {
      // The adapter died; the session is gone with it (a later turn
      // naming it resumes from Claude Code's store).
      sessions.delete(id);
      return;
    }
    if (u.sessionUpdate === "agent_message_chunk" && u.content.type === "text") s.reply += u.content.text;
    // Text blocks either side of a tool call are separate paragraphs.
    if (u.sessionUpdate === "tool_call" && s.reply && !s.reply.endsWith("\n")) s.reply += "\n\n";
    if (u.sessionUpdate === "tool_call") logLine(`TOOL:    ${u.title}  ${oneLine(JSON.stringify(u.rawInput ?? {}))}`);
    if (u.sessionUpdate === "tool_call_update" && (u.status === "completed" || u.status === "failed")) {
      logLine(`RESULT:  ${u.status === "failed" ? "ERR " : "OK  "}${u.toolCallId.slice(0, 8)}…`);
    }
    s.pub.update(u);
  };
}

/** The session a turn runs in: the named one if it's ours (or Claude
 *  Code can resume it), else a fresh one. A mode mismatch is a fresh
 *  session — a different system prompt. */
async function sessionFor(m: IncomingMessage, mode: Mode, fresh: boolean): Promise<Session> {
  const model = modelFor(mode, m.model);
  const want = fresh ? undefined : (m.resume_session_id ?? m.session_id ?? undefined);
  let s = want ? sessions.get(want) : undefined;
  if (s && s.mode !== mode) {
    publishReset(s.id, s.mode, `mode → ${mode}`);
    s = undefined;
  } else if (!s && want) {
    // Not live here (sidecar restart): Claude Code keeps the session
    // on disk — pick it up without replaying (the bus has the history).
    try {
      const spec = sessionSpec(mode, model, m);
      const conn = await host.connection();
      await conn.resumeSession({
        sessionId: want,
        cwd: spec.cwd,
        mcpServers: spec.mcpServers,
        _meta: { systemPrompt: spec.systemPrompt, claudeCode: { options: spec.options } },
      });
      host.listen(want, listenerFor(want));
      await host.bypassPermissions(want);
      s = track(want, mode, model);
      console.log(`[ferrite-ai] resumed session ${want}`);
    } catch (e) {
      console.warn(`[ferrite-ai] could not resume ${want}: ${String(e)}; starting fresh`);
    }
  }
  if (s) {
    if (s.model !== model) {
      try {
        const conn = await host.connection();
        await conn.setSessionConfigOption({ sessionId: s.id, configId: "model", value: model });
        s.model = model;
        s.pub = new SessionPublisher(s.id, mode, model);
      } catch (e) {
        console.warn(`[ferrite-ai] model switch → ${model} refused: ${String(e)}`);
      }
    }
    latest.set(mode, s.id);
    return s;
  }
  let id = "";
  id = await host.newSession(sessionSpec(mode, model, m), (u) => listenerFor(id)(u));
  console.log(`[ferrite-ai] new session ${id} (mode=${mode} model=${model})`);
  return track(id, mode, model);
}

class BusyError extends Error {}

/** Run one turn. Resolves once the session is known (`started`) and
 *  again when the turn ends (`done`). */
async function startTurn(
  m: IncomingMessage,
  opts: { fresh?: boolean } = {},
): Promise<{ session: Session; done: Promise<TurnResult> }> {
  const text = (m.text ?? "").trim();
  const mode = (m.mode && MODES.includes(m.mode as Mode) ? m.mode : DEFAULT_MODE) as Mode;
  const s = await sessionFor(m, mode, opts.fresh ?? false);
  if (s.turn) throw new BusyError(`session ${s.id} is busy with a turn`);
  s.reply = "";
  logLine(`──── turn  mode=${mode} model=${s.model} session=${s.id.slice(0, 8)}`);
  logLine(`USER:    ${oneLine(text)}`);
  s.pub.user(text);
  const blocks: acpTypes.ContentBlock[] = [
    { type: "text", text: turnContext(m, s.seen) },
    { type: "text", text },
  ];
  const done = (async (): Promise<TurnResult> => {
    const base = { session_id: s.id, mode, model: s.model };
    try {
      const stop = await host.prompt(s.id, blocks);
      const status = stop === "cancelled" ? "stopped" : "done";
      s.pub.finish(status);
      logLine(`──── ${status} (${stop})`);
      return { ...base, status, stop_reason: stop, text: s.reply };
    } catch (e) {
      const msg = e instanceof Error ? e.message : JSON.stringify(e);
      s.pub.finish("error", msg);
      logLine(`ERROR:   ${msg}`);
      console.error("[ferrite-ai] turn failed:", e);
      return { ...base, status: "error", error: msg, text: s.reply };
    } finally {
      s.turn = null;
    }
  })();
  s.turn = done;
  return { session: s, done };
}

async function cancel(id: string | undefined): Promise<string[]> {
  const ids = id ? [id] : [...sessions.values()].filter((s) => s.turn).map((s) => s.id);
  const hit: string[] = [];
  for (const sid of ids) {
    if (!sessions.get(sid)?.turn) continue;
    await host.cancel(sid).catch((e) => console.warn(`[ferrite-ai] cancel ${sid}: ${String(e)}`));
    hit.push(sid);
  }
  return hit;
}

// ── WebSocket: the AI panel's control channel ──────────────────────────

function hello() {
  return {
    type: "ferrite_ai_hello",
    modes: MODES,
    mode: DEFAULT_MODE,
    // Per-mode default model (after env overrides) + the ids the UI
    // picker offers.
    models: Object.fromEntries(MODES.map((m) => [m, modelFor(m)])),
    model_options: MODEL_OPTIONS,
  };
}

function send(ws: WebSocket, payload: unknown): void {
  if (ws.readyState !== ws.OPEN) return;
  ws.send(JSON.stringify(payload));
}

function parseIncoming(raw: unknown): IncomingMessage {
  const text = String(raw).trim();
  if (!text) return {};
  // Accept either a JSON envelope or a plain-text body for ergonomics
  // (a tester poking with `wscat -c ... -x "hello"` shouldn't have to
  // remember the JSON shape).
  if (text.startsWith("{")) {
    try {
      const parsed = JSON.parse(text);
      if (typeof parsed === "object" && parsed !== null) return parsed as IncomingMessage;
    } catch {
      /* fall through to plain text */
    }
  }
  return { text };
}

const httpServer = createServer((req, res) => {
  void handleHttp(req, res);
});
const wss = new WebSocketServer({ server: httpServer, path: "/ws/chat" });

wss.on("connection", (ws) => {
  // The session this panel is bound to (its last turn's), for stop.
  let bound: string | null = null;
  console.log("[ferrite-ai] client connected");
  send(ws, hello());

  ws.on("message", async (raw) => {
    const m = parseIncoming(raw);
    if (!m.text) {
      if (m.type === "stop") {
        const hit = bound ? await cancel(bound) : [];
        if (!hit.length) send(ws, { type: "ferrite_ai_stopped", reason: "idle" });
        return;
      }
      // The panel names the session it thinks it's continuing. Claude
      // Code keeps sessions on disk, so any id is resumable on the
      // next turn — adopt it. (The transcript itself is on the bus.)
      if (m.type === "request_snapshot") {
        bound = m.session_id ?? bound;
        if (bound) send(ws, { type: "conversation_snapshot", session_id: bound, events: [] });
        else send(ws, { type: "session_reset", reason: "no prior session" });
        return;
      }
      if (m.type === "reset_session") {
        const mode = (m.mode && MODES.includes(m.mode as Mode) ? m.mode : DEFAULT_MODE) as Mode;
        publishReset(bound, (bound && sessions.get(bound)?.mode) || mode, m.reason ?? "reset");
        if (bound && latest.get(sessions.get(bound)?.mode ?? mode) === bound) {
          latest.delete(sessions.get(bound)!.mode);
        }
        bound = null;
        // The banner is on the session bus (every panel); the
        // requesting tab must not add a local one too.
        send(ws, { type: "session_reset", reason: m.reason ?? "reset", on_bus: true });
        return;
      }
      send(ws, {
        type: "ferrite_ai_error",
        message: "expected JSON {text, mode?} or a control {type: stop|request_snapshot|reset_session}",
      });
      return;
    }
    try {
      // A panel with no binding starts its own conversation.
      const { session, done } = await startTurn(m, { fresh: !m.resume_session_id });
      bound = session.id;
      send(ws, { type: "system", subtype: "init", session_id: session.id });
      const r = await done;
      if (r.status === "stopped") send(ws, { type: "ferrite_ai_stopped", reason: "user" });
      else if (r.status === "error") send(ws, { type: "ferrite_ai_error", message: r.error });
      else send(ws, { type: "ferrite_ai_done", mode: r.mode });
    } catch (e) {
      send(ws, { type: "ferrite_ai_error", message: e instanceof Error ? e.message : String(e) });
    }
  });

  ws.on("close", () => {
    // Turns run on without the panel: the bus carries them to every
    // other panel, and this one picks the session back up on reconnect.
    console.log("[ferrite-ai] client disconnected");
  });
});

// ── REST ───────────────────────────────────────────────────────────────

function reply(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

async function readJson(req: HttpRequest): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const raw = Buffer.concat(chunks).toString("utf-8").trim();
  if (!raw) return {};
  const v = JSON.parse(raw) as unknown;
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error("body must be a JSON object");
  return v as Record<string, unknown>;
}

async function handleHttp(req: HttpRequest, res: ServerResponse): Promise<void> {
  const path = (req.url ?? "/").split("?")[0];
  try {
    if (req.method === "POST" && path === "/prompt") {
      // {text, mode?, model?, session_id?, new_session?, wait?} — the
      // turn runs exactly as a panel's does (same bus records), so it
      // shows on every AI panel. Without `session_id` it continues the
      // mode's latest session; `new_session` starts a fresh one.
      const body = await readJson(req);
      const m = body as IncomingMessage & { new_session?: boolean; wait?: boolean };
      if (!(m.text ?? "").trim()) return reply(res, 400, { error: "text is required" });
      const mode = (m.mode && MODES.includes(m.mode as Mode) ? m.mode : DEFAULT_MODE) as Mode;
      const sid = m.session_id ?? (m.new_session ? undefined : latest.get(mode));
      const { session, done } = await startTurn(
        { ...m, mode, resume_session_id: sid },
        { fresh: Boolean(m.new_session) || !sid },
      );
      if (m.wait) return reply(res, 200, await done);
      return reply(res, 202, {
        session_id: session.id,
        mode: session.mode,
        model: session.model,
        status: "started",
      });
    }
    if (req.method === "POST" && path === "/cancel") {
      const body = await readJson(req);
      const hit = await cancel(typeof body.session_id === "string" ? body.session_id : undefined);
      return reply(res, 200, { cancelled: hit });
    }
    if (req.method === "GET" && path === "/sessions") {
      return reply(res, 200, {
        sessions: [...sessions.values()].map((s) => ({
          session_id: s.id,
          mode: s.mode,
          model: s.model,
          busy: Boolean(s.turn),
          latest: latest.get(s.mode) === s.id,
        })),
      });
    }
    if (req.method === "GET" && (path === "/" || path === "/hello")) {
      return reply(res, 200, { ...hello(), ws: "/ws/chat", rest: ["POST /prompt", "POST /cancel", "GET /sessions"] });
    }
    reply(res, 404, { error: `no route ${req.method} ${path}` });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    reply(res, e instanceof BusyError ? 409 : e instanceof SyntaxError ? 400 : 500, { error: msg });
  }
}

httpServer.listen(PORT, () => {
  console.log(
    `ferrite-ai listening on http://127.0.0.1:${PORT} (ws /ws/chat, POST /prompt; modes: ${MODES.join(", ")})`,
  );
});

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    host.stop();
    process.exit(0);
  });
}
