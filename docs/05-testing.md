# 05 — Testing

## Layers

```
unit (cargo test -p ferrite-blocks)
   ↓ block-internal correctness; isolated process()
integration in-runtime (cargo test -p ferrite-runtime)
   ↓ doc parse, env_split, scheduler ticking, lifecycle
integration in-server (cargo test -p ferrited)
   ↓ ferrited boots, /ws/preset, preset reconfigure, file output
unit + DSP in-browser (pnpm --filter @ferrite/web test)
   ↓ Vitest jsdom; runs the WASM runtime in Node, drives blocks
```

The web tests run the *real* Rust runtime WASM module under Node + jsdom —
not a TS shim. The runner architecture
([`web/src/lib/runner/`](../web/src/lib/runner/)) is the same code path the
production browser uses; the test just feeds it `FakeFrameClient` instead of
a real `WebSocket`.

## Rust tests

```bash
cargo test --workspace                # everything native
cargo test -p ferrite-blocks          # block unit tests + wbfm_e2e
cargo test -p ferrite-runtime         # doc/env_split/scheduler + dtmf_e2e
cargo test -p ferrited                # server smoke + record + dtmf cross-env
cargo check -p ferrite-blocks --target wasm32-unknown-unknown --features wasm
                                      # WASM build sanity (CI runs this)
```

### Notable integration tests

- [`server/tests/smoke.rs`](../server/tests/smoke.rs) — boots `ferrited` on
  an ephemeral port, exercises REST + `/ws/preset`, asserts pipeline
  lifecycle.
- [`server/tests/record_smoke.rs`](../server/tests/record_smoke.rs) — drives
  a `FileAudioSink`-shaped preset; opens the resulting WAV, checks it parses
  and isn't silent.
- [`server/tests/dtmf_cross_env.rs`](../server/tests/dtmf_cross_env.rs) —
  the DTMF canary across the env split: node-side
  `DtmfAudioSource → AmModulator → … → WsBridgeTx`, browser-side
  `WsBridgeRx → AmDemod → DtmfDecoder → EventsSink`. Asserts the digits
  `["1","2","3","4"]` arrive on the browser side over the real
  `/ws/preset` transport.
- [`runtime/tests/dtmf_e2e.rs`](../runtime/tests/dtmf_e2e.rs) — same DTMF
  chain in-process (no WS hop) plus a `split_for_environment` test that
  validates bridge insertion and `CROSS_ENV_STREAM_BASE` allocation against
  `flowgraphs/dtmf-e2e.json`.
- [`blocks/tests/wbfm_e2e.rs`](../blocks/tests/wbfm_e2e.rs) — golden-fixture
  FM round trip: synthesise 240 kHz IQ with a 1 kHz tone + 19 kHz pilot,
  demodulate via `FmDemod`, assert the output spectrum is dominated by
  those tones and the audio RMS is within 10% of the input.

### Examples

- [`blocks/examples/fft_through_pipeline.rs`](../blocks/examples/fft_through_pipeline.rs)
  — non-test harness: reads a captured FM IQ WAV, pipes through
  `FileIqSource → FFT → LogMagU8`, writes a PGM spectrum image. Useful for
  poking the spectrum chain offline. Run with `cargo run -p ferrite-blocks
  --example fft_through_pipeline`.

## Web tests

```bash
pnpm --filter @ferrite/web test         # Vitest, all
pnpm --filter @ferrite/web test:watch   # interactive
pnpm --filter @ferrite/web check        # svelte-check
```

Configured by [`web/vitest.config.ts`](../web/vitest.config.ts) — jsdom
environment, `src/**/*.{test,spec}.{ts,js}`.

Test files in [`web/src/lib/`](../web/src/lib/):

- `api/flowgraph.test.ts` — block-schema fixtures, `reconfig_scope` parsing.
- `audio/audioRingProcessor.test.ts`,
  `audio/ringBuffer.test.ts` — SAB ring fill/drain across a producer +
  AudioWorklet-shaped reader.
- `controls/freqParse.test.ts` — frequency-string parser (kHz/MHz/GHz).
- `controls/optionsModel.test.ts` — device-capability → choice-grid
  expansion, `MAX_RATE_CHOICES` cap, source-config validation.
- `presets/catalog.test.ts`, `presets/receivers.test.ts` — preset listing
  + receiver-recipe application.
- `presets/wbfmE2E.test.ts` — orchestration test of the WBFM store: preset
  load → source retune → demod knob tweak; verifies the dispatcher and the
  local mirror sync path. No WS / WASM init.
- `runner/runnerClient.test.ts` — main-thread side: `FakeWorker` round-trip
  for `RunnerRequest` / `RunnerResponse`.
- `runner/runnerCore.test.ts` — runs the *real* Rust runtime WASM in Node:
  doc split → load → tick end-to-end with `FakeFrameClient` subscriptions.
- `runner/runnerDsp.test.ts` — feeds a synthetic FM-modulated 1 kHz tone
  through `WsBridgeRx → FmDemod → AudioSink`, drains audio out of the
  ring, asserts a 1 kHz zero-crossing rate.
- `runner/rustRuntime.test.ts` — wasm-pack init, `version()` round trip,
  `parse_and_validate_doc` and `split_doc_for_environment`.
- `viz/waterfall.test.ts` — pixel↔frequency mapping across the waterfall.

## CI

Two workflows in [`.github/workflows/`](../.github/workflows/):

- **`rust.yml`** — Ubuntu 24.04, stable Rust + `wasm32-unknown-unknown`
  target. Runs `cargo fmt --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace --all-targets`, and
  `cargo check -p ferrite-blocks --target wasm32-unknown-unknown
  --features wasm`. Cached with `Swatinem/rust-cache`.
- **`web.yml`** — Ubuntu 24.04, pnpm 10.33.0, Node 22. Runs
  `pnpm -r lint`, `pnpm -r check`, `pnpm -r test`.

Both fire on push to `main` and on every PR.

## Pre-commit

`pnpm install` runs `lefthook install` via the root `prepare` script.
Hooks defined in [`lefthook.yml`](../lefthook.yml):

| hook            | runs on                                              |
|-----------------|------------------------------------------------------|
| `cargo-fmt`     | staged `*.rs` → `cargo fmt --all -- --check`         |
| `pnpm-lint`     | staged web files → `pnpm -r lint`                    |
| `pnpm-check`    | staged `*.{ts,svelte,tsx}` → `pnpm -r check`         |

`cargo clippy` and the workspace test runs are CI's job.

## Test data and provenance

`runner/runnerDsp.test.ts` and a handful of others synthesise their own
inputs in code (sine + FM modulation, DTMF tones); no large IQ fixtures
are checked in.

`samples/` holds local capture artifacts (gitignored). They aren't required
to run any test.

## What we don't do

- **No mocked WebSocket protocol.** The cross-env tests boot real
  `ferrited` and hit `/ws/preset` over a real connection.
- **No mocked Soapy.** The shipped tests use software sources
  (`SineSource`, `DtmfAudioSource`, `FileIqSource`) in place of hardware;
  hardware paths are exercised by manual smoke (see
  [07-deploy.md](07-deploy.md) for what to check on a fresh box).
- **No browser-binary E2E.** Vitest under jsdom + the real Rust runtime
  WASM covers the data path; no Playwright in CI.
