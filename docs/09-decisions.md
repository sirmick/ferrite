# 09 — Decision log

Architectural choices that shape the code today, in the order they were
made. Each entry is **context → decision → consequence**. Superseded
entries get a one-line prefix and stay in place — the log is what lets a
future contributor understand *why* the code looks the way it does.

## D01 — Split the DSP: server does FFT + channelization, client does demod + decode

**Context.** The SBC is small; doing demod for every connected client there
balloons the backend. But every browser tab can't compute its own wideband
FFT from raw IQ either.

**Decision.** `ferrited` produces a wideband FFT (the waterfall) and
narrowband IQ per active VFO; everything downstream lives in the browser
(WASM blocks, Worker runtime).

**Consequence.** Server stays small. New decoders are blocks + a flowgraph
JSON, not a server release. Implemented end-to-end in the shipped
`flowgraphs/wbfm.json` chain (server-side `FFT → LogMagU8 → ui:fft` tap +
server-side `Channelizer` feeding a cross-env wire to the browser-side
`FmDemod`).

## D02 — Dual-compile DSP blocks: one Rust crate, two targets

**Context.** Server needs a native channelizer; browser needs the same DSP
kernels. Two languages or two implementations invites drift.

**Decision.** [`blocks/`](../blocks/) is one Rust crate with
`crate-type = ["rlib", "cdylib"]` and a `wasm` feature. `cargo build` →
native objects linked into `ferrited`; `wasm-pack build … --features wasm`
→ a wasm-pack package the browser loads. `#[ferrite_block]` registers
blocks at link time via `inventory` in both environments.

**Consequence.** One codebase for DSP. The wire-format `Frame` enum lives
in this crate; the browser decoder is the same code as the server encoder.

## D03 — Port C decoder cores with `clang --target=wasm32`, not Emscripten

**Decision.** When a C core is vendored: keep the pure DSP, drop stdio /
sockets / audio glue. Build twice — `cc` crate for native, `clang
--target=wasm32-unknown-unknown` (+ wasi-libc for libc bits) for WASM.
Wrap in a Rust `Block`. **Rejected:** Emscripten — drags in a JS shell and
`fs` polyfill irrelevant to a DSP kernel.

**Status.** No C cores are vendored yet; the strategy is on the books for
the first port (sequencing in [`docs/decoder-roadmap/`](decoder-roadmap/)).

## D04 — JSON flowgraph configuration, no visual editor

**Decision.** Flowgraphs are JSON files in [`flowgraphs/`](../flowgraphs/).
Ship a curated set; the runtime validates ports and params before running.
No graphical editor.

**Consequence.** Adding a decoder is a JSON file + any new blocks.
Block param schemas come from `BlockSpec::params` (static `&[ParamSpec]`),
exposed via `GET /api/blocks` so the dialog UI is generic.

## D05 — Flowgraph runtime is a shared TypeScript package

> **Superseded by D19.** A TS runtime shipped through Phase D; once WBFM ran
> end-to-end the symmetric runtime broke down on first contact with
> reconfigure / cross-env presets. D19 replaces it with a single Rust
> runtime (dual-compile).

## D06 — Single listener, last-connect wins

**Decision.** v0.1 supports one active session. The server holds *one*
preset and one source config on `AppState` ([`server/src/app_state.rs`](../server/src/app_state.rs));
no `SessionState`. New `/ws/preset` clients subscribe to the same
`FrameBus`; backpressure drops only the slow subscriber's copy.

**Consequence.** Simplest possible server. Channelizer pool architecture
keeps "lift this restriction later" an extension, not a rewrite.

## D07 — LAN trust, no auth

**Decision.** No authentication on any `ferrited` endpoint. Remote access
is the user's tunnel problem (Tailscale, WireGuard, reverse proxy with its
own auth).

**Consequence.** Same posture as KiwiSDR / OpenWebRX. Documented in
[07-deploy.md](07-deploy.md) so nobody accidentally exposes `ferrited` on a
public IP.

## D08 — User data lives in the browser

**Decision.** localStorage for prefs, presets, tuning. Zero server-side
state for the user.

