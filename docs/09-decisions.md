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

## D20 — Decoder growth is a separate roadmap; first post-M5 phase is analog listening, not decoders

**Context.** With M5 closed, the engine work is effectively done for
v0.x: Rust runtime, dual-compile blocks, JSON presets, per-VFO
channelizer, reconfigure via preset diff. The natural next question —
"what should Ferrite decode, and in what order?" — is large enough
(dozens of modes, several C-vendor lifts, patent-encumbered edge cases)
that it doesn't fit in `08-roadmap.md` without swamping the
platform-phase narrative. It also benefits from being driven
capability-first, not project-first (see
`research/CAPABILITIES_ROADMAP.md` and
`research/WASM_PORT_ASSESSMENT.md` for the feasibility groundwork).

**Decision.**

1. **Decoder roadmap lives in `docs/decoder-roadmap/`** — index at
   `README.md`, six phase files, plus a `90-vendor-port-guide.md`
   mechanics reference. Phases 1–2 are detailed; 3–6 are sketches that
   get fleshed out as their predecessors close.

2. **The first post-M5 phase is analog listening (Phase 1), not a
   decoder phase.** That phase ships the reusable helper blocks
   (`AmDemod`, `SsbDemod`, `Deemphasis`, `Squelch`, `Agc`, `Resample`)
   and the six listening presets (WBFM / NBFM / AM / USB / LSB / CW).
   Every later decoder chain starts with some subset of these; building
   them well once makes every subsequent phase cheap.

3. **The first C-vendor is multimon-ng (Phase 2), not dump1090.** Per
   `research/WASM_PORT_ASSESSMENT.md`, multimon-ng is the single
   cleanest codebase in the set and delivers five decoders (POCSAG,
   FLEX, DTMF, EAS, CTCSS) from one lift. Debug the `blocks/native/`
   tooling against the easiest target, not simultaneously with a
   harder port. dump1090 moves to Phase 3 alongside direwolf and
   rtl_433.

4. **ADS-B is Tier 3 (aviation data), not Tier 1.** Adjusts the
   natural reading of `docs/03-blocks.md`, which lists it under v0.1
   scope. The engineering is the same; the categorisation is just
   corrected.

5. **`blocks/native/` is built once in Phase 2 and reused by every
   subsequent vendor.** Shared shim header (`ferrite_port.h`),
   wasi-libc linking, golden-fixture harness. `liquid-dsp-sys` lifted
   as a sibling substrate in the same phase. No parallel
   infrastructure per vendor.

6. **Patent-encumbered digital voice (DMR/P25/NXDN/DAB/DRM) ships via
   a `ferrite-helper` native sidecar protocol**, not in-WASM. Ferrite
   distributes nothing patent-encumbered; users install helpers via
   their OS package manager. This is separate from the D17 Node
   sidecar and covers a different class of problem.

7. **Patent-free digital voice (M17, FreeDV) ships in-WASM via
   Codec2.** Strategic differentiator — "digital voice in your
   browser, no binary blobs".

**Rejected.**
- Fold decoder planning into `08-roadmap.md`. Too much volume; the
  platform narrative gets lost.
- Start post-M5 with the first C-vendor. Higher-risk — would force us
  to debug the `blocks/native/` pattern against a harder codebase
  (direwolf globals, dump1090 output coupling) before we've proved
  it works at all.
- Port wsjt-x for FT8. Fortran + FFTW is impractical; use `kgoba/ft8_lib`
  (pure C, WASM-friendly). Captured in Phase 4.

**Consequence.**
- `docs/decoder-roadmap/` is the authoritative forward plan for
  decoder growth; `docs/10-commits.md` pulls commit-level items from
  it as each phase opens.
- `research/WASM_PORT_ASSESSMENT.md` and
  `research/CAPABILITIES_ROADMAP.md` are the feasibility inputs the
  roadmap rests on — kept in `research/` (gitignored, exploratory)
  rather than promoted to `docs/`, because their value is
  point-in-time and the roadmap itself is the durable artefact.
- When a decoder ships, move the relevant line from the
  capabilities-roadmap table into a "shipped" list in the decoder's
  phase doc; the preset JSON + block source are then the living
  documentation.

## D21 — Waterfall tuning interactions: click-drag = SDR centre (expensive); right-click = VFO offset (cheap)

