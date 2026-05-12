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
import { appendFileSync, existsSync, readFileSync } from "node:fs";
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

// Where the per-turn transcript lands. Gives a human-readable record
// of what the AI actually said + what tools it ran, so when the user
// reports "the AI did something weird," we can grep this rather than
// reconstruct from /api/decoder/recent. Tail with:
//   tail -F /tmp/ferrite-ai-transcript.log
const TRANSCRIPT_PATH =
  process.env.FERRITE_AI_TRANSCRIPT ?? "/tmp/ferrite-ai-transcript.log";
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
    .replaceAll("{{CTL_HELP}}", CTL_HELP);
}

// "chat" mode is tools-free: the AI is Q&A only, no Bash, no Read.
// Other modes get the full toolkit ferrite-ctl exploration needs.
function allowedToolsFor(mode: Mode): string[] {
  if (mode === "chat") return [];
  return ["Bash", "Read", "Glob", "Grep"];
}

type IncomingMessage = {
  text?: string;
  mode?: Mode | string;
  /** Caller-supplied resume id — when present, takes precedence over
   *  the per-connection `sessionId` the WS handler tracks. Used by
   *  the SvelteKit UI to continue a conversation across browser
   *  reloads (the connection is brand new but the SDK session is
   *  still on disk under that id). */
  resume_session_id?: string;
};

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
    if (!parsed.text) {
      send(ws, {
        type: "ferrite_ai_error",
        message: "expected JSON {text, mode?}",
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

    const opts: Options = {
      systemPrompt: SYSTEM_PROMPTS[mode],
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
      }
      transcript.finish("done");
      send(ws, { type: "ferrite_ai_done", mode });
    } catch (err) {
      const msg = String(err);
      transcript.finish("error", msg);
      console.error("[ferrite-ai] turn failed:", err);
      send(ws, {
        type: "ferrite_ai_error",
        message: msg,
      });
    }
  });

  ws.on("close", () => {
    console.log("[ferrite-ai] client disconnected");
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