**Consequence.** `ferrited` is trivially redeployable — no schemas, no
migrations, no backups. Bookmarks are device-specific; that's an accepted
trade.

## D09 — Replay mode is a first-class feature, not a test hack

**Decision.** `FileIqSource` is a registered source like any other.
Selecting it via `--source-type FileIqSource --source-path …` (or via the
source dialog) substitutes for an SDR. Tests use the same path.

**Consequence.** No mock server, no stubbed Soapy. CI exercises the same
binary that runs in production.

## D10 — Target OS is Ubuntu 24.04 LTS or newer

**Decision.** Develop and deploy on Ubuntu 24.04. Other Linuxes probably
work; non-Linux hosts are out of scope.

**Consequence.** [`README.md`](../README.md) names specific apt packages.
CI runs on `ubuntu-24.04`. SoapySDR is built locally via
[`scripts/build-soapysdr.sh`](../scripts/build-soapysdr.sh) to avoid
distro-package skew.

## D11 — Web stack: Svelte 5 + Bits UI + Tailwind v4 + SvelteKit (adapter-static)

**Decision.**
- Svelte 5 runes for fine-grained reactivity (frequency dial, S-meter,
  frame counters update constantly).
- SvelteKit `adapter-static` so `ferrited` serves the bundle directly — no
  Node in production.
- Bits UI for headless accessible primitives.
- Tailwind v4 via the Vite plugin.

**Consequence.** Two build outputs — `target/release/ferrited` and
`web/build/`. Deployment ships both from the same host.

## D12 — SharedArrayBuffer for the audio ring; COOP/COEP everywhere

**Decision.** A lock-free SPSC ring in a `SharedArrayBuffer` carries audio
from the runner Worker to the AudioWorklet
([`web/src/lib/audio/ringBuffer.ts`](../web/src/lib/audio/ringBuffer.ts)).
SAB requires `COOP: same-origin` + `COEP: require-corp` on every response.

**Consequence.** Headers set in dev
([`web/src/lib/vite/coop-coep.ts`](../web/src/lib/vite/coop-coep.ts)) and
prod (`server/src/main.rs:313-314`). Without them, audio silently dies.

## D13 — Auto-generated source dialog from SoapySDR capability schema

**Decision.** `GET /api/devices` returns the full driver capability schema
(rates, freq ranges, named gain elements, antennas, per-driver settings
from `getSettingInfo`). The frontend renders the source-dialog form from
this schema.

**Consequence.** Adding a new SoapySDR driver doesn't add UI code as long
as the driver populates `getSettingInfo` properly. `GET /api/source/capabilities`
re-uses the same shape for the *currently-active* source so the UI doesn't
re-probe on every change.

## D15 — DMR / DSD ruled out

**Decision.** AMBE is patent-encumbered; DMR/DSD aren't on the decoder
list. M17 (Codec2, open) covers the digital-voice slot if/when that lands.

**Consequence.** Permissive licensing across the project.

## D16 — Conventional Commits, green-at-every-commit

**Decision.**
- Conventional Commits with optional scope: `feat(server): …`, `fix(blocks): …`.
- Every commit must pass `cargo check/test/clippy` and `pnpm test/lint/check`.
- One conceptual change per commit; tests land with the code they test.
- No committed TODOs.

**Consequence.** Bisect works. Pre-commit hooks ([`lefthook.yml`](../lefthook.yml))
enforce fmt/lint at commit time; CI runs the full suite.

## D17 — One `ferrited` binary, no separate sidecar

> **Adjusted by D19** — the "shared runtime" is now `runtime/` (Rust),
> not `packages/flowgraph-runtime/` (TS).

**Decision.** Headless flowgraph runs are a second `ferrited
--flowgraph <preset.json>` instance — same binary, same blocks. The
recording presets ([`flowgraphs/{capture_fm,fm-audio-record,am-audio-record}.json`](../flowgraphs/))
demonstrate the shape; `--run-for-secs N` is the right knob for capture-
to-disk runs that exit cleanly.

**Consequence.** One artifact to package, ship, and update.

