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

Native packages built and smoke-tested against `linux/{amd64, arm64,
riscv64}` on Ubuntu 24.04, Debian 12, Debian 13 (riscv64), and
Fedora 40 — see [Packaging](#packaging). [docs/08-roadmap.md](docs/08-roadmap.md)
covers everything still pending for 1.0.

## Target platform

- **OS:** Ubuntu 24.04 LTS (Noble), Debian 12 (Bookworm), or newer. Other
  Linux distros probably work; non-Linux hosts are out of scope.
- **Architecture:** x86_64 and aarch64 are first-class — full local build of
  daemon + web bundle. RISC-V (`riscv64gc-unknown-linux-gnu`) builds the
  daemon natively but the web bundle has to be cross-built on x86/aarch64
  and copied over (see [RISC-V notes](#risc-v-notes) below).
- **Hardware:** RTL-SDR (RTL2832U) and SDRplay (RSPduo, RSPdx) via
  SoapySDR. Anything else SoapySDR supports should also work. SDRplay's
  closed-source API is x86_64 / aarch64 only — no riscv64 release.

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
  librtlsdr-dev libhackrf-dev \
  wasi-libc
```

`wasi-libc` provides the `wasm32-wasi` headers + `libc.a`/`libm.a` that the
liquid-dsp wasm build links against (see `blocks/native/liquid-dsp/build.rs`).
Both Debian and Ubuntu ship it as `wasi-libc`.

`libsoapysdr-dev` from apt works, but the project ships a script that builds
SoapySDR + driver modules into a local prefix (no sudo, no version skew with
distro packages) — see step 4.

Node is installed via NodeSource in step 3 (the `nodejs` package in
Debian 12 / Ubuntu 24.04 default repos is too old).

### 2. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

Stable Rust ≥ 1.89 (set in `Cargo.toml`).

### 3. Node + pnpm

Node ≥ 20.10 is required (pinned by `engines` in `package.json`). Debian 12
and Ubuntu 24.04 default repos ship Node 18; install Node 20 from
NodeSource:

```bash
curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
sudo apt install -y nodejs
```

Then enable pnpm 10.x (pinned by `packageManager` in `package.json`) via
the corepack shim that ships with Node 20:

```bash
sudo corepack enable
corepack prepare pnpm@10.33.0 --activate
```

> **RISC-V:** NodeSource doesn't publish riscv64 builds. Use the official
> unofficial-builds tarball + npm-installed pnpm instead:
>
> ```bash
> mkdir -p ~/.local/opt ~/.local/bin
> curl -fsSL https://unofficial-builds.nodejs.org/download/release/v20.18.1/node-v20.18.1-linux-riscv64.tar.xz \
>   | tar -xJ -C ~/.local/opt
> ln -sfn ~/.local/opt/node-v20.18.1-linux-riscv64 ~/.local/opt/node
> for b in node npm npx; do ln -sfn ~/.local/opt/node/bin/$b ~/.local/bin/$b; done
> export PATH="$HOME/.local/bin:$PATH"
> npm install -g --prefix ~/.local pnpm@10.33.0
> ```
>
> Skip corepack: shipped versions in Node 20 LTS hit a stale signing key on
> `pnpm` activation and the new keys aren't backported.

### 4. SoapySDR (local prefix)

```bash
./scripts/build-soapysdr.sh
source soapysdr/env.sh
```

Clones and builds SoapySDR + SoapySDRPlay3 + SoapyHackRF + SoapyRTLSDR into
`./soapysdr-src/`, installs to `./soapysdr/`. The `env.sh` line sets
`PKG_CONFIG_PATH`, `LD_LIBRARY_PATH`, and `SOAPY_SDR_PLUGIN_PATH` so cargo
finds `libSoapySDR` and `ferrited` finds the driver modules at runtime.
**Source it in every shell that runs cargo or `ferrited`.**

The script detects each driver's userland lib (`libsdrplay_api`, `libhackrf`,
`librtlsdr`) and skips drivers whose dependency is missing rather than
aborting the whole build. After installing a missing dep (e.g. SDRplay API),
re-run the script to pick it up.

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

```bash
# One-time host setup for cross-arch (arm64 / riscv64) rows:
sudo apt install -y qemu-user-static binfmt-support docker-buildx
docker run --privileged --rm tonistiigi/binfmt --install all

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
`pnpm -r lint`, and `pnpm -r check` on staged files. The `prepare` hook
tolerates `lefthook install` failure on architectures with no upstream
prebuilt (riscv64, etc.) — the rest of the install still completes; you
just don't get the git hooks.

## RISC-V notes

Tested on Ubuntu 24.04 / `riscv64gc-unknown-linux-gnu` (Ky X1 SBC).

**The daemon (`ferrited`) builds and runs natively.** Apply the alternate
Node 20 install in step 3 above, then steps 4–7 work as documented.
Allow ~16 minutes for `cargo build --release` on an 8-core RISC-V SBC.

**The web bundle does not currently build natively on riscv64.** Two of
its native dependencies — `lightningcss` (used by Tailwind v4 + Vite for
CSS) and `@tailwindcss/oxide`'s native binding — only ship prebuilts for
x86_64 / aarch64. `oxide` falls back to its `wasm32-wasi` build
automatically; `lightningcss` does not.

Recommended workflow: build the static `web/build/` bundle on an
x86_64 / aarch64 host (CI or laptop), copy it to the riscv64 host, and
point `ferrited` at it:

```bash
# on the build host
pnpm --filter @ferrite/web build
rsync -a web/build/ riscv-host:~/ferrite/web/build/

# on the riscv64 host
FERRITE_STATIC_ROOT=web/build ./target/release/ferrited \
  --flowgraph flowgraphs/wbfm.json --start
```

The bundle contains HTML, JS, and `wasm32-unknown-unknown` modules — all
arch-independent at runtime.

A few smaller riscv-specific issues are already handled in the project:
the SoapySDR build script skips drivers whose userland lib is missing
(SDRplay's closed-source API has no riscv64 release); `wasm-pack` would
otherwise fail trying to download a nonexistent `wasm-opt` binary
(disabled via `[package.metadata.wasm-pack.profile.release]` in
`runtime/Cargo.toml` and `blocks/Cargo.toml`); and bindgen's libclang
target is derived from cargo's `HOST` rather than hardcoded to x86_64.

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
