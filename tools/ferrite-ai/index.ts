#!/usr/bin/env node
// Ferrite AI sidecar — runs Claude Code through the ACP adapter
// (`claude-agent-acp`, see acp.ts). One front door, plain HTTP, the same
// for every driver (ferrited proxies it as /api/ai/*):
//
//   GET  /hello     modes, per-mode default models, the model picker list
//   POST /prompt    run a turn   {text, mode?, model?, session_id?,
//                                 new_session?, wait?, time_zone?}
//   POST /cancel    stop a turn  {session_id?}  (none = every running turn)
//   POST /reset     start over   {mode?, reason?} (no mode = every mode)
//   GET  /setup     the station notes            → {text}
//   POST /setup     replace them {text}          (starts every mode over)
//   GET  /sessions  the live sessions
//
// A turn without `session_id` continues its mode's current conversation
// — the same one in every tab, the CLI and REST alike. The conversation
// itself (prompts, thinking, text, tool calls and results, plans) goes
// to ferrited's event plane (bus.ts); every AI panel renders from there,
// and which conversation is current is read back from there at boot.
//
// The context the AI gets — live radio state, driver / preset / block
// notes, the station notes, the clock — is built here from ferrited's
// own state (context.ts), so it doesn't depend on who asked.
//
// Auth: the local Claude Code login the adapter inherits from your
// `claude` CLI session — subscription billing, not per-token API.

import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { createServer, type IncomingMessage as HttpRequest, type ServerResponse } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type * as acpTypes from "@agentclientprotocol/sdk";

import { AcpHost } from "./acp.js";
import { connectBus, latestFromBus, publishReset, SessionPublisher } from "./bus.js";
import { driverNotes, readSetup, readStation, systemPrompt, turnContext, writeSetup } from "./context.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PROMPTS_DIR = join(__dirname, "prompts");
const PORT = Number(process.env.FERRITE_AI_PORT ?? 10002);

// The repo root (driver notes live in the web app) and the ferrite-ctl
// binary that serves the MCP tools. Override via FERRITE_HOME / FERRITE_CTL.
const FERRITE_HOME = process.env.FERRITE_HOME
  ? resolve(process.env.FERRITE_HOME)
  : resolve(__dirname, "..", "..");
const FERRITE_CTL =
  process.env.FERRITE_CTL ?? resolve(FERRITE_HOME, "target/release/ferrite-ctl");
// The daemon this sidecar — and the MCP server it spawns — drives. Same
// variable and default as `ferrite-ctl --connect`; run.sh sets it.
const FERRITED = (process.env.FERRITE_CTL_CONNECT ?? "http://127.0.0.1:10001").replace(/\/$/, "");

console.log(`[ferrite-ai] FERRITE_HOME=${FERRITE_HOME}`);
console.log(`[ferrite-ai] FERRITE_CTL=${FERRITE_CTL}`);
console.log(`[ferrite-ai] ferrited=${FERRITED}${process.env.FERRITE_CTL_CONNECT ? "" : "  (default — set FERRITE_CTL_CONNECT)"}`);

// State directory: the readable transcript log and the station notes.
// run.sh points it at a repo-local, gitignored dir.
const STATE_DIR = process.env.FERRITE_AI_STATE_DIR ?? "/tmp/ferrite-ai-state";
try {
  mkdirSync(STATE_DIR, { recursive: true });
} catch (e) {
  console.warn(`[ferrite-ai] could not create state dir ${STATE_DIR}:`, e);
}
// The operator's station notes (antenna ports, local interference,
// operating rules) — one copy, edited from the AI panel or by hand,
// read by every session at its start.
const SETUP_FILE = process.env.FERRITE_AI_SETUP_FILE ?? join(STATE_DIR, "setup.md");
// Station time zone for the AI's clock when the caller sends none.
const STATION_TZ = process.env.FERRITE_TZ;
console.log(`[ferrite-ai] state=${STATE_DIR}`);