## D18 — Single repo, pnpm + cargo workspaces

**Decision.** One git repo. `Cargo.toml` workspace at the root for
`server/`, `runtime/`, `blocks/`, `blocks-macros/`. `pnpm-workspace.yaml`
for `web/`, `tools/*`.

**Consequence.** One clone, one `pnpm install`, one `cargo fetch`.
Cross-package refactors land in one PR.

## D19 — Single Rust runtime + Rust/WASM blocks; no TS runtime

**Supersedes D05; adjusts D17.**

**Context.** D05 put the runtime in TS so browser and a Node sidecar could
share it. D02 kept blocks in Rust. That gave us two runtimes in practice:
`ferrited` ran a hardcoded native pipeline while the browser ran a TS
runtime over WASM blocks. Phase D shipped that split, then we hit the wall
it implied: the server has to *load and run the same preset the browser
does* — not just agree on its JSON shape. Reconfigure events, decoder
swaps, and cross-env flowgraphs all want one runtime on both ends.

**Decision.** One runtime, one language: **Rust**, dual-compile.
[`runtime/`](../runtime/) (rlib + cdylib, `wasm` feature) owns JSON parse,
validation, scheduler, block instantiation, lifecycle. `ferrited` links it
natively; the browser imports it as a WASM module via the
`RuntimeHandle` facade ([`runtime/src/wasm.rs`](../runtime/src/wasm.rs)).
A preset is one cross-environment doc with per-block `placement`;
`split_for_environment` ([`runtime/src/env_split.rs`](../runtime/src/env_split.rs))
carves it to one env and auto-inserts `WsBridgeTx`/`WsBridgeRx` pairs on
boundary-crossing wires.

**Consequence.**
- TS runtime/blocks packages were deleted at M4.
- New DSP work goes in [`blocks/`](../blocks/) only.
- `mutable_while_streaming` was replaced by `ReconfigureScope`
  (`SelfBlock` / `Downstream` / `SourceRestart`) on each `ParamSpec`.
- The `Block` trait lives in `blocks/`; `runtime/` depends on it.

## D20 — Decoder growth has its own roadmap; first post-M5 phase is analog listening

**Context.** With the engine done (Rust runtime, dual-compile, JSON
presets, channelizer, reconfigure-by-diff), the next question is "what
should Ferrite decode, and in what order?". That sequencing is large enough
to swamp [`08-roadmap.md`](08-roadmap.md) and benefits from being
capability-first.

**Decision.**

1. Decoder roadmap lives in [`docs/decoder-roadmap/`](decoder-roadmap/).
2. The first post-M5 phase is analog listening, not a decoder phase. It
   ships the reusable helper blocks (`AmDemod`, `SsbDemod`, `Deemphasis`,
   `Squelch`, `Agc`, `Resample`) and the listening presets.
3. The first C-vendor port is `multimon-ng` (smallest clean codebase,
   delivers POCSAG/FLEX/DTMF/EAS/CTCSS in one lift) — not dump1090.
4. Patent-encumbered digital voice (DMR/P25/NXDN) is out (D15).
5. Patent-free digital voice (M17, FreeDV) ships in-WASM via Codec2.

**Consequence.** [`docs/decoder-roadmap/`](decoder-roadmap/) is the
authoritative forward plan for decoder growth. As decoders ship, their
preset JSON + block source become the living documentation.

## D22 — Unify `WsIqSource` into `WsBridgeRx`

**Context.** Two blocks doing the same job: `WsIqSource` (`WasmOnly` IQ
ingress, `IqRing` + `pushIq`, no `env_split` synthesis path) and
`WsBridgeRx` (synthesised by `env_split`, no transport implementation).

**Decision.** Merge them. Keep the `WsBridgeRx` name (what `env_split`
synthesises and what D19 names as the browser-side "source"). Body becomes
`WsIqSource`'s verbatim — `IqRing` + `push_interleaved` + typed `IqF32`
output. The wasm-bindgen `pushIq` (now `RuntimeHandle::push_iq`) keeps its
shape.

