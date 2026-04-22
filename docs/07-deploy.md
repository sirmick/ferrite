# 07 — Deployment

## Topology

```
   ┌─────────────────────┐         ┌──────────────────────┐
   │  Browser (laptop)   │   LAN   │  SBC near antenna    │
   │                     │◄───────►│  (Pi 5, ROCK 5B, …)  │
   │  static assets +    │ HTTP/WS │  ferrited + Soapy +  │
   │  WS client          │         │  SDR dongle          │
   └─────────────────────┘         └──────────────────────┘
```

Single SBC, single SDR, single `ferrited` process. Browser clients fetch
the SvelteKit static bundle and open WebSockets to the same host. No
separate frontend server, no reverse proxy required. Trust boundary = the
LAN ([09-decisions.md D07](09-decisions.md)).

## What ships

A `ferrited` binary plus a `web/build/` static tree. Both come from the
canonical build path in [`README.md`](../README.md#build-and-run):

```bash
cargo build -p ferrited --release      # → target/release/ferrited
pnpm --filter @ferrite/web build       # → web/build/
```

The static tree must be readable by the user `ferrited` runs as.

## Configuration: CLI flags + `FERRITE_STATIC_ROOT`

There is no config file. `ferrited` takes everything via CLI flags
(see `--help` or `server/src/main.rs:45-154`) and one environment variable:

| variable               | default       | meaning                                          |
|------------------------|---------------|--------------------------------------------------|
| `FERRITE_STATIC_ROOT`  | `./web-dist`  | Directory served at `/`. Point at `web/build/`.  |

Common deployment invocation:

```bash
FERRITE_STATIC_ROOT=/usr/local/share/ferrite/web \
  /usr/local/bin/ferrited \
  --bind 0.0.0.0:8088 \
  --flowgraph /usr/local/share/ferrite/flowgraphs/wbfm.json \
  --presets-dir /usr/local/share/ferrite/flowgraphs \
  --source-type SoapySource \
  --source-args 'driver=rtlsdr' \
  --start
```

`--start` auto-starts the pipeline at boot; without it, the UI flips it on
via `POST /api/pipeline/start`. `--presets-dir` enables `GET /api/presets`
and `POST /api/preset` (preset switching from the UI); without it those
endpoints return empty / error.

For a headless capture run with no HTTP server:

```bash
ferrited --flowgraph flowgraphs/capture_fm.json --run-for-secs 60
```

`--run-for-secs N` implies `--start`, skips the HTTP bind, runs the pipeline
for N seconds (or until Ctrl-C), then stops cleanly. The intended shape for
the recording presets.

## Hardware prerequisites

Same SoapySDR setup as dev — see [`README.md`](../README.md#build-and-run)
steps 4 and onward. On the deploy box:

1. `./scripts/build-soapysdr.sh` (or distro packages) and the corresponding
   `LD_LIBRARY_PATH` / `SOAPY_SDR_PLUGIN_PATH` need to be in scope when
   `ferrited` runs.
2. RTL-SDR: `dvb_usb_rtl28xxu` blacklisted, udev rule for non-root access.
3. SDRplay: vendor `.run` installer, `systemctl enable --now sdrplay`.

## Static assets

`tower_http::ServeDir` serves `FERRITE_STATIC_ROOT` with COOP/COEP set on
every response by `tower_http::set_header` middleware
(`server/src/main.rs:313-314`):

```
Cross-Origin-Opener-Policy:   same-origin
Cross-Origin-Embedder-Policy: require-corp
```

These are mandatory — without them `SharedArrayBuffer` is unavailable in
the browser and the audio path stops working. There is no fallback. Any
reverse proxy in front of `ferrited` must preserve them end-to-end (or set
them itself).

## Reverse proxy (optional)

Not required. If the host is exposed beyond the LAN — see D07 and the
warning attached to it — the user operates a tunnel (Tailscale, WireGuard)
or a reverse proxy with its own auth. A proxy must:

- Pass through WebSocket upgrades on `/ws/preset` and `/ws/logs`.
- Preserve COOP/COEP (or set them itself).
- Not buffer WS frames (audio latency).

## Logs

`ferrited` writes `tracing` to stdout. The browser-visible mirror is the
LogPanel ([`web/src/lib/layout/LogPanel.svelte`](../web/src/lib/layout/LogPanel.svelte))
which subscribes to `/ws/logs`.

Levels follow `RUST_LOG` / `EnvFilter`:

```bash
RUST_LOG=info ferrited ...
RUST_LOG=ferrited=debug,ferrite_runtime=trace ferrited ...
```

Default is `info` if `RUST_LOG` isn't set.

## Browser data

User state lives in the browser, not on the server (D08):

- **localStorage** — preset, tuning, panel state.
- The server is stateless across restarts beyond `--flowgraph` /
  `--presets-dir` / the active `SourceConfig` patched in via the API.

Clear browser site data to reset.

## Upgrading

```bash
sudo systemctl stop ferrited            # if you have a unit
install -m 0755 ferrited /usr/local/bin/ferrited
rsync -a --delete web/build/ /usr/local/share/ferrite/web/
sudo systemctl start ferrited
```

The browser reconnects `/ws/preset` and `/ws/logs` automatically on close.

## Uninstall

Just remove what you installed: the binary, the static tree, the presets
dir, and any user/systemd unit you created. There is no server-side state
beyond the on-disk preset files.
