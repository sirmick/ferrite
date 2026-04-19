# 09 — Decision log

Decisions that shaped v0.1, in the order they were made. Each entry is kept
short; the "why" outweighs the "what" because future-us will want to know
whether a decision is still load-bearing.

The format: **context → decision → consequence**. Where a decision was
contentious, the rejected alternatives are listed.

## D01 — Split the DSP: server does FFT + channelization, client does demod + decode

**Context.** The SBC is small; doing full demod + decode for every client
there balloons the backend. But browsers cannot each compute their own
wideband FFT (each tab would recompute identical work from the same IQ).

**Decision.** `ferrited` produces one wideband FFT (the waterfall) and
narrowband IQ per active VFO, and nothing else. All demod and decoding
happens in the browser (WASM) or in the optional Node sidecar.

**Consequence.** Server stays tiny. Decoders are shipped as data
(flowgraphs + WASM blocks), not code-linked into the server. New
decoders do not require a server release.

## D02 — Dual-compile DSP blocks: one Rust crate, two targets (native + WASM)

**Context.** The server needs a native channelizer; the client (and
headless sidecar) runs the same DSP kernels. Maintaining two
implementations or two languages invites drift.

**Decision.** `blocks/` is a single Rust crate with `crate-type =
["rlib", "cdylib"]` and a `wasm` feature. `cargo build` → native;
`wasm-pack build --target web` → WASM. Identical source, identical
tests (plus a parity test comparing the two on fixtures).

**Consequence.** One codebase for DSP. `#[ferrite_block]` macro registers
blocks in both environments. No second runtime surface to test or
debug.

## D03 — Port C decoders with `clang --target=wasm32`, not Emscripten

**Context.** ADS-B (dump1090), FT8 (ft8_lib), and M17 (codec2) ship as C.
We want to reuse the DSP cores.

**Decision.** Vendor the pure-DSP cores only (rip out stdio / socket /
audio glue), and compile them twice: native via the `cc` crate, WASM via
`clang --target=wasm32-unknown-unknown` + wasi-libc for any libc bits
we need. A thin Rust `Block` wrapper implements the block trait on both
targets.

**Rejected.** Emscripten — drags in JS shell, `fs` polyfill, and runtime
glue serving no purpose for us. We are not running a Unix program in
the browser; we are running a DSP kernel.

**Consequence.** No Emscripten runtime in the bundle. C FFI is identical
on both sides.

## D04 — JSON flowgraph configuration, no visual editor

**Context.** GNU Radio Companion's visual graph is expensive to build,
expensive to maintain, and solves a problem we don't have (users are
not DSP authors here; curated decoders are enough).

**Decision.** Flowgraphs are JSON files in `flowgraphs/`. Ship a
curated set. No graphical editor. Runtime validates ports and params
before running.

**Consequence.** Adding a decoder is "drop in a JSON file + any new
blocks"; no UI changes. JSON is regenerable from Rust types so schema
stays authoritative.

## D05 — Flowgraph runtime is a shared TypeScript package, not Rust

> **Superseded by D19.** A TS runtime was shipped through Phase D; once
> channelizer + WBFM end-to-end worked, it became clear the server
> needed to run the *same* graph the browser did, not just agree on its
> JSON shape. Keeping "one runtime" meant picking a language — Rust
> wins because the DSP blocks are already there.

**Context.** Both the browser (WebAssembly) and a Node sidecar need to
run flowgraphs. A Rust runtime would need both native and WASM
instantiation paths and a second set of bindings for Node.

**Decision.** `packages/flowgraph-runtime/` is env-agnostic TS: JSON
parse, validation, block registry, scheduler. Environment-specific
sinks/sources (AudioSink, OpfsFileSink in the browser; FsFileSink,
MqttSink in Node) are separate packages.

**Rejected.** Rust flowgraph runtime on the server + TS in the browser —
drifts. Embedded JS in Rust via QuickJS / V8 / deno_core — exotic, hard
to debug across the FFI, no real benefit.

