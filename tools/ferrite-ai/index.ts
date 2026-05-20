#!/usr/bin/env node
// Ferrite AI sidecar — wraps Claude Code (via @anthropic-ai/claude-agent-sdk)
// and exposes a chat WebSocket the SvelteKit UI hooks into. Each WS
// message from the UI becomes a turn; the SDK keeps conversation
// context across turns via session-resume.
//
// Auth: uses the local Claude Code login the SDK inherits from your
// `claude` CLI session — subscription billing, not per-token API. Make
// sure `claude` is logged in before starting the sidecar.

import { query, type Options } from "@anthropic-ai/claude-agent-sdk";
import { execFileSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { WebSocketServer } from "ws";

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
const FFT_TO_PNG = resolve(FERRITE_HOME, "tools/fft_to_png.py");
const FFT_PEAKS = resolve(FERRITE_HOME, "tools/fft_peaks.py");
const FT8_WORLDMAP = resolve(FERRITE_HOME, "tools/ft8_worldmap.py");

// Capture `ferrite-ctl --help` once at startup so the AI sees the full
// CLI surface in its system prompt — saves a tool call per turn and
// avoids the AI having to guess at flag names. If the binary is
// missing (clean checkout, not built yet) we fall back to a hint
// pointing at the build command rather than crashing the sidecar.
function captureCtlHelp(): string {
  try {
    return execFileSync(FERRITE_CTL, ["--help"], {
      encoding: "utf-8",
      timeout: 5000,
    }).trim();
  } catch (e) {
    console.warn(`[ferrite-ai] \`${FERRITE_CTL} --help\` failed:`, e);
    return `(ferrite-ctl unavailable at ${FERRITE_CTL} — run \`cargo build --release -p ferrite-ctl\` from ${FERRITE_HOME})`;
  }
}
const CTL_HELP = captureCtlHelp();

console.log(`[ferrite-ai] FERRITE_HOME=${FERRITE_HOME}`);
console.log(`[ferrite-ai] FERRITE_CTL=${FERRITE_CTL}`);

// Single state directory for everything the sidecar is the authority
// for: the per-session raw event transcripts (`<session_id>.jsonl`,
// the conversation-state single source of truth) and the human-
// readable per-turn log. Mirrors the FERRITE_SCREENSHOTS_DIR
// convention — run.sh points this at a repo-local, gitignored dir so
// it survives reboots; default falls back to /tmp for a bare
// `npm start`. The session files are co-located with the SDK session
// they key off (both keyed by `session_id`) so the visible transcript
// and the LLM context reset and resume together by construction.
const STATE_DIR = process.env.FERRITE_AI_STATE_DIR ?? "/tmp/ferrite-ai-state";
try {
  mkdirSync(STATE_DIR, { recursive: true });
} catch (e) {
  console.warn(`[ferrite-ai] could not create state dir ${STATE_DIR}:`, e);
}
console.log(`[ferrite-ai] state=${STATE_DIR}`);

// Human-readable record of what the AI said + what tools it ran, for
// grepping when the user reports "the AI did something weird." Now
// lives under the state dir by default (one gitignored place) instead
// of a separate /tmp file; still overridable. Tail with:
//   tail -F "$FERRITE_AI_STATE_DIR/transcript.log"
const TRANSCRIPT_PATH =
  process.env.FERRITE_AI_TRANSCRIPT ?? join(STATE_DIR, "transcript.log");
console.log(`[ferrite-ai] transcript=${TRANSCRIPT_PATH}`);

/** Per-session raw event transcript path. The browser-bound event
 *  stream for one SDK `session_id`, one JSON value per line. This is
 *  the conversation-state authority: ferrited proxies /ws/chat
 *  transparently and the browser is a pure view, so this file (next to
 *  the SDK session it shares a `session_id` with) is the only place
 *  the full transcript lives off the browser. */
function sessionFile(id: string): string {
  return join(STATE_DIR, `${id}.jsonl`);
}

/** Append a turn's browser-bound events to its session file. Called
 *  once per turn (the turn is the unit) keyed by the SDK session id —
 *  so a new session (mode change, /clear, fresh conversation) rolls a
 *  new file by construction. A logging miss must not crash the turn. */
function recordSession(id: string, events: unknown[]): void {
  if (!id || events.length === 0) return;
  try {
    appendFileSync(
      sessionFile(id),
      events.map((e) => JSON.stringify(e)).join("\n") + "\n",
    );
  } catch (e) {
    console.warn(`[ferrite-ai] session record failed for ${id}:`, e);
  }
}

/** Read back the complete recorded event stream for a session, or
 *  `null` when there is no (readable) transcript on disk for that id —
 *  the caller treats `null` as "unresumable → session_reset". */
function readSession(id: string): unknown[] | null {
  if (!id) return null;
  const f = sessionFile(id);
  if (!existsSync(f)) return null;
  try {
    return readFileSync(f, "utf-8")
      .split("\n")
      .filter((l) => l.trim().length > 0)
      .map((l) => {
        try {
          return JSON.parse(l) as unknown;
        } catch {
          return null;
        }
      })
      .filter((x): x is unknown => x !== null);
  } catch (e) {
    console.warn(`[ferrite-ai] session read failed for ${id}:`, e);
    return null;
  }
}

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

/// Turn-scoped state that walks the SDK event stream and emits a
/// readable transcript. The same events still forward to the WS
/// client unchanged — this is purely an observability tap.
class TurnTranscript {
  private pendingText = "";
  private pendingTool: { id: string; name: string; input: string } | null = null;

  constructor(mode: string, userText: string) {
    logLine(`──── turn  mode=${mode}`);
    logLine(`USER:    ${oneLine(userText)}`);
  }

  ingest(event: Record<string, unknown>): void {
    const t = event["type"];
    if (t === "stream_event") {
      const inner = event["event"] as Record<string, unknown> | undefined;
      if (inner) this.ingestStreamEvent(inner);
      return;
    }
    if (t === "user" || t === "assistant") {
      const msg = event["message"] as Record<string, unknown> | undefined;
      const content = msg?.["content"];
      if (Array.isArray(content)) {
        for (const block of content) {
          this.ingestContentBlock(block as Record<string, unknown>);
        }
      }
    }
  }

  private ingestStreamEvent(inner: Record<string, unknown>): void {
    const it = inner["type"];
    if (it === "content_block_start") {
      const block = inner["content_block"] as Record<string, unknown> | undefined;
      if (block?.["type"] === "tool_use") {
        this.flushText();
        this.pendingTool = {
          id: String(block["id"] ?? ""),
          name: String(block["name"] ?? "?"),
          input: "",
        };
      }
      // text blocks just lazily start collecting on the first delta.
      return;
    }
    if (it === "content_block_delta") {
      const delta = inner["delta"] as Record<string, unknown> | undefined;
      if (!delta) return;
      const dt = delta["type"];
      if (dt === "text_delta") {
        this.pendingText += String(delta["text"] ?? "");
      } else if (dt === "input_json_delta" && this.pendingTool) {
        this.pendingTool.input += String(delta["partial_json"] ?? "");
      }
      return;
    }
    if (it === "content_block_stop") {
      if (this.pendingTool) {
        const input = prettyToolInput(this.pendingTool.input);
        logLine(`TOOL:    ${this.pendingTool.name}  ${input}`);
        this.pendingTool = null;
      } else {
        this.flushText();
      }
    }
  }

  private ingestContentBlock(block: Record<string, unknown>): void {
    if (block["type"] !== "tool_result") return;
    const id = String(block["tool_use_id"] ?? "");
    const text = renderToolResult(block["content"]);
    const trimmed = text.length > 240 ? text.slice(0, 240) + "…" : text;
    const errored = block["is_error"] === true;
    const tag = errored ? "ERR " : "OK  ";
    logLine(`RESULT:  ${tag}${id.slice(0, 8)}…  ${oneLine(trimmed)}`);
  }

  private flushText(): void {
    if (!this.pendingText) return;
    logLine(`AI:      ${oneLine(this.pendingText)}`);
    this.pendingText = "";
  }

  finish(status: "done" | "error", errorMessage?: string): void {
    this.flushText();
    if (this.pendingTool) {
      logLine(`TOOL:    ${this.pendingTool.name}  ${prettyToolInput(this.pendingTool.input)}  [stream cut]`);
      this.pendingTool = null;
    }
    if (status === "error") {
      logLine(`ERROR:   ${errorMessage ?? "?"}`);
    }
    logLine(`──── ${status}`);
  }
}

function oneLine(s: string): string {
  // Collapse newlines + runs of whitespace so each transcript line is
  // grep-friendly; full text still sits in the WS stream / browser.
  return s.replace(/\s+/g, " ").trim();
}

function prettyToolInput(raw: string): string {
  if (!raw) return "{}";
  try {
    return oneLine(JSON.stringify(JSON.parse(raw)));
  } catch {
    return oneLine(raw);
  }
}

function renderToolResult(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part: unknown) => {
        if (typeof part === "string") return part;
        if (typeof part === "object" && part !== null) {
          const text = (part as Record<string, unknown>)["text"];
          if (typeof text === "string") return text;
        }
        return JSON.stringify(part);
      })
      .join("");
  }
  return JSON.stringify(content);
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
  // Substitute environment-derived paths + the captured CLI help into
  // the prompt template. Doing it at load-time keeps the per-turn
  // hot path string-free.
  return readFileSync(path, "utf-8")
    .replaceAll("{{FERRITE_HOME}}", FERRITE_HOME)
    .replaceAll("{{FERRITE_CTL}}", FERRITE_CTL)
    .replaceAll("{{FFT_TO_PNG}}", FFT_TO_PNG)
    .replaceAll("{{FFT_PEAKS}}", FFT_PEAKS)
    .replaceAll("{{FT8_WORLDMAP}}", FT8_WORLDMAP)
    .replaceAll("{{CTL_HELP}}", CTL_HELP);
}