// Human-readable record of what the AI said + what tools it ran, for
// grepping when the user reports "the AI did something weird." Tail with:
//   tail -F "$FERRITE_AI_STATE_DIR/transcript.log"
const TRANSCRIPT_PATH =
  process.env.FERRITE_AI_TRANSCRIPT ?? join(STATE_DIR, "transcript.log");

function logLine(line: string): void {
  const stamped = `${new Date().toISOString().slice(11, 23)} ${line}`;
  console.log(`[transcript] ${stamped}`);
  try {
    appendFileSync(TRANSCRIPT_PATH, stamped + "\n");
  } catch {
    // stdout is enough; don't crash the sidecar over a logging miss.
  }
}

function oneLine(s: string, max = 240): string {
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > max ? `${t.slice(0, max)}…` : t;
}

// Modes: a different system prompt each. Adding one is "drop a markdown
// file in prompts/ and add it to MODES."
const MODES = ["explorer", "decoder", "diagnose", "chat"] as const;
type Mode = (typeof MODES)[number];
const DEFAULT_MODE: Mode = "explorer";
const asMode = (m: unknown): Mode => (MODES.includes(m as Mode) ? (m as Mode) : DEFAULT_MODE);

const SYSTEM_PROMPTS: Record<Mode, string> = Object.fromEntries(
  MODES.map((m) => [m, loadPrompt(m)]),
) as Record<Mode, string>;

function loadPrompt(name: string): string {
  const path = join(PROMPTS_DIR, `${name}.md`);
  if (!existsSync(path)) throw new Error(`prompt missing: ${path}`);
  // Prompts are plain text: the AI has no shell or file access, so they
  // carry no paths (a leftover `{{PLACEHOLDER}}` is a bug).
  const text = readFileSync(path, "utf-8");
  const left = text.match(/\{\{[A-Z_]+\}\}/);
  if (left) throw new Error(`prompt ${name}.md: unresolved ${left[0]}`);
  return text;
}

// Per-mode model. Tool-driving modes get the current Opus; tools-free
// chat gets Haiku. Override globally with FERRITE_AI_MODEL, or per mode
// with FERRITE_AI_MODEL_<MODE> (e.g. FERRITE_AI_MODEL_CHAT=claude-opus-5-5).
const MODEL_BY_MODE: Record<Mode, string> = {
  explorer: "claude-opus-5-5",
  decoder: "claude-opus-5-5",
  diagnose: "claude-opus-5-5",
  chat: "claude-haiku-5-5",
};
/** Ids a caller may pick. An unknown override is ignored (the mode
 *  default applies) rather than forwarded — a typo'd id would otherwise
 *  fail every turn. */
const MODEL_OPTIONS: string[] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];
function modelFor(mode: Mode, override?: string): string {
  if (override && MODEL_OPTIONS.includes(override)) return override;
  return process.env[`FERRITE_AI_MODEL_${mode.toUpperCase()}`] ?? process.env.FERRITE_AI_MODEL ?? MODEL_BY_MODE[mode];
}

// No built-in tools in any mode — no shell, no file access. The AI's
// whole surface is the `ferrite` MCP server (absent in chat mode): every
// action shows in the operator's AI tab, and the radio assistant can't
// touch the box it runs on.
//
//   `tools`        — the base set of built-in tools the model sees
//                    (`[]` = none). This is the restriction.
//   `allowedTools` — tools auto-approved without a prompt. Moot under
//                    bypassPermissions, kept explicit for safety.
const BUILTIN_TOOLS: string[] = [];
const allowedToolsFor = (mode: Mode): string[] => (mode === "chat" ? [] : ["mcp__ferrite"]);

function ferriteMcpServer(): acpTypes.McpServer {
  return {
    name: "ferrite",
    command: FERRITE_CTL,
    args: ["mcp", "--connect", FERRITED],
    env: [{ name: "RUST_LOG", value: process.env.FERRITE_AI_MCP_LOG ?? "info" }],
  };
}