**Context.** There are two frequency knobs in every preset that carries
a `Channelizer`: the SDR's hardware LO (`source.center_freq_hz`, set
via `PATCH /api/source`) and the Channelizer's offset within the
wideband capture (`chan.freq_shift_hz`, set via `PATCH
/api/flowgraph`). They sit at very different cost tiers — moving the
LO is a device restart-ish operation (retune latency, gain
renegotiation, waterfall resets); moving the Channelizer offset is a
single-block `Self`-scope reconfigure, imperceptible. Phase D's
original "drag VFO on waterfall to retune" plan conflated them.

Today the UI only exposes the LO knob (Nixie widget + BandsPanel
presets both `PATCH /api/source`). The Channelizer offset has no UI
control at all, so every retune goes through the expensive path. This
matters more the moment multi-VFO lands — N parallel channelizers
sharing one wideband capture is the whole point.

**Decision.** Two distinct waterfall interactions, one per tier:

- **Click-and-drag the waterfall ≡ SDR centre re-tune.** Drags the
  whole spectrum. Debounced at mouse-up; commits via `PATCH
  /api/source` (so `SourceRestart` scope). Cursor shows "grabbing" +
  "heavy" visual affordance so the user feels the weight.
- **Right-click on a spectrum feature ≡ set VFO offset.** Places the
  channelizer at the clicked absolute frequency (computed as `clicked
  − source.center`). Commits via `PATCH /api/flowgraph` on the
  channelizer's `freq_shift_hz` param (`Self` scope, no source
  disturbance). Multi-VFO future: right-click menu offers "set
  VFO<n>" per channelizer in the preset.

Rationale for this mapping: drag is a **motion-weighted** gesture the
user expects to be continuous and costly (you move the whole picture);
right-click is a **point-weighted** gesture the user expects to be
instant (targeted, surgical). That matches the cost tiers.

**Rejected.**
- Single "drag to tune" that decides based on how far you dragged —
  too ambiguous, unpredictable.
- Always route tuning through the Channelizer (keep the SDR LO
  fixed). Loses access to spectrum beyond one decimated bandwidth,
  and the LO *does* need to move sometimes (different bands).
- A separate Channelizer-offset slider in a dialog — doesn't
  compose with the spectrum picture the user is already staring at.

**Consequence.**
- Phase D's "drag VFO on waterfall to retune" line in
  `docs/08-roadmap.md` was under-specified; this decision fills the
  gap.
- A new commit-plan item lands under "Post-M5" in
  `docs/10-commits.md`: `feat(web): waterfall tuning — drag = source
  center, right-click = channelizer offset`.
- When multi-VFO lands, the right-click menu grows a "set VFO<n>"
  submenu; the drag behaviour is unchanged.
- The `reconfigScope` contract (M3) gets its first real exercise from
  this UX — `freq_shift_hz` must be `Self`-scope so right-click feels
  instant. That's a concrete acceptance test for the scope machinery.

## D22 — Unify `WsIqSource` into `WsBridgeRx`

**Context.** `WsIqSource` and `WsBridgeRx` were developed on separate
tracks:

- `WsIqSource` shipped first — a `WasmOnly` IQ ingress block with an
  `IqRing`, `push_interleaved`, and a wasm-bindgen `pushIq` API. The
  JS host owns the WebSocket (via `FrameClient`), decodes frames, and
  pushes samples into the block's ring; the block emits onto its
  typed `IqF32` output at tick time. Originally a reskin of the TS
  `wsIqSourceBlock.ts` for preset-authored browser ingress.
- `WsBridgeRx` landed as the consuming half of the bridge pair that
  `env_split` auto-inserts on `node → browser` crossings. Its
  `process` body is a placeholder returning `Ok(Work::new())` — no
  transport. The first commit message said *"once M2 lands, this
  fills the output buffer from decoded WS frames"*; M2 landed for the
  Tx side (`IqBridgeSink` → `BridgeSink` unification → `FftU8` Tx →
  postcard `Frame` transport) but Rx was never paired up.

Both declare `Placement::WasmOnly` with a single `IqF32` output and a
`stream_id` param; both consume decoded samples pushed by a JS-side
transport. They are the same block wearing two names. That's exactly
what D19's "one runtime, one block inventory" position forbids.

**Decision.** Merge them. Keep the `WsBridgeRx` name — it's what
`env_split` synthesizes, it's what D19 names as the browser-half
"source", and the docs already reference it. Body becomes
`WsIqSource`'s verbatim: `IqRing` + `push_interleaved` + typed
`IqF32` output. Params: `stream_id` + `buffer_samples`.

