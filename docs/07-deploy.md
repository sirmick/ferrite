# 07 — Deployment

## Target topology

```
  ┌─────────────────────┐          ┌───────────────────────┐
  │   Browser (laptop)  │   LAN    │   SBC near antenna    │
  │                     │◄────────►│   (Pi 5, ROCK 5B, …)  │
  │   static assets +   │  HTTP/WS │   ferrited + Soapy +  │
  │   WS client         │          │   SDR dongle          │
  └─────────────────────┘          └───────────────────────┘
```

Single SBC, single SDR, single `ferrited` process. Browser clients fetch the
SvelteKit static bundle and open a WebSocket to the same host. No separate
frontend server, no reverse proxy required. Trust boundary = the LAN (see
`09-decisions.md`).

## Supported deployment targets

Tested / supported in v0.1:

- **Raspberry Pi 5 (4 GB or 8 GB)** — comfortable, recommended.
- **Raspberry Pi 4 (4 GB)** — works for single-device, single-VFO.
- **Any x86-64 Ubuntu 24.04 machine** — dev desktops, home servers.
- **ROCK 5B, Orange Pi 5** — probably fine (arm64 Ubuntu 24.04), untested.

Out of scope: Windows, macOS, non-Ubuntu Linux distros (may work, no tests).

## System requirements

- Ubuntu 24.04 LTS (Noble) or newer.
- 2 GB RAM minimum, 4 GB recommended.
- 1 GB free disk for the install + a reasonable margin for OPFS recordings
  (if users opt into them — OPFS lives in the browser, not here).
- USB 2.0 port for the SDR.
- Wired Ethernet or stable Wi-Fi for backhaul. See the design note in
  `01-architecture.md` on keeping RF cable runs short and moving bits
  instead of analog.

## Install (manual, one host)

```bash
# 1. Install system prerequisites
sudo apt update
sudo apt install -y \
  libsoapysdr0.8 soapysdr-tools \
  soapysdr-module-rtlsdr \
  librtlsdr0

# 2. Create a dedicated user
sudo useradd --system --home /var/lib/ferrite --create-home \
             --shell /usr/sbin/nologin ferrite
sudo usermod -aG plugdev ferrite         # USB device access

# 3. Drop the release binary and assets
sudo install -m 0755 ferrited /usr/local/bin/ferrited
sudo install -d /usr/local/share/ferrite
sudo cp -r web-dist/* /usr/local/share/ferrite/
sudo cp -r flowgraphs /usr/local/share/ferrite/
sudo chown -R root:root /usr/local/share/ferrite

# 4. Configuration
sudo install -d -o ferrite -g ferrite /etc/ferrite
sudo install -m 0640 -o ferrite -g ferrite \
     ferrited.toml.example /etc/ferrite/ferrited.toml
sudo -e /etc/ferrite/ferrited.toml      # edit to taste

# 5. udev rules (see 06-build.md) — same rules as dev
sudo cp packaging/udev/20-rtlsdr.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger

# 6. systemd unit
sudo cp packaging/systemd/ferrited.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ferrited
```

Visit `http://<sbc>:8088/` from any browser on the LAN.

## Configuration (`/etc/ferrite/ferrited.toml`)

```toml
# Network
bind_addr   = "0.0.0.0:8088"
static_root = "/usr/local/share/ferrite"
flowgraphs  = "/usr/local/share/ferrite/flowgraphs"

# Device (optional — otherwise chosen in the UI)
# auto_open = "driver=rtlsdr,serial=0000001"

# FFT defaults applied at session open unless the client overrides
[fft]
size    = 8192
rate_hz = 30
window  = "hann"

# Identify (Phase F) — optional
# [identify]
# provider = "anthropic"
# api_key  = "sk-ant-…"
# model    = "claude-opus-4-7"

# Debug / observability
debug_stats = false
log_level   = "info"   # trace, debug, info, warn, error
```

Nothing here points at user data (bookmarks, recordings) because there is
none server-side. Those live in the browser.

## systemd unit

```ini
# /etc/systemd/system/ferrited.service
[Unit]
Description=Ferrite SDR daemon
After=network-online.target sdrplay.service
Wants=network-online.target

[Service]
Type=simple
User=ferrite
Group=ferrite
ExecStart=/usr/local/bin/ferrited --config /etc/ferrite/ferrited.toml
Restart=on-failure
RestartSec=2
# Hardening
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/ferrite
DeviceAllow=/dev/bus/usb rw
SupplementaryGroups=plugdev

[Install]
WantedBy=multi-user.target
```

`After=sdrplay.service` is harmless on hosts without SDRPlay (the dependency
is not `Requires=`), and avoids a race on hosts that do.

## Static assets

`ferrited` serves the prebuilt SvelteKit bundle from `static_root`. The
SvelteKit build (adapter-static) produces a static tree:

```
web-dist/
├── index.html
├── _app/…
├── favicon.svg
└── …
```

Every response from this layer carries:

```
Cross-Origin-Opener-Policy:   same-origin
Cross-Origin-Embedder-Policy: require-corp
```

These are mandatory — without them, the browser does not expose
`SharedArrayBuffer` and the audio path does not work. See `06-build.md`.

## Headless flowgraph runs

`ferrited --flowgraph <path.json>` loads a preset and runs its node
half through the same Rust runtime the GUI uses. Long-running headless
decoders (ADS-B → MQTT, APRS → syslog, FT8 → SQLite) run by launching
a second `ferrited` instance on a different port with a preset whose
sinks are wired for the deployment. No separate sidecar binary.

## Upgrading

```bash
sudo systemctl stop ferrited
sudo install -m 0755 ferrited-new /usr/local/bin/ferrited
sudo rsync -a --delete web-dist/ /usr/local/share/ferrite/
sudo systemctl start ferrited
```

Browsers reconnect automatically on WS close. Bookmarks and prefs live in
localStorage so they survive server upgrades trivially.

## Reverse proxy / HTTPS

Not required, not recommended for v0.1. If the host is exposed beyond the
LAN (it shouldn't be — see `09-decisions.md`), the user operates their own
tunnel (Tailscale, WireGuard) or a reverse proxy with its own auth. A proxy
in front of `ferrited` must:

- Pass through WebSocket upgrades.
- **Preserve COOP/COEP headers** end-to-end (or set them itself; they must
  reach the browser).
- Not buffer WS frames.

Nginx, Caddy, and Traefik all handle this with a trivial config.

## Logs and observability

- `ferrited` logs to stdout; systemd captures to the journal.
  - `journalctl -u ferrited -f`.
- `--debug-stats` enables `/api/debug/stats` (see `05-testing.md`).
- No metrics exporter in v0.1. A Prometheus endpoint is a
  straightforward addition later.

## Uninstall

```bash
sudo systemctl disable --now ferrited
sudo rm /etc/systemd/system/ferrited.service
sudo rm /usr/local/bin/ferrited
sudo rm -rf /usr/local/share/ferrite
sudo rm -rf /etc/ferrite
sudo userdel -r ferrite
sudo rm /etc/udev/rules.d/20-rtlsdr.rules
```

Browser data (bookmarks, OPFS recordings) remains in each user's browser
profile; clearing site data removes it.
