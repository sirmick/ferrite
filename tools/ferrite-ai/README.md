# ferrite-ai

WebSocket sidecar that exposes Claude Code (via
`@anthropic-ai/claude-agent-sdk`) to the Ferrite UI. Each WS message
from the browser becomes a chat turn; the AI uses the Bash tool to
drive `ferrite-ctl`, captures FFT snapshots via `tools/fft_to_png.py`,
and reads catalog references from `samples/sigidwiki/`.

The browser **does not connect here directly** — ferrited reverse-
proxies `/ws/chat` to this sidecar so the operator only ever talks to
one port. This process listens on localhost only; ferrited does the
public-facing accept.

**Auth**: the SDK uses your local Claude Code login (subscription
billing — Pro / Max plans). Make sure `claude` is logged in before
starting the sidecar — no `ANTHROPIC_API_KEY` needed.

## Run

```sh
cd tools/ferrite-ai
npm install
npm run dev
```

Listens on `ws://127.0.0.1:10002/ws/chat`. Override with
`FERRITE_AI_PORT=10005 npm run dev`; if you change the port, also set
`FERRITE_AI_URL=ws://127.0.0.1:10005/ws/chat` on ferrited so its
proxy points at the right place.

## WS protocol

Browser → server (one of):
```json
{"text": "what's at 100.5 MHz?", "mode": "explorer"}
```
or just plain text:
```
what's at 100.5 MHz?
```
or a control envelope to abort the in-flight turn:
```json
{"type": "stop"}
```

Server → browser: forwards every Agent SDK event verbatim (text
deltas, tool uses, tool results) plus a few `ferrite_ai_*`-typed
envelope events:
- `ferrite_ai_hello` — sent on connect with the available modes.
- `ferrite_ai_done` — turn complete.
- `ferrite_ai_stopped` — turn aborted; `reason` is `"user"` (a
  `{type:"stop"}` arrived mid-turn) or `"idle"` (stop with nothing
  running).
- `ferrite_ai_error` — turn failed; `message` field has the cause.

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

### Isolation

The sidecar runs the Agent SDK in isolation mode: no `~/.claude` or
project settings (`settingSources: []`), only the MCP servers declared
in `index.ts` (`strictMcpConfig`), and the account's claude.ai cloud
connectors disabled (`disableClaudeAiConnectors`). Without this the
bundled CLI behaves like the operator's interactive `claude` and — under
`bypassPermissions` — a "tools-free" chat session inherits things like
Google Drive write/share/trash tools. `tools` (the built-in set) is the
restriction; `allowedTools` only auto-approves.
  Use this when burning a tool call would be overkill.

## Adding a new mode

Drop `prompts/<name>.md` and add `<name>` to the `MODES` tuple in
`index.ts`. The UI's mode dropdown reads the list from the
`ferrite_ai_hello` event so it picks up new modes automatically.
