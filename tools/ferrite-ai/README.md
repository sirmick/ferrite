# ferrite-ai

The in-app AI. Runs Claude Code through the ACP adapter
([`claude-agent-acp`](https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp),
Agent Client Protocol over stdio — see `acp.ts`), the way wash drives
agents. ACP gives the full session stream: summarised thinking, reply
text, tool calls with input and status, and plans. The AI drives the
radio through the `ferrite` MCP server (`ferrite-ctl mcp`).

The conversation is published to ferrited's event plane as the `ai`
store kind (`bus.ts`): `user`, `thought`, `text`, `tool_use`,
`tool_result`, `plan`, `turn_end` and `reset` records, plus the
forming text/thought on `ai_live`. Every AI panel renders from there,
so a turn started over REST or from another tab shows everywhere.

The browser **does not connect here directly**: ferrited proxies
`/ws/chat` and `/api/ai/*` to this sidecar. This process listens on
localhost only.

**Auth**: your local Claude Code login (subscription billing — Pro /
Max plans). Make sure `claude` is logged in before starting the
sidecar — no `ANTHROPIC_API_KEY` needed.

## Run

```sh
cd tools/ferrite-ai
npm install
npm run dev
```

Listens on `127.0.0.1:10002`. Override with `FERRITE_AI_PORT=10005`;
if you change the port, also set `FERRITE_AI_URL=ws://127.0.0.1:10005/ws/chat`
on ferrited so its proxies point at the right place.

## REST

Through ferrited (`/api/ai/…`) or direct:

- `POST /prompt` `{"text", "mode"?, "model"?, "session_id"?, "new_session"?, "wait"?}`
  — run a turn exactly as if typed into the AI panel. Without
  `session_id` it continues the mode's latest session; `new_session`
  starts a fresh one. Returns `202 {session_id, mode, model, status:"started"}`,
  or with `wait` the finished turn `{…, status: done|stopped|error, stop_reason, text}`.
  `409` if that session already has a turn running.
- `POST /cancel` `{"session_id"?}` — stop a turn (all running turns without an id).
- `GET /sessions` — live sessions (`busy`, `latest`).

CLI: `ferrite-ctl ai-prompt "<text>" [--mode M] [--model ID] [--session ID | --new] [--wait]`.

Sessions survive a sidecar restart: Claude Code keeps them on disk, and
a turn naming an unknown id resumes it (`session/resume`, no replay —
the history is already on the bus).

## WS protocol (`/ws/chat`, the panel's control channel)

Browser → sidecar: `{"text", "mode", "model"?, "resume_session_id"?, …per-turn context}`
(current source, preset/block notes, local time, driver notes,
`setup_description`), or a control envelope: `{"type":"stop"}`,
`{"type":"request_snapshot","session_id"}`, `{"type":"reset_session","reason"}`.

Sidecar → browser: `ferrite_ai_hello` (modes, per-mode default
`models`, `model_options`) on connect; `{"type":"system","subtype":"init","session_id"}`
when a turn starts (the panel keeps it to continue the session);
`ferrite_ai_done` / `ferrite_ai_stopped` / `ferrite_ai_error` when it
ends; `conversation_snapshot` / `session_reset` in reply to the
controls. The conversation itself comes from the bus, not this socket.

## Modes

Each mode is a different system prompt + a different tool allow-list,
loaded from `prompts/<mode>.md`. The browser picks per turn; switching
mode resets the conversation (different system prompt would re-prime
prior context badly).

- **explorer** (default) — full toolkit, autonomy-flavoured. Scans
  the band, captures FFT, identifies signals.
- **decoder** — full toolkit, focused on running a decoder and
  reporting events; doesn't spectrum-hop.
- **diagnose** — full toolkit but read-leaning. Inspects state,
  flowgraph JSONs, recent logs; mentions writes before making them.
- **chat** — *no tools*. Pure Q&A about SDR / Ferrite internals.

### Models

Each mode pins a model explicitly (the Agent SDK's fallback is whatever
the bundled CLI defaults to, which silently lagged a generation):

| mode | model |
|---|---|
| explorer / decoder / diagnose | `claude-opus-5-5` |
| chat | `claude-haiku-5-5` |

Override globally with `FERRITE_AI_MODEL=…`, or per mode with
`FERRITE_AI_MODEL_<MODE>=…` (e.g. `FERRITE_AI_MODEL_CHAT=claude-opus-5-5`).
Per turn, the browser may send `{"text": …, "mode": …, "model": "<id>"}`;
`ferrite_ai_hello` advertises the per-mode defaults (`models`) and the ids
the picker may choose (`model_options`). Unknown ids are ignored, not
forwarded.

### Context

A session's system prompt is fixed when it starts: the mode prompt,
the SDR driver notes, and the operator's station notes. Station notes
are the browser's `setup_description`; a turn without one (a headless
driver or REST) gets `$FERRITE_AI_STATE_DIR/setup.md` (override:
`FERRITE_AI_SETUP_FILE`). Each turn then leads with a
`<turn-context>` block: live source state, the clock, and the preset
and block notes whenever they changed.

Thinking is on (`adaptive`, summarised); `FERRITE_AI_EFFORT` sets the
effort level.

### Isolation

Sessions run in isolation mode (passed through ACP's `_meta.claudeCode.options`): no `~/.claude` or
project settings (`settingSources: []`), only the MCP servers declared
in `index.ts` (`strictMcpConfig`), and the account's claude.ai cloud
connectors disabled (`disableClaudeAiConnectors`). Without this the
bundled CLI behaves like the operator's interactive `claude` and — under
`bypassPermissions` — a "tools-free" chat session inherits things like
Google Drive write/share/trash tools. `tools` (the built-in set) is the
restriction; `allowedTools` only auto-approves. Sessions run in
`bypassPermissions`; any permission request is auto-allowed.

## Adding a new mode

Drop `prompts/<name>.md` and add `<name>` to the `MODES` tuple in
`index.ts`. The UI's mode dropdown reads the list from the
`ferrite_ai_hello` event so it picks up new modes automatically.
