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

The sidecar is plain HTTP on localhost. Nothing connects to it
directly: ferrited proxies `/api/ai/*` to it, so the AI panel, the CLI
and any REST client all use the same front door.

**Auth**: your local Claude Code login (subscription billing — Pro /
Max plans). Make sure `claude` is logged in before starting the
sidecar — no `ANTHROPIC_API_KEY` needed.

## Run

```sh
cd tools/ferrite-ai
npm install
FERRITE_CTL_CONNECT=http://127.0.0.1:12001 npm run dev
```

Listens on `127.0.0.1:10002`. Override with `FERRITE_AI_PORT=10005`;
then set `FERRITE_AI_URL=http://127.0.0.1:10005` on ferrited so its
proxy points at the right place. `FERRITE_CTL_CONNECT` is the daemon to
drive (the same variable `ferrite-ctl --connect` reads; run.sh exports
it). `./run.sh` starts all of this for you.

## REST — the one front door

Through ferrited (`/api/ai/…`) or direct:

- `GET /hello` — modes, per-mode default `models`, the `model_options`
  a caller may pick.
- `POST /prompt` `{"text", "mode"?, "model"?, "session_id"?, "new_session"?, "wait"?, "time_zone"?}`
  — run a turn. Without `session_id` it continues the mode's current
  conversation (the same one in every tab and the CLI); `new_session`
  starts a fresh one, which becomes current. Returns
  `202 {session_id, mode, model, status:"started"}`, or with `wait` the
  finished turn `{…, status: done|stopped|error, stop_reason, text}`.
  `409` if that conversation already has a turn running.
- `POST /cancel` `{"session_id"?}` — stop a turn (every running turn
  without an id).
- `POST /reset` `{"mode"?, "reason"?}` — the mode's next turn (every
  mode's, without `mode`) opens a fresh conversation; one `reset` record
  is the banner every panel shows.
- `GET /setup` → `{text}`, `POST /setup` `{text}` — the station notes.
  An edit starts every mode over (the notes are in the system prompt).
- `GET /sessions` — live sessions (`busy`, `latest`) and the current
  conversation per mode.

CLI: `ferrite-ctl ai-prompt "<text>" [--mode M] [--model ID] [--session ID | --new] [--wait]`.

Which conversation each mode continues is read back from the `ai` log
at boot, so a sidecar restart picks it up. Claude Code keeps sessions
on disk, and a turn naming one that isn't live resumes it
(`session/resume`, no replay — the history is already on the bus).

## The transcript

The conversation is published to ferrited's event plane as the `ai`
store kind (`bus.ts`) — `user`, `thought`, `text`, `tool_use`,
`tool_result` (with `analysis_id` when the call drew a graphic),
`plan`, `turn_end`, `reset` — plus the forming text/thought on
`ai_live`. Every AI panel renders from there: thinking (foldable),
replies, each MCP call as a card (`⚙ tune carrier=7.0258 MHz`, its
why, its outcome) with the graphic it drew inside — spectrum,
spectrogram, envelope, sigwiki card, spot map, view snapshot — and a
`signals` table. A coding agent mirrored onto the bus (below) shows
its shell commands as `$ …` cards.

`node test/live-e2e.mjs` checks the whole loop against a running stack
with one cheap real turn; `npm test` covers the pure parts.

### Mirroring a coding session

`.claude/settings.json` wires Claude Code hooks to
`scripts/ai-chat-mirror.sh`, which posts that session's prompts, tool
calls and replies onto the same bus (`ferrite-ctl chat-post`). It is
**off unless** `FERRITE_AI_MIRROR=1` is in Claude Code's environment
(e.g. `"env": {"FERRITE_AI_MIRROR": "1"}` in
`.claude/settings.local.json`), so ordinary coding sessions in this
repo stay out of the radio AI's transcript.

## Modes

Each mode is a different system prompt + a different tool allow-list,
loaded from `prompts/<mode>.md`, and its own conversation: switching
mode in the panel continues that mode's conversation.

- **explorer** (default) — all ferrite tools, autonomy-flavoured. Scans
  the band, captures FFT, identifies signals.
- **decoder** — all ferrite tools, focused on running a decoder and
  reporting events; doesn't spectrum-hop.
- **diagnose** — all ferrite tools but read-leaning. Inspects state,
  flowgraph JSONs, recent logs; mentions writes before making them.
- **chat** — *no tools* (no MCP server). Pure Q&A about SDR / Ferrite internals.

### Models

Each mode pins a model explicitly (the Agent SDK's fallback is whatever
the bundled CLI defaults to, which silently lagged a generation):

| mode | model |
|---|---|
| explorer / decoder / diagnose | `claude-opus-5-5` |
| chat | `claude-haiku-5-5` |

Override globally with `FERRITE_AI_MODEL=…`, or per mode with
`FERRITE_AI_MODEL_<MODE>=…` (e.g. `FERRITE_AI_MODEL_CHAT=claude-opus-5-5`).
A prompt may pick `"model": "<id>"` from `model_options` (`GET /hello`);
unknown ids are ignored, not forwarded.

### Context

Built here from ferrited's own state (`context.ts`), so it's the same
whoever asks. A session's system prompt is fixed when it starts: the
mode prompt, the SDR driver notes (`web/src/lib/controls/sdr-presets/<driver>.json`),
and the station notes (`$FERRITE_AI_STATE_DIR/setup.md`, override
`FERRITE_AI_SETUP_FILE`; edited from the panel). Each turn then leads
with a `<turn-context>` block: live source state, the preset and block
notes whenever they changed, and the clock — in the caller's
`time_zone` (the panel sends the browser's), else `FERRITE_TZ`, else
the host's.

Thinking is on (`adaptive`, summarised); `FERRITE_AI_EFFORT` sets the
effort level.

### No shell

No mode has built-in tools (`tools: []`): no Bash, no file reads. The
AI's whole surface is the `ferrite` MCP server, so everything it does
is visible in the AI tab — measurements as analysis cards
(`spectrum`, `spectrogram`, `envelope`, `spot_map`), pictures inline on
tool cards (`view_snapshot`, `reference_image`), daemon logs via
`logs`. A capability it lacks is a missing MCP tool, not a script to
shell out to.

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
`index.ts`. The UI's mode dropdown reads the list from `GET /hello`, so
it picks up new modes automatically.