**Consequence.** The Node sidecar is the same runtime the browser uses,
connected to `ferrited` over loopback WebSocket — same wire protocol
the browser speaks. One transport, one runtime, one set of blocks.

## D06 — Single listener, last-connect wins (v0.1)

**Context.** Multi-listener requires sharing the channelizer, signalling
VFO ownership, and thinking about fairness. None of that is necessary
for "one person, one SDR, at home."

**Decision.** v0.1 supports at most one active session on `ferrited`.
A new `POST /api/device/open` while a session is active closes the
first session (notifies it via a `session_closed` WS event) and takes
over.

**Consequence.** Simplest possible server. No tuner contention, no
auth. The channelizer is pool-architected so lifting this restriction
later is an extension, not a rewrite.

## D07 — LAN trust, no auth (v0.1)

**Context.** The target user runs `ferrited` at home on a trusted LAN.
Adding auth in v0.1 costs UX (login flow, token storage, etc.) for a
threat that doesn't exist on a home network.

**Decision.** No auth on any `ferrited` endpoint in v0.1. Any remote
access is the user's VPN/tunnel problem (Tailscale, WireGuard,
reverse proxy with its own auth).

**Consequence.** Same posture as KiwiSDR, OpenWebRX, WebSDR. Documented
loudly in `07-deploy.md` so nobody accidentally exposes `ferrited`
on a public IP.

## D08 — User data lives in the browser (v0.1)

**Context.** Bookmarks, UI preferences, short IQ recordings — all could
live on the server or the client. Server storage needs a schema,
migrations, backups, maybe auth.

**Decision.** localStorage for bookmarks + prefs. OPFS (Origin Private
File System) for short IQ recordings. Zero server state. `ferrited`
stays a stateless DSP daemon.

**Tradeoff.** Bookmarks are device-specific (no sync). Acceptable for
v0.1. A later opt-in sync endpoint is possible if the browser data
schema is serializable from day one — which it is.

**Consequence.** `ferrited` is trivially redeployable. No schema
migrations. No backup story to write.

## D09 — Replay mode is a first-class feature, not a test hack

**Context.** Testing the data path is the thing most likely to go wrong.
Mocks drift from the real server. CI can't have hardware.

**Decision.** `ferrited --source file://path.iq --loop` is a supported
mode of the same binary that runs in production. All tests — protocol,
fixture, E2E — drive this mode.

**Consequence.** No mock server. The thing CI tests is the thing users
run. Bonus: users get "replay last night's band opening" as a feature.

## D10 — Target OS is Ubuntu 24.04 LTS or newer

**Context.** SDR userspace (Soapy, kernel USB policy, udev) varies across
distros. We can support all of Linux, or we can pick one and do it well.

**Decision.** Ubuntu 24.04 LTS (Noble) for development and deployment.
Other Linuxes probably work; we do not test or promise them. Non-Linux
hosts are out of scope.

**Consequence.** Build instructions in `06-build.md` and deploy in
`07-deploy.md` name specific apt packages. CI runs on
`ubuntu-latest`. Contributor setup is one copy-pasted block of
commands.

## D11 — Web stack: Svelte 5 + Bits UI + Tailwind v4 + Dockview

**Context.** SDR UIs need dense panel layouts, many custom widgets
(frequency dial, waterfall, switches), and responsive updates. Avoid
heavy React ecosystems; avoid unstyled headless kits that require
reinventing everything.

**Decision.**
- **Svelte 5 (runes)** — small bundle, excellent imperative escape
  hatches for the WebGL waterfall.
- **SvelteKit with `adapter-static`** — no Node frontend server needed;
  `ferrited` serves the static tree.
- **Bits UI** — headless Svelte primitives for accessible widgets.
- **Tailwind v4** (Vite plugin) — styling.
- **Dockview** — docking panel layout (waterfall, message list, map,
  etc., rearrangeable).

