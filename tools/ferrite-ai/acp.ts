// ACP host — runs Claude Code through the `claude-agent-acp` adapter
// (Agent Client Protocol, JSON-RPC over stdio) the way wash does, so the
// sidecar gets the full session stream: thinking (`agent_thought_chunk`),
// reply text, tool calls with live status, plans — one long-lived
// process, many sessions, a `session/prompt` per turn.
//
// The adapter wraps the same @anthropic-ai/claude-agent-sdk `query()`
// the sidecar used before; `_meta.claudeCode.options` passes our SDK
// options through (isolation, model, tools, thinking).

import { spawn, type ChildProcess } from "node:child_process";
import { createRequire } from "node:module";
import { Readable, Writable } from "node:stream";

import * as acp from "@agentclientprotocol/sdk";

const require = createRequire(import.meta.url);

export type UpdateListener = (update: acp.SessionNotification["update"]) => void;

export interface SessionSpec {
  cwd: string;
  systemPrompt: string;
  /** SDK `Options` passed through `_meta.claudeCode.options`. */
  options: Record<string, unknown>;
  mcpServers: acp.McpServer[];
}

export class AcpHost {
  private proc: ChildProcess | null = null;
  private conn: acp.ClientSideConnection | null = null;
  private starting: Promise<acp.ClientSideConnection> | null = null;
  private listeners = new Map<string, UpdateListener>();

  /** The live connection, spawning the adapter on first use (and again
   *  after it exits — sessions don't survive that; callers re-create). */
  async connection(): Promise<acp.ClientSideConnection> {
    if (this.conn) return this.conn;
    if (!this.starting) this.starting = this.start().finally(() => (this.starting = null));
    return this.starting;
  }

  private async start(): Promise<acp.ClientSideConnection> {
    const entry = require.resolve("@agentclientprotocol/claude-agent-acp/dist/index.js");
    const proc = spawn(process.execPath, [entry], {
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
    });
    proc.stderr?.on("data", (b: Buffer) => {
      for (const line of b.toString().split("\n")) {
        if (line.trim()) console.log(`[acp] ${line}`);
      }
    });
    proc.on("exit", (code, sig) => {
      console.warn(`[ferrite-ai] claude-agent-acp exited (code=${code} sig=${sig}); sessions dropped`);
      this.proc = null;
      this.conn = null;
      for (const l of this.listeners.values()) l({ sessionUpdate: "session_info_update", _meta: { ferriteAdapterExited: true } } as never);
      this.listeners.clear();
    });
    this.proc = proc;
    const stream = acp.ndJsonStream(
      Writable.toWeb(proc.stdin!) as WritableStream<Uint8Array>,
      Readable.toWeb(proc.stdout!) as unknown as ReadableStream<Uint8Array>,
    );
    const listeners = this.listeners;
    const conn = new acp.ClientSideConnection(
      () => ({
        async sessionUpdate(n: acp.SessionNotification) {
          listeners.get(n.sessionId)?.(n.update);
        },
        // The radio agent is autonomous (sessions run in
        // bypassPermissions); anything that still asks is allowed.
        async requestPermission(r: acp.RequestPermissionRequest) {
          const pick =
            r.options.find((o) => o.kind === "allow_always") ??
            r.options.find((o) => o.kind === "allow_once") ??
            r.options[0];
          return { outcome: { outcome: "selected", optionId: pick.optionId } };
        },
      }),
      stream,
    );
    await conn.initialize({
      protocolVersion: acp.PROTOCOL_VERSION,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
    });
    this.conn = conn;
    console.log("[ferrite-ai] claude-agent-acp ready");
    return conn;
  }

  /** Create a session; `onUpdate` receives its stream. */
  async newSession(spec: SessionSpec, onUpdate: UpdateListener): Promise<string> {
    const conn = await this.connection();
    const res = await conn.newSession({
      cwd: spec.cwd,
      mcpServers: spec.mcpServers,
      _meta: {
        systemPrompt: spec.systemPrompt,
        claudeCode: { options: spec.options },
      },
    });
    this.listeners.set(res.sessionId, onUpdate);
    await this.bypassPermissions(res.sessionId);
    return res.sessionId;
  }

  /** Route a session's stream (one created or resumed by the caller). */
  listen(sessionId: string, onUpdate: UpdateListener): void {
    this.listeners.set(sessionId, onUpdate);
  }

  /** The radio agent is autonomous; anything that still asks is
   *  auto-allowed by `requestPermission` above. */
  async bypassPermissions(sessionId: string): Promise<void> {
    const conn = await this.connection();
    try {
      await conn.setSessionMode({ sessionId, modeId: "bypassPermissions" });
    } catch (e) {
      console.warn(`[ferrite-ai] bypassPermissions refused (${String(e)}); permission requests will be auto-allowed`);
    }
  }

  /** Run one turn; resolves with the stop reason when it ends. */
  async prompt(sessionId: string, blocks: acp.ContentBlock[]): Promise<acp.StopReason> {
    const conn = await this.connection();
    const res = await conn.prompt({ sessionId, prompt: blocks });
    return res.stopReason;
  }

  async cancel(sessionId: string): Promise<void> {
    const conn = await this.connection();
    await conn.cancel({ sessionId });
  }

  forget(sessionId: string): void {
    this.listeners.delete(sessionId);
  }

  stop(): void {
    this.proc?.kill();
  }
}