// AI session bus: every turn is published onto ferrited's event plane.
void connectBus(FERRITED);

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
  /** Preset / block notes this session has been shown. */
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
/** The conversation each mode continues. Seeded from the bus at boot. */
const latest = new Map<Mode, string>();
const ready = latestFromBus(FERRITED).then((m) => {
  for (const [mode, id] of m) if (MODES.includes(mode as Mode)) latest.set(mode as Mode, id);
  if (m.size) console.log(`[ferrite-ai] continuing: ${[...latest].map(([k, v]) => `${k}=${v.slice(0, 8)}`).join(" ")}`);
});

async function sessionSpec(mode: Mode, model: string) {
  const station = await readStation(FERRITED);
  const options: Record<string, unknown> = {
    model,
    tools: BUILTIN_TOOLS,
    allowedTools: allowedToolsFor(mode),
    // SDK isolation. Without these the bundled CLI behaves like the
    // operator's interactive `claude`: ~/.claude settings, project
    // .mcp.json, plugins and the account's claude.ai connectors. The
    // radio sidecar gets exactly what this file declares.
    settingSources: [],
    strictMcpConfig: true,
    settings: { disableClaudeAiConnectors: true },
    // Thinking on, summarised so it streams as readable text.
    thinking: { type: "adaptive", display: "summarized" },
    ...(process.env.FERRITE_AI_EFFORT ? { effort: process.env.FERRITE_AI_EFFORT } : {}),
  };
  return {
    cwd: FERRITE_HOME,
    systemPrompt: systemPrompt(SYSTEM_PROMPTS[mode], driverNotes(FERRITE_HOME, station.driverKey), readSetup(SETUP_FILE)),
    options,
    mcpServers: mode === "chat" ? [] : [ferriteMcpServer()],
  };
}