**Consequence.** One block, one transport. The browser runner subscribes
each `WsBridgeRx` instance through `FrameClient` by `stream_id` and routes
incoming `IqF32` payloads through `push_iq`. Future non-`IqF32` cross-env
deliveries get `WsBridgeRxFftU8` / `WsBridgeRxEvents` clones at the time
they're needed.

## D23 — Rate-aware scheduler with accumulating ring buffers

**Context.** The original scheduler called each block's `process` once per
tick and overwrote per-port output buffers between ticks; `Work.consumed`
was reported but ignored. Correct for rate-reducing chains; silently broken
for rate-expanding chains (DTMF canary's 300× AM upsample lost samples).

**Decision.** `TypedRing`
([`runtime/src/typed_ring.rs`](../runtime/src/typed_ring.rs)) is a
power-of-two circular buffer per wire with one writer head and N reader
heads (fan-out is one wire, multi-reader). The scheduler advances each
reader by `Work.consumed[i]`; samples persist across ticks. `Runtime::tick`
re-runs blocks demand-driven until quiescent or `MAX_TICK_PASSES = 1024`.

**Block trait additions** ([`blocks/src/block.rs`](../blocks/src/block.rs)):
- `relative_rate(in, out) -> (u32, u32)` — sync-block ratio.
- `forecast(noutput) -> Option<[usize; MAX_PORTS]>` — variable/chunked.
- `apply_live_params(delta) -> Result<bool>` — in-place params, optional.

**Consequence.** Rate-expansion chains work end-to-end; the DTMF canary
runs the real authored preset.

## D24 — Generic block-params pipe: one dispatcher, one component

**Decision.** Make `ParamSpec` the source of truth and route all
DSP-affecting writes through one dispatcher branching on `placement`.

- `ParamSpec` carries `key`, `label`, `kind` (with `min`/`max`/`step`/`unit`
  on numeric kinds), and `reconfig_scope`.
- Server: `GET /api/pipeline/blocks` (current preset's composed blocks +
  spec + values) and `POST /api/pipeline/blocks/:id/params` (delta).
- Browser: `RuntimeHandle::reconfigure_block(id, deltaJson)` mirrors the
  REST endpoint over the WASM facade.
- Web: `setBlockParam(id, key, value)` in
  [`web/src/lib/pipeline.svelte.ts`](../web/src/lib/pipeline.svelte.ts)
  routes by `placement`; one `<BlockParams>` Svelte component renders
  every block's params from `ParamKind`.

**Consequence.** Adding a new block makes its params editable in the
receiver pane with no Svelte changes. The scope-merging logic in
`ReconfigureScope::merge` graduates from a design note to a runtime-
enforced invariant.

## D25 — Spectrum interaction: left-click = VFO; right-click = SDR centre

**Supersedes D21** (which had it the other way around).

**Context.** Field trial of D21's mapping (drag = SDR centre, right-click
= VFO) found users overwhelmingly read "click on a peak" as "tune there",
and the drag affordance for an expensive-retune-sometimes interaction
confused.

**Decision.**
- **Left-click on the spectrum ≡ set VFO.** `SelfBlock`-scope reconfigure
  on `channelizer[0].center_freq_hz`. Primary tuning gesture.
- **Right-click on the spectrum ≡ set SDR centre.** `SourceRestart`-scope
  on `source.center_freq_hz`. Secondary "different band" gesture.
- **No drag-to-retune.** Drag is reserved for future pan/zoom interactions.

**Consequence.** Click handlers are one-liners on `setBlockParam` (D24).

## D26 — Preset-first UX: dir-based registry, `bands.json` `preset` field, fixed layout

*Item 2 (`bands.json preset` field) superseded by D27. Items 1 and 3 stand.*

**Decision.** Three coupled changes:

1. **Dir-based preset registry.** `GET /api/presets` enumerates
   `flowgraphs/*.json`; `POST /api/preset {name}` swaps atomically and
   retains tuning across the swap (user thinks "I changed decoder, not
   station"). Header dropdown is the canonical preset switcher.
2. **`bands.json` entry gains optional `preset` field.** Clicking a band
   entry switches preset (if different), then writes both
   `source.center_freq_hz` and `channelizer[0].center_freq_hz`.
3. **Fixed spectrum-over-waterfall layout.** Movable-pane machinery
   removed; the layout is always spectrum on top, waterfall below, one
   resizable divider.

**Consequence.** The layout matches every SDR UI users are familiar with
and simplifies hit-testing for D25's click-to-tune.

## D27 — Catalog ↔ bands separation: catalog is *what to demod*, bands is *where to listen*

Supersedes D26 item 2.

**Decision.** Two coupled changes that pull apart the conflated role
`bands.json` had under D26:

1. **Catalog entries (`flowgraphs/*.json` shown in the Signal Catalog)
   carry demod topology only.** `src.center_freq_hz` and
   `chan.freq_shift_hz` hints stripped from every shipped catalog
   preset. Picking a mode no longer retunes the SDR — only the demod
   chain swaps.
2. **Bands entries (`web/src/lib/presets/bands.json`) carry frequency +
   optional VFO offset.** The `preset` field is gone; clicking a band
   *only* writes `flow.src.center_freq_hz` (and
   `flow.chan.freq_shift_hz` when the entry needs offset tuning to
   dodge the SDR's DC spike, e.g. the APRS row). It does not swap the
   catalog mode.

**Why.** Under D26 a band click did both — swapped flowgraph *and*
retuned. That coupled two independent user intents ("I want to listen
to APRS" vs "tune to 144.39") and made it unclear which one a user was
expressing on any given click. After the split, picking a mode and
picking a frequency are clean independent gestures: the Signal Catalog
chooses *what to demod*, the Bands panel chooses *where to listen*.
APRS-style offset tuning is preserved via the new `vfo_offset_hz`
field on band entries (the field is read; preset-coupled flowgraph
swap is not).

**Consequence.** Server-side `compose_source` and the pipeline-side
hint readers (`presetSrcFreqHint`, `presetVfoHint`, `restoreVfo`) are
left intact — a future preset can still bring frequency back if a use
case appears. No machinery deleted; only the data carrying it.

## D28 — Browser-side decode + unified event transport (placement is invisible above the runner)

**Decision.** Make every decoder block genuinely placement-`Either` and
let the demod-placement chip move it node↔browser **live, no reload**.
The blockers were closed rather than worked around:

- fldigi (C++/STL, no clean wasm path) rides a *link-vs-bridge*: Rust
  wasm declares the modem `extern "C"` ABI as imports; a sibling
  Emscripten module (`blocks/native/fldigi/emscripten/`, built in CI
  via emsdk) satisfies them, instantiated lazily only when a fldigi
  block is browser-placed.
- FT8/WSPR were `Either` but absent from the runtime `wasm` feature and
  panicked on `wasm32` (`SystemTime::now()`); fixed via `web_time` + the
  feature flip + an 8 MiB wasm shadow stack (wsprd's working set).
- **Event transport unified at the consumption seam, not a new bridge
  block.** A browser-side `ui:` `Events` producer terminates in a
  drainable `EventsSink` (`__ui_<name>_<sid>`, sid == the node half's);
  the runner drains it per-tick and loopbacks via
  `FrameClient.injectLocal`, so the ft8/wspr/fldigi/aprs/adsb/rds
  stores are byte-for-byte unchanged and don't know which side ran.
  `pipeline.uiSinks` is a `$derived` over node `/api/ui-sinks` +
  browser-split sinks so the advanced view attaches either way.

**Why not a single `EventBridge` block.** Considered and declined: it
would rewrite the proven node WS path for a tidiness win, moving the
node-vs-browser branch inside a block rather than removing it. The
convergence-at-`FrameClient` design erases the only divergence that
mattered (to the UI) at zero risk to the shipping node path; the two
`env_split` block types never leak above the runner.

## Revisiting decisions

Decisions are load-bearing assumptions, not immutable. To reverse one:

1. Add a new entry (`Dnn`) describing the new decision and explicitly
   noting which earlier entry it supersedes.
2. Leave the old entry in place with a one-line "superseded by Dnn" prefix.

Never silently edit history.
