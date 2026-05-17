# 12 — Shipped vs. planned

A short audit of what the original master plan (kept locally; the gist is
echoed in [`09-decisions.md`](09-decisions.md)) called for vs. what
landed by 0.9.0. This doc is here so newcomers can tell at a glance which
parts of the codebase match the design intent and which parts were
redirected mid-flight.

## Big architectural change: flowgraph runtime moved Rust-side

**Planned.** A shared TypeScript flowgraph runtime in
`packages/flowgraph-runtime/`, env-agnostic at its core, run by *both*
the browser and a tiny Node sidecar (`ferrite-headless`). Same TS
interpreter on both ends, with environment-specific sources/sinks.

**Shipped.** Flowgraph runtime is a Rust crate
([`runtime/`](../runtime/)) that dual-compiles. Native target links into
`ferrited`. WASM target loads in a browser Worker via wasm-pack
([`runtime/src/wasm.rs`](../runtime/src/wasm.rs)). `ferrite-headless`
does not exist; the cross-env split (Tx/Rx WS bridge pair generated at
load time) achieves the "half on server, half on browser" effect without
a separate Node process.

**Why the redirect.** Two interpreters would drift; doubling the surface
buys nothing once Rust→WASM symmetry works. See
[D19](09-decisions.md#d19) for the call.

## Big addition: AI operator (`ferrite-ai`)

Nowhere in the original plan. The plan's signal-identification idea was
a one-shot `POST /api/identify` endpoint: client uploads a waterfall
PNG + metadata, server composes a vision prompt with RAG context from
sigidwiki, calls an LLM, returns a structured guess card.

Shipped instead: a **persistent chat sidecar** (`tools/ferrite-ai/`)
that wraps the Claude Agent SDK and gives the model a Bash shell with
`ferrite-ctl` and the capture-analysis tools on the PATH. The AI can
tune the rig, change presets, take captures, and read PNG renders — not
just classify a static image. Subscription billing via the local
`claude` CLI login (no API key in config). Reverse-log via
`X-Ferrite-Command` header → `ai::activity` events surfaces every
command back in the UI activity panel.

The original `/api/identify` shape is *not* implemented; the chat
sidecar is the user-facing identification path. The sigidwiki catalog
still ships and still powers a searchable UI panel — just the
classification entrypoint changed.

## Decoder roster vs. plan

Plan called for ADS-B, FT8 / WSPR, Codec2 / M17, APRS as the v0.1
subset; DMR/DSD deferred for AMBE patent reasons.

| Decoder | Planned? | Shipped? | Source |
| --- | --- | --- | --- |
| ADS-B (Mode S) | ✓ | ✓ | `blocks/native/dump1090` |
| AIS | — | ✓ | `blocks/native/rtl-ais` |
| APRS (AFSK + HDLC) | ✓ | ✓ | `blocks/native/multimon-ng` |
| EAS / SAME | — | ✓ | `blocks/native/multimon-ng` |
| FT8 / FT4 | ✓ | ✓ | `blocks/native/ft8` (kgoba `ft8_lib`) — also browser-side |
| WSPR | ✓ | ✓ | `blocks/native/wsprd` (WSJT-X core via Guenael) — also browser-side |
| RTTY / PSK31 / CW / MT63 / Olivia / Contestia / DominoEX / Throb / NAVTEX | — | ✓ | `blocks/native/fldigi` v4.2.11 (curated cores; link-vs-bridge on wasm) |
| RSID auto-mode | — | ✓ | `blocks/native/fldigi` (FldigiAuto — RSID hot-swaps the inner modem) |
| POCSAG / FLEX | — | ✓ | `blocks/native/multimon-ng` |
| Morse / DTMF | — | ✓ | `blocks/native/multimon-ng` |
| RDS | — | ✓ | `blocks/src/rds_demod.rs` (in-tree) |
| Codec2 / M17 | ✓ | — | Deferred |
| DMR / DSD | deferred | deferred | AMBE patent |
| `rtl_433` (ISM bundle) | — | ✓ | `blocks/native/rtl_433` (broader device coverage tracked for 1.0) |

Net: the full planned subset shipped (incl. WSPR — the plan expected
`ft8_lib` to host it, but it became a dedicated vendored `wsprd`), plus
the entire unplanned fldigi keyboard-mode family + RSID auto-detect and
the decoders that came along with `multimon-ng` / `rtl-ais`. FT8/FT4/
WSPR and all fldigi modes decode browser-side as well as on the server
(D28); the mode-specific decoder panels (FT8/WSPR map+table, ADS-B map,
APRS map+console, fldigi text console) have shipped.

## Audio noise reduction stack

**Planned.** Not called out specifically; "voice modes" was treated as a
single demod block.

**Shipped.** A five-stage chain
([`blocks/src/audio_nr/`](../blocks/src/audio_nr)): de-emphasis →
impulse blanker → adaptive notch → spectral subtraction (MMSE-LSA /
Wiener) → DFN3 neural denoiser. Per-preset tuning so AM doesn't get the
WBFM stack and SSB gets the neural denoiser at a different threshold.

## Force-params: preset-imposed hard overrides

**Planned.** Not contemplated. Original `compose_source` merge order
was placeholder hints → live `SourceConfig` (live state always won).

**Shipped.** `BlockInstanceDecl.force_params` is an optional third
layer that wins over live state. Used to pin known-good values per
preset — e.g. `wbam.json` carries `"force_params": {"agc_enable":
false}` because AM AGC pumping otherwise resets the AGC about once per
second. See [D24](09-decisions.md#d24) and
`runtime/src/compose.rs::apply_force_params`.

## Capture tooling

**Planned.** Recordings were a browser-side OPFS concern; server-side
recording explicitly deferred (D08).

**Shipped.** A server-side `Recorder` block lineage
([`blocks/src/record.rs`](../blocks/src/record.rs)) writes `.bin` byte
streams + `.json` sidecars (sample_rate, frame_size, center_freq,
capture_duration). Python tools render and analyse:
- [`tools/fft_to_png.py`](../tools/fft_to_png.py) — strip PNG with
  dark-theme styling matching the UI, frequency + time axes, axis
  labels.
- [`tools/fft_peaks.py`](../tools/fft_peaks.py) — histogram-based peak
  finder with sigma threshold and minimum-gap-kHz filtering, JSON or
  text output.

These tools are used by **both** the human operator (via the shell) and
the AI (via Bash). Same code path.

## Signal catalog vs. spectrum explorer

**Planned.**
- (a) Signal catalog scraped from sigidwiki MediaWiki API → static
  JSON in repo → searchable UI panel.
- (b) Whole-spectrum-at-a-glance explorer (Dockview panel,
  log-scale axis 0 Hz to 300 GHz, click-to-retune).

**Shipped.** (a) lives:
[`web/src/lib/presets/SignalCatalog.svelte`](../web/src/lib/presets/SignalCatalog.svelte)
+ [`web/src/lib/presets/catalog.ts`](../web/src/lib/presets/catalog.ts),
with sigidwiki-derived thumbnails inlined under
[`samples/sigidwiki/images/`](../samples/sigidwiki/images). The US
band-allocation ribbon overlays the spectrum line itself.

(b) is *not* shipped. The whole-spectrum explorer is parked.

## Visual flowgraph editor

**Planned.** Explicit non-goal for v0.1 (JSON-authored only). Stretch
goal noted.

**Shipped.** Still JSON. The block-params pipe
(`/api/pipeline/blocks` + `BlockSpec`) makes per-block live editing
trivial, but the topology editor doesn't exist.

## What the plan got right

- **One block crate, two compile targets** — held up. Same Rust
  source, `cargo test` covers native; Vitest covers the WASM bundle.
- **Replay-as-source** — `FileIqSource` is a registered source like any
  other; every E2E test uses it.
- **LAN-trust, no auth** — held up. Single listener, last-connect wins.
- **Source dialog generated from capability schema** — held up. Adding
  a new SoapySDR driver requires no frontend changes; the dialog
  renders from `getSettingInfo`.
- **Native↔WASM parity testing** — held up. Identical fixtures run
  against both compilations of each block.

## What didn't survive contact with reality

- The TS flowgraph runtime (replaced by dual-compiled Rust).
- `ferrite-headless` as a Node sidecar (the AI sidecar `ferrite-ai`
  exists, but for a different purpose — it's a chat surface, not a
  flowgraph runner).
- The `/api/identify` one-shot endpoint (replaced by persistent AI
  chat).
- M17 / Codec2 (no blocker, just unprioritised). WSPR did ship
  (dedicated `wsprd`, not the `ft8_lib`-hosted route the plan guessed).
- Mobile UI (still desktop-first by design).

## Pending for 1.0 — see [08-roadmap.md](08-roadmap.md)

- Broader `rtl_433` ISM-device coverage.
- `Mode A/C` follow-up to dump1090.
- sigidwiki sample/thumbnail backfill for the newest fldigi presets.

Shipped since the original plan: the fldigi keyboard-mode family +
RSID, FT8/FT4/WSPR, browser-side decode with the live node↔browser
swap, and the mode-specific decoder panels — ADS-B aircraft map,
APRS station map + packet console, FT8/FT4/WSPR decode table +
station map, and the fldigi text console.