function track(id: string, mode: Mode, model: string): Session {
  const s: Session = { id, mode, model, pub: new SessionPublisher(id, mode, model), turn: null, reply: "", seen: {} };
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
      // The adapter died; the session goes with it (the next turn
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

/** The session a turn runs in: `want` if given (live here, or resumed
 *  from Claude Code's store after a restart), else a fresh one. A
 *  session from another mode is never continued — a different system
 *  prompt. */
async function sessionFor(want: string | undefined, mode: Mode, modelOverride?: string): Promise<Session> {
  const model = modelFor(mode, modelOverride);
  let s = want ? sessions.get(want) : undefined;
  if (s && s.mode !== mode) s = undefined;
  if (!s && want && !sessions.has(want)) {
    try {
      const spec = await sessionSpec(mode, model);
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
  id = await host.newSession(await sessionSpec(mode, model), (u) => listenerFor(id)(u));
  console.log(`[ferrite-ai] new session ${id} (mode=${mode} model=${model})`);
  return track(id, mode, model);
}

class BusyError extends Error {}

interface PromptRequest {
  text?: string;
  mode?: string;
  model?: string;
  /** Continue this session. Absent: the mode's current conversation. */
  session_id?: string;
  /** Start a fresh conversation (it becomes the mode's current one). */
  new_session?: boolean;
  /** Hold the reply until the turn ends. */
  wait?: boolean;
  /** The caller's IANA time zone, for the AI's clock (the browser sends
   *  it; otherwise FERRITE_TZ, then the sidecar host's zone). */
  time_zone?: string;
}

/** Start one turn. Resolves once the session is known; `done` resolves
 *  when the turn ends. */
async function startTurn(m: PromptRequest): Promise<{ session: Session; done: Promise<TurnResult> }> {
  await ready;
  const text = (m.text ?? "").trim();
  const mode = asMode(m.mode);
  const want = m.new_session ? undefined : (m.session_id ?? latest.get(mode));
  const s = await sessionFor(want, mode, m.model);
  if (s.turn) throw new BusyError(`the ${mode} conversation is busy with a turn`);
  s.reply = "";
  logLine(`──── turn  mode=${mode} model=${s.model} session=${s.id.slice(0, 8)}`);
  logLine(`USER:    ${oneLine(text)}`);
  s.pub.user(text);
  const station = await readStation(FERRITED);
  const blocks: acpTypes.ContentBlock[] = [
    { type: "text", text: turnContext(station, s.seen, new Date(), m.time_zone || STATION_TZ) },
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

/** Start over: the next turn of `mode` (or of every mode) opens a fresh
 *  conversation. One `reset` record is the banner every panel shows. */
function reset(mode: Mode | null, reason: string): void {
  publishReset(mode ? (latest.get(mode) ?? null) : null, mode, reason);
  if (mode) latest.delete(mode);
  else latest.clear();
  logLine(`──── reset ${mode ?? "all"} (${reason})`);
}

// ── HTTP ───────────────────────────────────────────────────────────────

function hello() {
  return {
    type: "ferrite_ai_hello",
    modes: MODES,
    mode: DEFAULT_MODE,
    models: Object.fromEntries(MODES.map((m) => [m, modelFor(m)])),
    model_options: MODEL_OPTIONS,
  };
}

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
  const path = (req.url ?? "/").split("?")[0].replace(/\/$/, "") || "/";
  const route = `${req.method} ${path}`;
  try {
    switch (route) {
      case "GET /":
      case "GET /hello":
        return reply(res, 200, hello());
      case "POST /prompt": {
        const m = (await readJson(req)) as PromptRequest;
        if (!(m.text ?? "").trim()) return reply(res, 400, { error: "text is required" });
        const { session, done } = await startTurn(m);
        if (m.wait) return reply(res, 200, await done);
        return reply(res, 202, { session_id: session.id, mode: session.mode, model: session.model, status: "started" });
      }
      case "POST /cancel": {
        const body = await readJson(req);
        const hit = await cancel(typeof body.session_id === "string" ? body.session_id : undefined);
        return reply(res, 200, { cancelled: hit });
      }
      case "POST /reset": {
        const body = await readJson(req);
        const mode = body.mode == null ? null : asMode(body.mode);
        reset(mode, typeof body.reason === "string" && body.reason.trim() ? body.reason.trim() : "reset");
        return reply(res, 200, { ok: true, mode });
      }
      case "GET /setup":
        return reply(res, 200, { text: readSetup(SETUP_FILE) });
      case "POST /setup": {
        const body = await readJson(req);
        if (typeof body.text !== "string") return reply(res, 400, { error: "text (string) is required" });
        if (body.text.trim() === readSetup(SETUP_FILE).trim()) return reply(res, 200, { ok: true, changed: false });
        writeSetup(SETUP_FILE, body.text);
        // The notes are in every session's system prompt: start over.
        reset(null, body.text.trim() ? "station notes updated" : "station notes cleared");
        return reply(res, 200, { ok: true, changed: true });
      }
      case "GET /sessions":
        return reply(res, 200, {
          sessions: [...sessions.values()].map((s) => ({
            session_id: s.id,
            mode: s.mode,
            model: s.model,
            busy: Boolean(s.turn),
            latest: latest.get(s.mode) === s.id,
          })),
          latest: Object.fromEntries(latest),
        });
      default:
        return reply(res, 404, { error: `no route ${route}` });
    }
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    reply(res, e instanceof BusyError ? 409 : e instanceof SyntaxError ? 400 : 500, { error: msg });
  }
}

const httpServer = createServer((req, res) => {
  void handleHttp(req, res);
});
httpServer.listen(PORT, "127.0.0.1", () => {
  console.log(`ferrite-ai listening on http://127.0.0.1:${PORT} (modes: ${MODES.join(", ")})`);
});

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    host.stop();
    process.exit(0);
  });
}