The transport split stays the same — `FrameClient` + postcard decode
on the JS side, typed ring inside the block. The wasm-bindgen
`pushIq(blockId, floats)` keeps its JS name and signature; internally
it dispatches to `block_typed::<WsBridgeRx>`. The browser runner
iterates every `WsBridgeRx` instance at preset-load time, reads each
one's `stream_id` param, subscribes through `FrameClient`, and routes
incoming `IqF32` payloads through `pushIq`.

Future non-`IqF32` cross-env wires get parallel types — `WsBridgeRxFftU8`
and `WsBridgeRxEvents`, each a ~30-line clone with a different output
port type. The block framework's port types are static; the transport
itself (`Frame` enum + `to_postcard`/`from_postcard` + `decodeFrame`)
is already type-agnostic, so the duplication is an adapter, not a
parallel transport.

**Rejected.**

- Keep both blocks. No added capability; `WsIqSource` has no
  `env_split` synthesis path and `WsBridgeRx` has no transport.
  Duplication with zero leverage.
- Single polymorphic `WsBridgeRx` whose output-port type is set
  per-instance via a param. Would require changes to `BlockSpec`
  (today a type-level const method) and the runtime's port-type
  dispatch. Disproportionate cost for the modest three-variant
  family that replaces it.
- Generic byte-port type + downstream decoder adapter block per
  variant. Adds a block per variant *plus* a decoder — N+1 instead of
  N. No gain.

**Consequence.**

- `blocks/src/ws_iq_source.rs` is deleted; `blocks/src/ws_bridge.rs`
  grows the `IqRing` + push API on `WsBridgeRx`.
- Any code referencing `"WsIqSource"` (blocks re-export, runtime
  wasm facade, env_split tests, sample-preset JSON) becomes
  `"WsBridgeRx"`.
- The task-14 vitest E2E becomes implementable — the DTMF canary's
  browser half finally has a working IQ ingress.
- When a preset needs server→browser `FftU8` or `Events` delivery,
  add `WsBridgeRxFftU8` / `WsBridgeRxEvents` at that time. Not
  speculative.

## D23 — Rate-aware scheduler with accumulating ring buffers

**Context.** Today's `Runtime::tick` walks blocks in topological order
and calls each block's `process` exactly once per tick. Every output
port owns a pre-allocated buffer of size `max(frames_hint,
output_capacity_hints[i])`. The scheduler hands each block the full
output buffer; the block writes `Work.produced[i]` samples, and
downstream consumers see a slice trimmed to that count.
`Work.consumed[i]` is reported but *not honored* — unconsumed input
samples vanish when the upstream producer overwrites its output
buffer on the next tick.

This is correct for rate-reducing chains (wideband capture → narrow
channelizer → demod → audio) because consumers are always cheaper
than producers — there are no unconsumed samples to lose. Every
preset that ships today has this shape.

It is **silently wrong** for rate-expanding chains. The DTMF-e2e
canary's `AmModulator` takes 8 kS/s real audio and produces 2.4 MS/s
IQ (300× upsample). With `frames_hint = 1024`: the upstream source
emits 1024 real samples; `AmModulator`'s output-driven loop (`while
produced < dst.len()` with `step = 1/300`) consumes ~4 of them to
fill its 1024-IQ output buffer; the other ~1020 samples are
overwritten when `DtmfAudioSource` runs again on the next tick. The
effective audio rate reaching the AM modulator is ≈ 27 Hz, which
aliases the DTMF tones to noise. The downstream decoder cannot
recover what the scheduler threw away.

The issue isn't `frames_hint` (that's just a sizing knob, already
per-port via `output_capacity_hints`). It isn't even the buffer
sizes — a 300× buffer costs 2.4 MB per port and scales poorly. The
issue is the scheduler has no concept of "block X needs more input
before producing more output" — no back-pressure mechanism, no rate
awareness, no way for `Work.consumed` to mean anything.

**Decision.** Rebuild the scheduler on a GNU Radio-shaped foundation:
accumulating ring buffers per wire, per-block rate declarations, and a
demand-driven work loop that re-runs blocks as long as they can make
progress. The outer contract stays the same — one `tick()` per
`AudioWorklet` batch on the browser, one per reader batch natively —
but internally `tick()` is a loop, not a single pass.

**Block trait additions.**

- `fn relative_rate(&self, in_port: usize, out_port: usize) -> (u32, u32)` —
  returns `(output_samples, input_samples)` for a production step.
  Default `(1, 1)`. A `sync_decimator(N)` returns `(1, N)`; a
  `sync_interpolator(L)` returns `(L, 1)`; a source returns `(1, 0)`
  on the input-less side. `(0, 0)` means "unconstrained" — event
  blocks whose output rate isn't a function of input rate.
- `fn forecast(&self, noutput_items: usize) -> [usize; MAX_PORTS]` —
  answers *"to produce this many outputs, how many inputs do I need
  on each port?"*. Default derives from `relative_rate` for sync
  blocks; override for variable-rate blocks (e.g. `DtmfDecoder`
  emits events only on tone transitions, not per-input-sample).
- `Work.consumed[i]` becomes load-bearing. The scheduler advances
  each input ring's read pointer by `consumed[i]`; unconsumed
  samples remain for the next call to the same block. Today's
  blocks already populate `consumed` honestly, so the API shape
  doesn't change — only the scheduler's use of it does.

**Buffer model.** Each wire owns one `TypedRing` — a power-of-two
circular buffer. Fan-out wires (one producer, N consumers) have one
writer pointer and N independent reader pointers, so a slow
consumer doesn't stall the others on the same wire. Ring capacity
is derived at init time from the downstream's maximum forecasted
demand × a safety factor, with a floor at `frames_hint` (preserving
today's "at least one tick of headroom" guarantee). `TypedBuf` is
retired; `output_capacity_hints` folds into the ring-sizing math.

**Scheduler loop.** `Runtime::tick()` becomes:

```text
loop:
  progress = false
  for block in topo_order:
    nout = min(output_ring_free, block.forecast_max_output())
    if nout == 0: continue          # output backed up — skip
    if !block.has_enough_input(nout): continue  # starved — skip
    work = block.process(io_with_ring_views)
    advance rings by work.consumed[*] / work.produced[*]
    if work.consumed > 0 || work.produced > 0: progress = true
  if !progress: break               # quiescent
  if tick_budget_exhausted: break   # keep WASM ticks bounded
