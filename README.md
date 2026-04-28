# Ferrite

A modern web-based SDR application. Spectrum-centric, pleasant, fast.

Runs a thin Rust daemon (`ferrited`) next to the antenna — typically on an ARM
SBC — and a rich browser front end that does the demodulation and decoding in
WASM. The backend streams a wideband FFT for the waterfall plus narrowband IQ
slices for each active VFO; the browser owns everything downstream.

Decoders (ADS-B, APRS, digital voice, FT8, …) are built as shared blocks
(Rust + WASM) wired together by JSON flowgraph files.

## Status

**0.9.0 — pre-release.** Working: `ferrited` server, native + WASM builds of
the runtime and DSP blocks, SvelteKit web client, RTL-SDR + HackRF +
SDRplay (RSPduo / RSPdx) + Airspy R2 / HF+ + BladeRF + PlutoSDR over
SoapySDR. Listening modes (WBFM mono/stereo, NBFM, AM, USB/LSB, CW) plus
data decoders (POCSAG, FLEX, APRS, ADS-B, NOAA EAS, Morse, DTMF, RDS)
all decode end-to-end against live RF.

Native `.deb` / `.rpm` packages for Ubuntu / Debian / Fedora across
amd64, arm64, and riscv64 — see [Packaging](#packaging).
[docs/08-roadmap.md](docs/08-roadmap.md) covers everything still
pending for 1.0.

## Target platform

- **OS:** Ubuntu 24.04 LTS (Noble) for development. Pre-built packages
  also target Debian 12 / 13 and Fedora 40 — see [Packaging](#packaging).
  Non-Linux hosts are out of scope.
- **Hardware:** RTL-SDR (RTL2832U), HackRF, SDRplay (RSPduo / RSPdx),
  Airspy R2 / HF+, BladeRF, PlutoSDR via SoapySDR. Anything else
  SoapySDR supports should also work.

## Build and run

The repo has two workspaces:

| Workspace | Members                                              | Manager |
| --------- | ---------------------------------------------------- | ------- |
| Cargo     | `server/`, `runtime/`, `blocks/`, `blocks-macros/`   | cargo   |
| pnpm      | `web/`, `tools/*`                                    | pnpm    |

`server/` builds the `ferrited` binary. `runtime/` and `blocks/` dual-compile
to native (used by `ferrited`) and to `wasm32-unknown-unknown` (used by the
browser). `web/` is a SvelteKit app whose `pnpm build` script invokes
`wasm-pack` on `runtime/` and `blocks/` before running Vite.

### 1. System packages

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config curl git cmake clang lld \
  wasi-libc \
  librtlsdr-dev libhackrf-dev \
  libairspy-dev libairspyhf-dev libbladerf-dev \
  libiio-dev libad9361-dev
```

The SDR `-dev` packages give you the userland libs each Soapy plugin
links against — RTL-SDR, HackRF, Airspy R2 / HF+, BladeRF, PlutoSDR.
Drop any you don't care about; `scripts/build-soapysdr.sh` (step 4)
detects what's installed and only builds matching plugins.

`wasi-libc` provides the `wasm32-wasi` headers + `libc.a`/`libm.a` that
the liquid-dsp wasm build links against (see
`blocks/native/liquid-dsp/build.rs`).

`libsoapysdr-dev` from apt works, but the project ships a script that
builds SoapySDR + driver modules into a local prefix (no sudo, no
version skew with distro packages) — see step 4.

Node is installed via NodeSource in step 3 (Ubuntu 24.04's default
`nodejs` package is too old).

### 2. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

Stable Rust ≥ 1.89 (set in `Cargo.toml`).

### 3. Node + pnpm

Ubuntu 24.04's default `nodejs` package is 18.x, older than the
`engines.node >= 20.10.0` pin. Install Node 20 from NodeSource and
activate pnpm via corepack (which ships with Node 20):

```bash
curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
sudo apt install -y nodejs
sudo corepack enable
corepack prepare pnpm@10.33.0 --activate
```

### 4. SoapySDR (local prefix)

```bash
./scripts/build-soapysdr.sh
source soapysdr/env.sh
```

Clones and builds SoapySDR + driver plugins (SoapyRTLSDR, SoapyHackRF,
SoapySDRPlay3, SoapyAirspy, SoapyAirspyHF, SoapyBladeRF, SoapyPlutoSDR,
SoapyUHD) into `./soapysdr-src/`, installs to `./soapysdr/`. The
`env.sh` line sets `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH`, and
`SOAPY_SDR_PLUGIN_PATH` so cargo finds `libSoapySDR` and `ferrited`
finds the driver modules at runtime.
**Source it in every shell that runs cargo or `ferrited`.**

The script detects each driver's userland lib via `pkg-config` and
silently skips drivers whose dependency is missing rather than aborting
the whole build. The apt step above pulls everything except SDRplay
(closed-source, see below) and UHD (large dep tree; install
`libuhd-dev` + re-run the script if you need USRPs). Re-run the script
any time you install a new userland lib to pick up the matching plugin.

Sanity check: `SoapySDRUtil --info && SoapySDRUtil --find`.

#### SDRplay

The Soapy module needs the closed-source SDRplay API daemon installed
system-wide first. Grab the `.run` installer from sdrplay.com, run it, then:

```bash
sudo systemctl enable --now sdrplay
```

Re-run `./scripts/build-soapysdr.sh` afterward so SoapySDRPlay3 picks it up.

#### RTL-SDR

Blacklist the kernel DVB driver and add a udev rule for non-root access:

```bash
sudo tee /etc/modprobe.d/blacklist-rtl.conf <<'EOF'
blacklist dvb_usb_rtl28xxu
EOF

sudo tee /etc/udev/rules.d/20-rtlsdr.rules <<'EOF'
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE:="0666"
EOF

sudo udevadm control --reload && sudo udevadm trigger
sudo rmmod dvb_usb_rtl28xxu  # or reboot
```

Unplug/replug the dongle.

### 5. Fetch dependencies

```bash
pnpm install
cargo fetch
```

### 6. Build everything

```bash
# Native Rust (ferrited + native blocks/runtime)
cargo build --release

# Web bundle (chains wasm-pack on runtime/ and blocks/, then vite build)
pnpm --filter @ferrite/web build
```

Outputs:

- `target/release/ferrited` — the daemon
- `web/build/` — static SvelteKit bundle (served by `ferrited` in prod)
- `web/src/lib/wasm/{blocks,runtime}/` — wasm-pack output (gitignored,
  regenerated by the web build)

The WASM steps can also be run individually:

```bash
pnpm --filter @ferrite/web wasm:build         # both
pnpm --filter @ferrite/web wasm:build:blocks
pnpm --filter @ferrite/web wasm:build:runtime
```

### 7. Run

`ferrited` requires `--flowgraph PATH`. Presets live in `flowgraphs/`. To
serve the web UI built in step 6, point `FERRITE_STATIC_ROOT` at
`web/build/` (otherwise only `/api` and `/ws` respond — handy if you're
running `vite dev` separately).

```bash
source soapysdr/env.sh
export FERRITE_STATIC_ROOT=web/build

# RTL-SDR, FM broadcast, auto-start the pipeline:
./target/release/ferrited \
  --flowgraph flowgraphs/wbfm.json \
  --source-type SoapySource \
  --source-args 'driver=rtlsdr' \
  --start

# SDRplay (RSPduo: 'Tuner 1 50 ohm', RSPdx: 'Antenna A' / 'Antenna B'):
./target/release/ferrited \
  --flowgraph flowgraphs/wbfm.json \
  --source-type SoapySource \
  --source-args 'driver=sdrplay' \
  --antenna 'Antenna A' \
  --start

# No hardware — synthetic SineSource (default if --source-type is omitted):
./target/release/ferrited --flowgraph flowgraphs/wbfm.json --start
```

Then open <http://localhost:8088>.

#### Dev loop

```bash
# Terminal 1 — backend
source soapysdr/env.sh
cargo run -p ferrited -- --flowgraph flowgraphs/wbfm.json --source-args 'driver=rtlsdr' --start

# Terminal 2 — frontend with HMR
pnpm --filter @ferrite/web dev
```

Vite serves on <http://localhost:5173> and proxies `/api` and `/ws` to
`http://127.0.0.1:8088` (override with `FERRITED_URL`). The page reloads on
frontend changes; restart `cargo run` for backend changes.

#### Diagnostic flags

```bash
ferrited --list-devices                       # enumerate SoapySDR devices
ferrited --probe-device 'driver=rtlsdr'       # one device's full capability schema
ferrited --probe-all                          # probe every attached device
ferrited --flowgraph ... --run-for-secs 30    # headless capture, no HTTP server
```

## Packaging

`packaging/run_matrix.sh` builds native `.deb` / `.rpm` packages for a matrix
of (distro, arch) combos using Docker. Each row spins up a clean container,
runs the same install steps documented above, and produces a self-contained
package with `ferrited`, the bundled SoapySDR + driver plugins, the static
web bundle, and the flowgraph presets.

One-time host setup (Ubuntu 24.04):

```bash
# Docker engine + buildx plugin. Add yourself to the docker group
# (`sudo usermod -aG docker $USER` + log out/in) so the script doesn't
# need sudo per-row.
sudo apt install -y docker.io docker-buildx

# QEMU + binfmt_misc for cross-arch (arm64 / riscv64) rows. Skip if you
# only build amd64.
sudo apt install -y qemu-user-static binfmt-support
docker run --privileged --rm tonistiigi/binfmt --install all
```

Then:

```bash
bash packaging/run_matrix.sh
```

Output lands in `dist/`:
- `dist/<tag>.log` — full build log per row, streamed live
- `dist/packages/<tag>/*.deb` or `*.rpm` — extracted artifact

The current matrix targets Ubuntu 24.04, Debian 12, Debian 13 (riscv64
only — bookworm has no official riscv64 image), and Fedora 40 across
amd64 + arm64 + riscv64 where the base image manifest supports it. Edit
`TARGETS=( … )` in `run_matrix.sh` to add or drop rows. Per-row failures
don't halt the matrix; the script prints a PASS/FAIL summary at the end.

The web bundle is built once on the host (arch-independent) and copied
into each container's source tarball — sidesteps the lightningcss /
@tailwindcss/oxide native-binding gap on non-x86 archs.

Install the produced package the usual way:

```bash
sudo apt install ./dist/packages/ubuntu-24.04-amd64/ferrite_0.9.0_amd64.deb
sudo dnf install ./dist/packages/fedora-40-amd64/ferrite-0.9.0-1.fc40.x86_64.rpm
```

The `/usr/bin/ferrited` wrapper installed by the package sets
`LD_LIBRARY_PATH`, `SOAPY_SDR_PLUGIN_PATH`, and `FERRITE_STATIC_ROOT`
so the daemon finds its bundled libs and the web bundle without any
manual env setup.

## Tests

```bash
# All native Rust tests (server + runtime + blocks integration)
cargo test --workspace

# Web unit tests (Vitest)
pnpm --filter @ferrite/web test

# Type-check the SvelteKit app
pnpm --filter @ferrite/web check

# Lint everything
cargo clippy --workspace -- -D warnings
pnpm -r lint
```

Notable integration tests:

- `server/tests/smoke.rs`, `server/tests/record_smoke.rs` — boot `ferrited`,
  exercise REST + WS, verify recording presets write valid sidecars.
- `server/tests/dtmf_cross_env.rs`, `runtime/tests/dtmf_e2e.rs` — DTMF tone
  decoded end-to-end across the node↔browser env split.
- `blocks/tests/wbfm_e2e.rs` — WBFM demod parity against a known-good
  fixture.

Pre-commit (lefthook, installed by `pnpm install`) runs `cargo fmt --check`,
`pnpm -r lint`, and `pnpm -r check` on staged files.

## Documentation

Design docs in [docs/](docs/), roughly in order:

- [00 — Context and goals](docs/00-context.md)
- [01 — Architecture](docs/01-architecture.md)
- [02 — Control API and WS frame format](docs/02-protocol.md)
- [03 — Block system](docs/03-blocks.md)
- [04 — Flowgraph JSON schema](docs/04-flowgraphs.md)
- [05 — Testing strategy](docs/05-testing.md)
- [06 — Build and dev setup](docs/06-build.md)
- [07 — Deployment](docs/07-deploy.md)
- [08 — Roadmap](docs/08-roadmap.md)
- [09 — Decision log](docs/09-decisions.md)
- [10 — Commit-level implementation plan](docs/10-commits.md)

## License

See [LICENSE](LICENSE).