**Consequence.** Two build outputs only: `target/release/ferrited` and
`web-dist/`. Deployment ships both from the same host.

## D12 — SharedArrayBuffer for the audio ring, COOP/COEP everywhere

**Context.** Audio from the flowgraph Worker to the AudioWorklet has to
cross a thread boundary without allocations. `postMessage` with
Transferable buffers is too high-latency for sustained audio.

**Decision.** A lock-free SPSC ring in a `SharedArrayBuffer` between
the flowgraph Worker (producer) and the AudioWorklet (consumer). This
requires cross-origin isolation; every response must carry COOP
`same-origin` and COEP `require-corp`.

**Consequence.** Headers set in dev (Vite plugin) and prod (`ferrited`
static serve). Required, documented, tested (SAB ring-buffer stress
test in CI).

## D13 — Auto-generated options dialog from Soapy capability schema

**Context.** Every Soapy driver exposes different knobs (RTL-SDR has
bias-tee and direct-sampling; SDRPlay has bias-tee, RF/DAB notches,
LNA state, tuner selection). Hard-coding UI for each driver is a
treadmill.

**Decision.** `GET /api/devices` returns a full capability schema
(rates, freq ranges, named gain elements, antennas, per-driver settings
from `getSettingInfo`). The frontend renders a generic options form
from this schema using Bits UI primitives.

**Consequence.** Adding a new Soapy driver adds new UI automatically
as long as the driver populates `getSettingInfo` correctly. No
driver-specific frontend code.

## D14 — LLM signal identify proxied server-side

**Context.** The identify feature sends a waterfall image + metadata
to an LLM and returns structured guesses. Doing this from the browser
exposes the API key and moves the RAG index off the server (where
sigidwiki data lives).

**Decision.** `POST /api/identify` on `ferrited`. Backend retrieves
top-N sigidwiki candidates by frequency+bandwidth match, composes the
vision prompt, calls the configured LLM. API key in
`/etc/ferrite/ferrited.toml`. Backend can cache identical requests.

**Consequence.** API key never touches the browser. Sigidwiki scraper
(`tools/scrape-sigidwiki`) runs at build time using the MediaWiki API
(not HTML scraping), emits `data/sigidwiki.json` with CC-BY-SA
attribution.

## D15 — DMR / DSD ruled out (for now)

**Context.** DMR voice needs the AMBE vocoder, which is patent- and
license-encumbered. Shipping a DMR decoder in a permissively-licensed
project is a legal minefield.

**Decision.** DMR / DSD are not on the v0.1 decoder list, nor the
post-v0.1 roadmap. M17 (Codec2, open) fills the "digital voice" slot.

**Consequence.** Clean licensing story for v0.1. Revisit only if a
clean-room AMBE alternative lands.

## D16 — Conventional Commits, green-at-every-commit

**Context.** The project will grow. A messy history makes bisect
painful; a history where some commits don't build makes bisect useless.

**Decision.**
- **Conventional Commits** (`feat`, `fix`, `chore`, `docs`, `test`,
  `refactor`, `build`, `ci`). Scope optional but encouraged
  (`feat(server): …`).
- **Every commit green** — `cargo check/test/clippy`, `pnpm
  test/lint/check` all pass at every commit.
- **One conceptual change per commit.** If a diff can be split into
  "add a thing" + "use the thing," make it two commits.
- **Tests land with the code they test.**
- **No committed TODOs** — either do it, ticket it, or delete it.

**Consequence.** Bisect works. PRs are reviewable. History is useful.

## D17 — Ship `ferrite-headless` runtime in v0.1, ship the binary post-v0.1

> **Superseded by D19.** The "shared runtime" referenced here is now
> `runtime/` (Rust), not `packages/flowgraph-runtime/` (TS). The
> "ships-in-v0.1, binary post-v0.1" split still stands; only the
> language changed.

**Context.** The symmetric runtime story (browser + Node) is load-bearing
for future decoders that want to run headlessly. Whether the Node
binary itself ships in v0.1 is a separate call.

