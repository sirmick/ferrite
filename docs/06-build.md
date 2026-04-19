# 06 — Build and dev setup

## Target OS

**Ubuntu 24.04 LTS (Noble) or newer.** Both for development machines and
deployment targets. Other Linuxes probably work but are not tested. Non-Linux
hosts are explicitly out of scope — SoapySDR on macOS/Windows works, but
we neither test nor support that configuration.

## One-time prerequisites

```bash
# System packages (apt)
sudo apt update
sudo apt install -y \
  build-essential pkg-config curl git cmake \
  libsoapysdr-dev soapysdr-tools \
  soapysdr-module-rtlsdr \
  librtlsdr-dev \
  clang lld

# Rust (rustup — do not use the distro package)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack

# Node + pnpm (corepack ships with Node 20+)
sudo apt install -y nodejs npm
sudo corepack enable
corepack prepare pnpm@latest --activate
```

### SDRPlay (if using an RSPduo or other SDRPlay device)

SDRPlay ships a closed-source API daemon that the SoapySDRPlay module depends
on. Install it from SDRPlay's downloads page, then enable the service:

```bash
sudo systemctl enable --now sdrplay
```

And install the Soapy module:

```bash
# Ubuntu does not package SoapySDRPlay3; build from source
git clone https://github.com/pothosware/SoapySDRPlay3
cd SoapySDRPlay3 && mkdir build && cd build
cmake .. && make -j && sudo make install
sudo ldconfig
```

Verify with `SoapySDRUtil --probe="driver=sdrplay"`.

### RTL-SDR udev rules

So non-root users can open the device:

```bash
sudo tee /etc/udev/rules.d/20-rtlsdr.rules <<'EOF'
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE:="0666"
EOF
sudo udevadm control --reload && sudo udevadm trigger
```

Unplug/replug the dongle after installing.

Also blacklist the kernel DVB driver that grabs the device by default:

```bash
sudo tee /etc/modprobe.d/blacklist-rtl.conf <<'EOF'
blacklist dvb_usb_rtl28xxu
EOF
```

Reboot (or `sudo rmmod dvb_usb_rtl28xxu`) before first use.

## Clone and bootstrap

```bash
git clone https://github.com/<user>/ferrite.git
cd ferrite
pnpm install            # pulls every web/ and packages/* dep
cargo fetch             # prime the Rust cache
```

## Workspace layout

```
ferrite/
├── Cargo.toml                    # cargo workspace
├── pnpm-workspace.yaml           # pnpm workspace
├── server/                       # ferrited — Rust axum/tokio daemon
├── blocks/                       # DSP blocks (Rust + ported C) — dual-compile
├── decoders/                     # vendored C decoder cores (dump1090, ft8_lib, …)
├── packages/
│   ├── flowgraph-runtime/        # env-agnostic TS runtime
│   └── flowgraph-blocks/         # WASM-block wrappers
├── web/                          # SvelteKit app (adapter-static)
├── flowgraphs/                   # shipped JSON flowgraph presets
├── fixtures/                     # test IQ recordings + sidecars
├── tools/                        # scrape-sigidwiki, etc.
└── docs/                         # these files
```

## Common commands

### Daily dev loop

```bash
# Terminal 1: Rust backend in watch mode
cd server
cargo watch -x 'run -- --source file://../fixtures/wbfm_1khz_pilot.iq --loop'

# Terminal 2: Svelte dev server (proxies /api and /ws to the above)
cd web
pnpm dev
```

`pnpm dev` listens on `http://localhost:5173`. The page auto-reloads on
frontend changes. Rust changes trigger a rebuild + restart via `cargo watch`;
the browser reconnects the WS automatically.

### Build everything

```bash
# Rust (workspace — server + native blocks)
cargo build --release

# WASM blocks
wasm-pack build --release --target web blocks --out-dir ../web/src/lib/wasm/blocks

# Web bundle
cd web && pnpm build
```