```

`tick_budget_exhausted` counts samples processed on the
highest-rate output and caps one `tick()`'s total work. Keeps the
`AudioWorklet` batch predictable and prevents runaway loops if a
rate declaration is buggy.

**Rejected.**

- **Per-port `output_capacity_hints = frames_hint × rate_ratio`.**
  Works up to some ratio ceiling but the 300× IQ case is 2.4 MB per
  port, and the overwrite-on-next-tick semantics still mean
  back-pressure isn't real — just delayed. Fixes a symptom, leaves
  the root cause (scheduler ignores `consumed`).
- **Per-block `frames_hint`.** Already exists as
  `output_capacity_hints`. A sizing knob, not a rate-awareness
  mechanism. Doesn't touch the scheduler's "overwrite" assumption.
- **Thread-per-block scheduler (GR's TPB).** Not viable in the
  single-worker WASM model; native benefits don't justify the
  complexity for any graph we have or plan to have.
- **Synchronous dataflow compile (compute a static schedule at init
  and tick each block K_i times per outer tick).** Elegant but
  requires every block's rate to be rational and known at init;
  variable-rate blocks (`DtmfDecoder`) break the premise. The
  demand-driven loop handles variable rates naturally.

**Consequence.**

- `runtime/src/runtime.rs` gets a new `TypedRing` type and a
  rewritten `tick()`. The module doc's "back-pressure is coarse"
  disclaimer disappears.
- Every block migrates. Most are 1:1 sync and keep the default
  `relative_rate`. Explicit overrides for `DtmfAudioSource` (source
  rate), `AmModulator` (`(L, 1)`), `Channelizer` / `Decimator`
  (`(1, N)`), `FFT` / `LogMagU8` (fixed-chunker — already using
  `output_capacity_hints`, folds into `forecast`), `DtmfDecoder`
  (variable — custom `forecast`).
- Per-block unit tests that stand up `InputPort { buf:
  InBuf::…(&slice) }` change shape. A small `TestRing` harness
  keeps them terse.
- `output_capacity_hints` is retired; ring sizing derives from rate
  declarations + `frames_hint`.
- DTMF-e2e's AM round-trip becomes runnable in-process, validating
  the full canary end-to-end rather than a rate-sidestepping variant.
- Lands after D22 (WsBridgeRx unification), because D22 unblocks a
  reduced DTMF E2E today while D23 is being built; the "before" and
  "after" states are each testable.

## Revisiting decisions

Decisions here are not immutable — they are **load-bearing assumptions**.
If new information shows one is wrong, the fix is:

1. Add a new entry (`D19`, `D20`, …) describing the new decision and
   explicitly noting which earlier entry it supersedes.
2. Leave the old entry in place with a one-line "superseded by Dnn"
   prefix.

Never silently edit history. The log is what lets a future contributor
understand *why* things are the way they are.
