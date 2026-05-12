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

Server → browser: forwards every Agent SDK event verbatim (text
deltas, tool uses, tool results) plus a few `ferrite_ai_*`-typed
envelope events:
- `ferrite_ai_hello` — sent on connect with the available modes.
- `ferrite_ai_done` — turn complete.
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
  Use this when burning a tool call would be overkill.

## Adding a new mode

Drop `prompts/<name>.md` and add `<name>` to the `MODES` tuple in
`index.ts`. The UI's mode dropdown reads the list from the
`ferrite_ai_hello` event so it picks up new modes automatically.