`pnpm build` runs the WASM build as a pre-step via a package.json script,
so in practice `cd web && pnpm build` is the only command needed for a
release frontend.

### Tests

```bash
cargo test --workspace                 # native Rust
wasm-pack test --headless --chrome blocks  # WASM parity
pnpm -r test                           # JS/TS unit (all packages)
pnpm --filter web test:e2e             # Playwright (requires ferrited binary)
```

A single `make test` target in the repo root runs all of the above in
sequence; CI uses the same target.

### Linting / formatting

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
pnpm -r lint
pnpm -r check        # svelte-check strict
```

Pre-commit hook (via lefthook) runs `cargo fmt --check`, `prettier --check`,
and `svelte-check` on staged files. Installed by `pnpm run prepare`.

## Cross-origin isolation (COOP/COEP)

`SharedArrayBuffer` is required for the AudioWorklet ring buffer, and browsers
only expose SAB in a **cross-origin isolated** context. That means every
response must carry:

```
Cross-Origin-Opener-Policy:   same-origin
Cross-Origin-Embedder-Policy: require-corp
```

### Dev

Vite needs a small plugin to set these on the dev server:

```ts
// web/vite.config.ts
import { sveltekit } from '@sveltejs/kit/vite';
import type { Plugin } from 'vite';

const coopCoep = (): Plugin => ({
  name: 'coop-coep',
  configureServer(s) {
    s.middlewares.use((_, res, next) => {
      res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
      res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
      next();
    });
  },
});

export default { plugins: [coopCoep(), sveltekit()] };
```

### Prod

`ferrited` sets the same headers on every response from its static-asset
layer. See `07-deploy.md`.

## WASM toolchain for C decoders

C decoder cores are built with **`clang --target=wasm32-unknown-unknown`**,
not Emscripten. Emscripten drags in a JS shell, an `fs` polyfill, and a ton
of glue we do not want.

A typical build command for a C decoder:

```bash
clang --target=wasm32-unknown-unknown \
      -O3 -flto \
      -fno-builtin -nostdlib \
      -Wl,--no-entry -Wl,--export-dynamic \
      -I decoders/dump1090/include \
      decoders/dump1090/src/*.c \
      -o target/wasm32/dump1090_core.wasm
```

When a decoder needs libc pieces (`memcpy`, `memset`, `malloc`), we link
against **wasi-libc** (static, zero JS glue) — the build script in
`decoders/<name>/build.rs` handles this.

Native builds of the same C sources go through the `cc` crate from the
`blocks/build.rs`. Identical source, identical results (modulo FP ordering
covered by parity tests — see `05-testing.md`).

## Which Node, which Rust

- **Rust:** current stable. CI tracks stable + MSRV (Minimum Supported Rust
  Version) pinned one release back. MSRV bumps are a flagged PR.
- **Node:** 20.x LTS or newer. Used only for the SvelteKit build and
  tooling — there is no Node runtime component shipped in v0.1.
- **pnpm:** 9.x. `packageManager` field in root `package.json` pins exactly.

## Troubleshooting

**"Failed to open device: Resource busy"**
Another process has the device. Check for `rtl_test`, old `ferrited`, or
the kernel DVB driver (`lsmod | grep dvb`).

**"Cannot use SharedArrayBuffer"**
COOP/COEP headers missing. In dev, confirm the Vite plugin is loaded. In
prod, curl the response and verify the two headers.

**`wasm-pack` build fails with `linker 'rust-lld' not found`**
Install the `lld` package or ensure `rustup component add llvm-tools-preview`.

**Playwright cannot find a browser**
`pnpm --filter web exec playwright install chromium`.

**SDRPlay device not enumerated**
Confirm the service is running (`systemctl status sdrplay`) and the
SoapySDRPlay3 module is installed (`SoapySDRUtil --info | grep -i sdrplay`).
