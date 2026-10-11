// AI session bus — publish the sidecar's conversations onto ferrited's
// event plane so every AI panel (any tab, any time later: the store
// persists) and any agent (`await_events(kind="ai")`) sees them, side by
// side with headless drivers (Claude Code hooks → `ferrite-ctl
// chat-post`).
//
// Records go to `ferrite/ingest/ai` (the store's `ai` kind — an append
// log); what's forming right now goes to `ferrite/ingest/ai_live`
// (keyed by session, ≤4 Hz). The vocabulary, shared with chat-post:
//
//   {type:"user",        session, source, mode, model, t, text}
//   {type:"thought",     …, text}                       a finished thinking block
//   {type:"text",        …, text}                       a finished reply block
//   {type:"tool_use",    …, tool_id, name, input, kind?}
//   {type:"tool_result", …, tool_id, text, is_error, images?}
//   {type:"plan",        …, entries:[{content, status, priority}]}
//   {type:"turn_end",    …, status: done|error|stopped, error?}
//   {type:"reset",       …, reason}
//
// Fed by ACP `session/update` notifications (acp.ts). Publishing is
// best-effort: with the broker down records are dropped, not queued.

import mqtt, { type MqttClient } from "mqtt";
import type * as acp from "@agentclientprotocol/sdk";

const SOURCE = "ferrite-ai";
/** Tool input/result text cap per record. */
const TEXT_CAP = 4000;
/** Inline tool-result images: at most this many, each at most this big. */
const IMAGE_MAX = 2;
const IMAGE_BYTES = 400_000;
/** Throttle for `ai_live` partial updates. */
const LIVE_MS = 250;

let client: MqttClient | null = null;
let connecting = false;

/** Connect once, lazily: find the broker port via ferrited's hello. */
export async function connectBus(ferritedUrl: string): Promise<void> {
  if (client || connecting) return;
  connecting = true;
  try {
    for (;;) {
      try {
        const hello = (await (await fetch(`${ferritedUrl}/api/hello`)).json()) as {
          mqtt?: { tcp?: string };
        };
        const port = Number(hello.mqtt?.tcp?.split(":").pop());
        if (!Number.isFinite(port) || port <= 0) throw new Error("daemon has no event plane");
        const host = new URL(ferritedUrl).hostname;
        client = mqtt.connect(`mqtt://${host}:${port}`, {
          clientId: `ferrite-ai-${process.pid}`,
          protocolVersion: 4,
          reconnectPeriod: 2000,
        });
        client.on("connect", () =>
          console.log(`[ferrite-ai] session bus connected (mqtt ${host}:${port})`),
        );
        client.on("error", (e) => console.warn(`[ferrite-ai] session bus: ${e.message}`));
        return;
      } catch (e) {
        console.warn(
          `[ferrite-ai] session bus: ferrited not reachable yet (${String(e)}); retrying in 5 s`,
        );
        await new Promise((r) => setTimeout(r, 5000));
      }
    }
  } finally {
    connecting = false;
  }
}

function publish(kind: "ai" | "ai_live", record: Record<string, unknown>): void {
  if (!client?.connected) return;
  client.publish(`ferrite/ingest/${kind}`, JSON.stringify(record), { qos: 0 });
}

function clip(s: string): string {
  return s.length > TEXT_CAP ? `${s.slice(0, TEXT_CAP)}… [${s.length - TEXT_CAP} more chars]` : s;
}

/** Text + inline images out of ACP tool-call content. */
function splitToolContent(content: acp.ToolCallContent[] | undefined | null): {
  text: string;
  images: string[];
} {
  const texts: string[] = [];
  const images: string[] = [];
  for (const c of content ?? []) {
    if (c.type !== "content") continue;
    const b = c.content as { type: string; text?: string; data?: string; mimeType?: string };
    if (b.type === "text" && typeof b.text === "string") texts.push(b.text);
    else if (b.type === "image" && b.data && images.length < IMAGE_MAX && b.data.length <= IMAGE_BYTES) {
      images.push(`data:${b.mimeType ?? "image/png"};base64,${b.data}`);
    }
  }
  return { text: texts.join(""), images };
}

function hasInput(v: unknown): boolean {
  if (v === undefined || v === null) return false;
  return typeof v !== "object" || Object.keys(v as object).length > 0;
}