**Decision.** The shared runtime (`packages/flowgraph-runtime/`) ships
in v0.1. Environment sinks for Node (MqttSink, SyslogSink, SqliteSink)
and the `headless/` binary are a post-v0.1 release — but the package
structure is in place on day one so adding them is drop-in.

**Consequence.** No refactor tax later. Users who don't want the
sidecar don't pay for it.

## D18 — Single repo, pnpm + cargo workspaces

**Context.** Web, Rust, and shared TS packages all evolve together.
Splitting repos means cross-cutting changes become multi-PR
choreography.

**Decision.** One git repo. `Cargo.toml` workspace at the root for
`server/`, `blocks/`. `pnpm-workspace.yaml` at the root for `web/`,
`packages/*`, `tools/*`. Shared CI.

**Consequence.** One clone, one `pnpm install`, one `cargo fetch`.
Cross-package refactors land in one PR.

## D19 — Single Rust runtime + Rust/WASM blocks, no TS runtime

**Supersedes D05, adjusts D17.**

**Context.** D05 put the flowgraph runtime in TypeScript so the browser
and Node sidecar could share it. D02 kept DSP blocks in Rust
(dual-compile). That gave us two runtimes in practice: `ferrited` ran
a hardcoded pipeline natively, while the browser ran the TS runtime
over the WASM blocks. Phase D shipped this split (commits #77–#80:
`feat(runtime)` TS work) and then we hit the wall the split always
implied: *the server needs to load and run the same preset the browser
does* — not just agree on its JSON shape. Reconfigure events,
receiver/demod swaps, and cross-environment flowgraphs (SoapySource
server-side → Channelizer → WsBridge → FmDemod browser-side) all want
one runtime on both ends, not two runtimes with a wire-format contract.

**Decision.** One runtime, one language: **Rust**, dual-compile. A new
`runtime/` crate (rlib + cdylib, `wasm` feature) owns JSON parse,
validation, scheduler, block instantiation, tick pump, and lifecycle.
`ferrited` links it as a library; the browser imports it as a WASM
module. A preset is one cross-environment doc with per-block
placement; `env_split` carves the doc to one env and auto-inserts
`WsBridgeTx`/`WsBridgeRx` pairs on wires that cross the boundary,
allocating `stream_id` deterministically from `CROSS_ENV_STREAM_BASE`
(1000). Every block — sources, sinks, DSP, bridges — is Rust/WASM;
there is no TS `WsIqSource` in the destination architecture, the
browser half's "source" is simply the auto-inserted `WsBridgeRx`.

**Rejected.** Keep the TS runtime and add a second Rust runtime on the
server — the split we're trying to escape. Port TS runtime semantics
into a Rust port and call it "the same" — we'd still own two copies
forever.

**Consequence.**
- `packages/flowgraph-runtime/` and `packages/flowgraph-blocks/` are
  deleted at M4.
- Phase D commits #77–#80 (TS `feat(runtime)` work) are historically
  accurate for what shipped but are superseded by M1–M5 (see
  `10-commits.md`).
- New DSP blocks go in `blocks/` (Rust); do not add TS block wrappers.
- `mutable_while_streaming` gives way to a `reconfigScope` on each
  param: `Self` | `Downstream` | `SourceRestart`. The runtime diffs
  presets and applies the minimum necessary action.
- The Block trait currently lives in `blocks/`; `runtime/` depends on
  it. The dep direction will likely invert once tick-pump and
  lifecycle land — logged here so future-us isn't surprised.

## Revisiting decisions

Decisions here are not immutable — they are **load-bearing assumptions**.
If new information shows one is wrong, the fix is:

1. Add a new entry (`D19`, `D20`, …) describing the new decision and
   explicitly noting which earlier entry it supersedes.
2. Leave the old entry in place with a one-line "superseded by Dnn"
   prefix.

Never silently edit history. The log is what lets a future contributor
understand *why* things are the way they are.