// "chat" mode is tools-free: the AI is Q&A only, no Bash, no Read.
// Other modes get the full toolkit ferrite-ctl exploration needs.
function allowedToolsFor(mode: Mode): string[] {
  if (mode === "chat") return [];
  return ["Bash", "Read", "Glob", "Grep"];
}

type IncomingMessage = {
  /** Control envelope shape. `{type: "stop"}` aborts the current
   *  turn; `{type: "request_snapshot", session_id?}` asks for the
   *  authoritative transcript replay; `{type: "reset_session"}` drops
   *  the SDK session binding and rolls a fresh transcript. Plain text
   *  messages have no `type` and use `text`. */
  type?: "stop" | "request_snapshot" | "reset_session" | string;
  text?: string;
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

/** Glue caller-supplied prompt extensions onto the mode's system
 *  prompt. Order is: base mode prompt → driver-specific operator notes
 *  (capabilities) → live source state (what the SDR is *doing right
 *  now*) → active preset notes → active-pipeline block notes →
 *  operator-supplied setup (this rig's physical reality) → local
 *  clock / TZ. Each section is wrapped in a heading so the AI can
 *  tell them apart from the base prompt. */
function appendPromptExtras(
  systemPrompt: string,
  driverNotes: string | undefined,
  currentSource: CurrentSource | undefined,
  presetNotes: string | undefined,
  blockNotes: BlockNote[] | undefined,
  setup: string | undefined,
  localTimeHuman: string | undefined,
  localTimeIso: string | undefined,
  timeZone: string | undefined,
): string {
  let out = systemPrompt;
  const driver = (driverNotes ?? "").trim();
  if (driver) {
    out = `${out}\n\n## Active SDR — driver-specific operator notes\n\n${driver}\n`;
  }
  out += renderCurrentSource(currentSource);
  const preset = (presetNotes ?? "").trim();
  if (preset) {
    out = `${out}\n\n## Active preset — AI notes\n\n${preset}\n`;
  }
  out += renderBlockNotes(blockNotes);
  const op = (setup ?? "").trim();
  if (op) {
    out = `${out}\n\n## Operator-supplied radio setup\n\n${op}\n`;
  }
  const tz = (timeZone ?? "").trim();
  const human = (localTimeHuman ?? "").trim();
  const iso = (localTimeIso ?? "").trim();
  if (tz || human || iso) {
    out =
      `${out}\n\n## Local clock + region\n\n` +
      (human ? `- Operator's local time: ${human}\n` : "") +
      (iso ? `- ISO (UTC): ${iso}\n` : "") +
      (tz
        ? `- IANA timezone: ${tz} (gives approximate region — e.g. ` +
          `\`America/Los_Angeles\` → US west coast → use for propagation, ` +
          `MUF, broadcast-band makeup, and band-condition reasoning)\n`
        : "");
  }
  return out;
}

const httpServer = createServer((_req, res) => {
  res.writeHead(200, { "content-type": "text/plain; charset=utf-8" });
  res.end(
    `ferrite-ai\n\nWS endpoint:  ws://127.0.0.1:${PORT}/ws/chat\nModes:        ${MODES.join(", ")}\n`,
  );
});

const wss = new WebSocketServer({ server: httpServer, path: "/ws/chat" });

wss.on("connection", (ws) => {
  // Per-connection session id, picked up from the SDK's first
  // system/init event of each turn. `null` until the first turn lands.
  let sessionId: string | null = null;
  // Mode is sticky once the client picks one — they can override per
  // message but the default for the connection is the first mode seen
  // (or DEFAULT_MODE).
  let currentMode: Mode = DEFAULT_MODE;
  // AbortController for the currently-running turn (null when idle).
  // The client can send `{type: "stop"}` to abort the in-flight
  // query; we call `.abort()` on this and the SDK terminates the
  // event stream. The for-await loop sees the rejection and falls
  // through into a `ferrite_ai_stopped` envelope.
  let activeAbort: AbortController | null = null;

  console.log(`[ferrite-ai] client connected (mode default=${currentMode})`);
  ws.send(
    JSON.stringify({
      type: "ferrite_ai_hello",
      modes: MODES,
      mode: currentMode,
    }),
  );

  ws.on("message", async (raw) => {
    const parsed = parseIncoming(raw);
    // Control envelopes don't carry `text`. `{type: "stop"}` aborts
    // the in-flight turn; anything else with no text is an error.
    if (!parsed.text) {
      if (parsed.type === "stop") {
        if (activeAbort) {
          activeAbort.abort();
        } else {
          send(ws, { type: "ferrite_ai_stopped", reason: "idle" });
        }
        return;
      }
      // The sidecar is the conversation-state authority. On connect
      // (or an explicit ask) the browser hands back the session id it
      // *thinks* it's continuing; we replay the complete recorded
      // transcript so the browser's view matches our record — or, if
      // that session has no transcript on disk (sidecar restarted, file
      // gone, never existed), tell it to clear coherently instead of
      // showing its stale localStorage log.
      if (parsed.type === "request_snapshot") {
        const want = parsed.session_id ?? sessionId;
        const events = want ? readSession(want) : null;
        if (want && events) {
          // Adopt as the connection binding so the next turn resumes
          // this same session even without a caller-supplied id.
          sessionId = want;
          send(ws, {
            type: "conversation_snapshot",
            session_id: want,
            events,
          });
        } else {
          sessionId = null;
          send(ws, {
            type: "session_reset",
            reason: want
              ? "no transcript on disk for the prior session"
              : "no prior session",
          });
        }
        return;
      }
      // Unified reset: drop the SDK session binding so the next turn
      // starts fresh (a new session id rolls a new transcript file by
      // construction — log + context reset together). Echo a
      // session_reset so the UI clears coherently with an honest
      // banner instead of a now-orphaned log.
      if (parsed.type === "reset_session") {
        sessionId = null;
        send(ws, {
          type: "session_reset",
          reason: parsed.reason ?? "reset",
        });
        return;
      }
      send(ws, {
        type: "ferrite_ai_error",
        message:
          "expected JSON {text, mode?} or a control {type: stop|request_snapshot|reset_session}",
      });
      return;
    }
    const mode = (parsed.mode && MODES.includes(parsed.mode as Mode)
      ? parsed.mode
      : currentMode) as Mode;
    if (mode !== currentMode) {
      // Mode switch resets the session — different system prompt means
      // the old conversation history would re-prime under wrong context.
      sessionId = null;
      currentMode = mode;
    }

    const transcript = new TurnTranscript(mode, parsed.text);

    // Buffer this turn's browser-bound events so we can persist them
    // once (the turn is the unit) under the SDK session id. The
    // leading synthetic `ferrite_ai_user_turn` lets a snapshot replay
    // reconstruct the user's prompt + a fresh assistant turn through
    // the browser's existing reducer; it is NOT sent live (the browser
    // already shows the prompt optimistically), so the proxied live
    // stream is byte-identical to before.
    const turnRecords: unknown[] = [
      { type: "ferrite_ai_user_turn", text: parsed.text, t: Date.now() },
    ];
    const emit = (payload: unknown): void => {
      send(ws, payload);
      turnRecords.push(payload);
    };

    // Per-turn AbortController. The SDK's `abortController` option
    // is the supported way to cancel mid-query for non-streaming
    // input (the streaming-input `.interrupt()` method requires a
    // bigger refactor — async-iterable prompts).
    const turnAbort = new AbortController();
    activeAbort = turnAbort;

    const opts: Options = {
      abortController: turnAbort,
      systemPrompt: appendPromptExtras(
        SYSTEM_PROMPTS[mode],
        parsed.driver_notes,
        parsed.current_source,
        parsed.preset_notes,
        parsed.block_notes,
        parsed.setup_description,
        parsed.local_time_human,
        parsed.local_time_iso,
        parsed.time_zone,
      ),
      allowedTools: allowedToolsFor(mode),
      // User opted into autonomy; bypassPermissions skips the SDK's
      // confirmation prompts entirely. The activity-log middleware on
      // ferrited still surfaces every API call to the UI panel.
      permissionMode: "bypassPermissions",
      // Stream every token + tool event so the UI can show live
      // progress instead of a long blank pause.
      includePartialMessages: true,
    };
    // Prefer a caller-supplied resume id over the connection-local
    // one. Lets the browser persist `session_id` across reloads and
    // hand it back on the first turn after a reconnect — the SDK
    // picks the conversation up from disk.
    const resumeId = parsed.resume_session_id ?? sessionId;
    if (resumeId) {
      (opts as Options & { resume?: string }).resume = resumeId;
      // Adopt the caller-supplied id as our connection-local one so a
      // subsequent turn without an explicit resume still continues
      // the same conversation.
      sessionId = resumeId;
    }

    try {
      for await (const event of query({ prompt: parsed.text, options: opts })) {
        const e = event as Record<string, unknown>;
        // First event of a turn is `system/init` with the session id;
        // capture it so the next turn can `resume`.
        if (e.type === "system" && e["subtype"] === "init") {
          const sid = e["session_id"];
          if (typeof sid === "string") sessionId = sid;
        }
        // Tap the event stream for the readable transcript before
        // forwarding to the WS client unchanged.
        transcript.ingest(e);
        send(ws, event);
        turnRecords.push(event);
      }
      if (turnAbort.signal.aborted) {
        transcript.finish("error", "stopped by user");
        emit({ type: "ferrite_ai_stopped", reason: "user" });
      } else {
        transcript.finish("done");
        emit({ type: "ferrite_ai_done", mode });
      }
    } catch (err) {
      if (turnAbort.signal.aborted) {
        // The SDK surfaces aborts as thrown errors. Treat as a clean
        // stop, not a fatal error.
        transcript.finish("error", "stopped by user");
        emit({ type: "ferrite_ai_stopped", reason: "user" });
      } else {
        const msg = String(err);
        transcript.finish("error", msg);
        console.error("[ferrite-ai] turn failed:", err);
        emit({
          type: "ferrite_ai_error",
          message: msg,
        });
      }
    } finally {
      if (activeAbort === turnAbort) activeAbort = null;
      // Persist the turn under its session id. A turn that errored
      // before `system/init` has no session to key by (nothing
      // resumable anyway) — skip it.
      recordSession(sessionId ?? "", turnRecords);
    }
  });

  ws.on("close", () => {
    console.log("[ferrite-ai] client disconnected");
    // Cancel any in-flight turn so it doesn't hang forever on a
    // disconnected client. The SDK won't have anywhere to send
    // events anyway.
    if (activeAbort) {
      activeAbort.abort();
      activeAbort = null;
    }
    sessionId = null;
  });
});

function parseIncoming(raw: unknown): IncomingMessage {
  const text = String(raw).trim();
  if (!text) return {};
  // Accept either a JSON envelope or a plain-text body for ergonomics
  // (a tester poking with `wscat -c ... -x "hello"` shouldn't have to
  // remember the JSON shape).
  if (text.startsWith("{")) {
    try {
      const parsed = JSON.parse(text);
      if (typeof parsed === "object" && parsed !== null) {
        return parsed as IncomingMessage;
      }
    } catch {
      /* fall through to plain text */
    }
  }
  return { text };
}

function send(ws: import("ws").WebSocket, payload: unknown): void {
  if (ws.readyState !== ws.OPEN) return;
  ws.send(JSON.stringify(payload));
}

httpServer.listen(PORT, () => {
  console.log(
    `ferrite-ai listening on http://127.0.0.1:${PORT}/ws/chat (modes: ${MODES.join(", ")})`,
  );
});