/** One session's publisher: turns ACP updates into bus records. The
 *  open text / thought run is held until the stream switches kind (or
 *  the turn ends), then published as one record; the run in progress
 *  rides `ai_live`. */
export class SessionPublisher {
  private run: { kind: "text" | "thought"; buf: string } | null = null;
  /** Tool calls announced before their input streamed in: the
   *  `tool_use` record waits for the update that carries `rawInput`. */
  private pendingTools = new Map<string, Record<string, unknown>>();
  private liveAt = 0;

  constructor(
    readonly session: string,
    private mode: string,
    private model: string,
  ) {}

  private emit(rec: Record<string, unknown>): void {
    publish("ai", {
      session: this.session,
      source: SOURCE,
      mode: this.mode,
      model: this.model,
      t: Date.now(),
      ...rec,
    });
  }

  private flush(): void {
    if (this.run && this.run.buf.trim()) this.emit({ type: this.run.kind, text: this.run.buf });
    this.run = null;
    this.live("", "text");
  }

  private live(text: string, kind: "text" | "thought"): void {
    const now = Date.now();
    if (text && now - this.liveAt < LIVE_MS) return;
    this.liveAt = now;
    publish("ai_live", { session: this.session, source: SOURCE, kind, text, t: now });
  }

  user(text: string): void {
    this.emit({ type: "user", text });
  }

  /** Feed one ACP `session/update`. */
  update(u: acp.SessionNotification["update"]): void {
    switch (u.sessionUpdate) {
      case "agent_message_chunk":
      case "agent_thought_chunk": {
        const kind = u.sessionUpdate === "agent_thought_chunk" ? "thought" : "text";
        const c = u.content as { type: string; text?: string };
        if (c.type !== "text" || !c.text) return;
        if (this.run && this.run.kind !== kind) this.flush();
        if (!this.run) this.run = { kind, buf: "" };
        this.run.buf += c.text;
        this.live(this.run.buf, kind);
        return;
      }
      case "tool_call": {
        this.flush();
        const name =
          ((u._meta as { claudeCode?: { toolName?: string } } | undefined)?.claudeCode?.toolName) ??
          u.title ??
          "tool";
        const rec = { type: "tool_use", tool_id: u.toolCallId, name, title: u.title, kind: u.kind };
        // The adapter announces a call as its block starts, before the
        // input has streamed; the input follows on a `tool_call_update`.
        if (hasInput(u.rawInput)) this.emit({ ...rec, input: clip(JSON.stringify(u.rawInput)) });
        else this.pendingTools.set(u.toolCallId, rec);
        return;
      }
      case "tool_call_update": {
        const pending = this.pendingTools.get(u.toolCallId);
        if (pending && (hasInput(u.rawInput) || u.status === "completed" || u.status === "failed")) {
          this.pendingTools.delete(u.toolCallId);
          this.emit({
            ...pending,
            ...(u.title ? { title: u.title } : {}),
            input: clip(hasInput(u.rawInput) ? JSON.stringify(u.rawInput) : ""),
          });
        }
        if (u.status !== "completed" && u.status !== "failed") return;
        const { text, images } = splitToolContent(u.content);
        const raw = text || (u.rawOutput === undefined ? "" : typeof u.rawOutput === "string" ? u.rawOutput : JSON.stringify(u.rawOutput));
        this.emit({
          type: "tool_result",
          tool_id: u.toolCallId,
          text: clip(raw),
          is_error: u.status === "failed",
          ...(images.length ? { images } : {}),
        });
        return;
      }
      case "plan": {
        this.flush();
        this.emit({ type: "plan", entries: u.entries });
        return;
      }
      default:
        return;
    }
  }

  finish(status: "done" | "error" | "stopped", error?: string): void {
    this.flush();
    for (const rec of this.pendingTools.values()) this.emit({ ...rec, input: "" });
    this.pendingTools.clear();
    this.emit({ type: "turn_end", status, ...(error ? { error } : {}) });
  }
}

/** A context reset (mode switch, /reset, setup edit) — a banner on every panel. */
export function publishReset(session: string | null, mode: string, reason: string): void {
  publish("ai", { type: "reset", session, source: SOURCE, mode, reason, t: Date.now() });
}
