# 05 — Testing strategy

## Guiding principle

The data path is the thing most likely to break and the thing hardest to test
after the fact. We spend our testing budget on making it **cheap, fast, and
deterministic to exercise the real data path** without hardware.

The single biggest lever: **replay mode is a first-class `ferrited` feature**,
not a test hack.

```
ferrited --source file://fixtures/wbfm.iq --rate 2.4e6 --freq 100e6 --loop
```

Replay mode means every layer of the test pyramid — block unit tests, protocol
conformance, browser E2E — runs against the **same binary shipped to users**.
No mock server. No stubbed Soapy. No divergence between "what CI tests" and
"what runs on a Pi." As a bonus, users get "replay last night's band opening"
as a user feature.

## Test layers (cheapest first)

The more work we push into lower layers, the faster CI is and the quicker the
feedback loop for a contributor.

### 1. Block unit tests

Every block gets unit tests that:

- Feed it a synthetic or fixture input.
- Assert a property of the output (a tone at a known freq, an RMS within a
  band, a frame that decodes to a known payload).
- Run natively via `cargo test` **and** in WASM via `wasm-bindgen-test`
  against the same fixtures.

The block's `process()` is a pure function from `(state, inputs)` →
`(state', outputs)`. Unit tests exercise it directly. No runtime, no WS,
no scheduler. This is where the bulk of DSP bugs get caught.

```rust
#[test]
fn fm_demod_recovers_1khz_tone() {
    let iq = synth_fm(1_000.0, 48_000, 0.5);  // 1 kHz tone, 0.5 rad peak
    let mut demod = FmDemod::new(default_params()).unwrap();
    let out = drive(&mut demod, &iq);
    assert_peak_freq(&out, 1_000.0, tol_hz = 5.0);
}
```

### 2. Golden fixture tests

Small recorded IQ clips of known signals, checked into the repo (git-LFS once
they exceed ~5 MB total). Each fixture has a manifest describing what it is
and what a correct decoder must produce from it.

```
fixtures/
  wbfm_1khz_pilot.iq          # FM broadcast fragment, 200 kHz BW, pilot @ 19 kHz
  wbfm_1khz_pilot.json        # { rate, center, expected: { rms, pilot_hz } }
  adsb_df17_known.iq          # one ADS-B DF17 burst
  adsb_df17_known.json        # { expected_hex: "8D4840D6202CC371C32CE0576098" }
```

A fixture that passes today **never stops passing while the decoder is
correct**. When it fails, one of two things is true: the decoder regressed,
or the fixture was wrong — either way, human attention is justified.

Fixture discipline:

- **Small.** 1–10 seconds at the block's natural sample rate. No full captures.
- **Narrowband.** Capture at the block's input rate, not the SDR's raw rate,
  to keep files small and to focus the test on one block.
- **Self-describing.** The `.json` sidecar is authoritative metadata.
- **One signal per file** except when testing deliberate interference.
- **License-clean.** Signals recorded by us or from public domain / CC
  sources, documented in `fixtures/LICENSE.md`.

### 3. Property tests

`proptest` (Rust) and `fast-check` (TS) for invariants that should hold for
**any** input, not just the fixtures we thought of:

- **FFT Parseval**: `sum(|x[n]|²) == sum(|X[k]|²) / N` within FP tolerance.
- **Decimator rate invariant**: output samples = `floor(input / ratio)`.
- **Frame serializer round-trip**: `deserialize(serialize(frame)) == frame`
  for every generated frame.
- **Ring buffer**: producer never overwrites unread data; consumer never
  reads past producer.

Property tests are cheap once written and catch edge cases fixtures miss.

### 4. Synthetic loopback

A `SignalSource` block generates test signals (sine, chirp, WGN, modulated
tone) in-process. Compose it with the block under test:

```
SignalSource(FM, 1 kHz, 0.5 rad) → FmDemod → assert_tone(1 kHz ± tol)
```

No fixture file. No hardware. Runs in the runtime the same way a real
flowgraph would. Catches integration bugs that pure-unit tests miss (wrong
sample rate plumbing, buffer sizing, lifecycle).

### 5. Wire-protocol conformance

Start `ferrited --source file://fixtures/...`, connect a test client over
WS, assert frames are well-formed:

- Header version = 0x01.
- `stream_id` matches what REST declared.
- `seq` is monotonic per stream, with no gaps.
- `timestamp_ns` is monotonic.
- Payload length consistent with `payload_type`.
- JSON events on stream 0 validate against their schemas.

Same binary as prod. No mock server. If the protocol doc says it, the test
proves it.

### 6. Browser E2E (Playwright)

**One E2E per flagship demod path.** Not one per panel, not one per button.
The E2E covers what a user actually does end-to-end:

- `wbfm-smoke.spec.ts` — open replay-mode ferrited, load the app, select
  the replay device, open, tune to the known FM station in the fixture,
  drag a VFO onto it, assert the AudioWorklet receives PCM matching a
  reference RMS, assert waterfall canvas has non-trivial pixel variance.
- `adsb-smoke.spec.ts` — same shape, flowgraph = ADS-B, assert the
  expected hex message appears in the decoded list.

E2Es are expensive (headed Chromium, seconds per run) so we keep the count
small and the assertions sharp.

## Environment-specific concerns

### AudioWorklet tests

`AudioWorkletProcessor.process()` is a function. **Unit-test it directly**
in Vitest with synthetic inputs — do not go through the WebAudio graph.
WebAudio's scheduling is its own test problem; our code is the function we
wrote.

```ts
import { FmAudioProcessor } from './worklet';
const p = new FmAudioProcessor();
const inputs  = [[new Float32Array(128).fill(0.1)]];
const outputs = [[new Float32Array(128)]];
p.process(inputs, outputs, {});
expect(rms(outputs[0][0])).toBeCloseTo(0.1, 3);
```

### SharedArrayBuffer ring-buffer stress test

The SAB ring between the flowgraph Worker and the AudioWorklet is the most
error-prone piece of browser plumbing we have. One dedicated test lives in
CI:

- Runs in **headless Chromium** (required for COOP/COEP + AudioWorklet).
- Producer thread writes N million samples of a known pattern (ramp, or
  counter-mod-M) at realtime rate for >10 seconds.
- Consumer reads and asserts every value.
- Zero tolerance for pattern corruption or dropped samples.

This catches memory-ordering bugs, wrap-around bugs, and cache-line false
sharing that shorter tests miss.

### Native ↔ WASM parity

Every block with a C decoder core runs both compilations against the same
fixture set. The `blocks` crate has a test feature:

```
cargo test -p ferrite-blocks
wasm-pack test --headless --chrome blocks
```

A diff in decoded output between the two is a failing test. This catches:

- FP ordering differences that blow past tolerance.
- Endian mistakes in C packing code.
- `#ifdef __wasm__` branches that quietly skip code.
- Missing libc functions on the wasm side.

## What we deliberately do not do

- **Mock the WebSocket protocol.** Mocks desync from the server. Replay
  mode is both cheaper and truer — use it.
- **Mock Soapy.** Same reason. The replay source plugs in at the same layer
  a Soapy source does; any surface downstream of that is real code.
- **Snapshot-test binary data.** Magic-number diffs on binary blobs give no
  signal. Assert properties, not bytes.
- **End-to-end test every permutation.** Block unit + fixture + property
  tests cover the combinatorial space. E2E is for the human-visible golden
  path only.

## Observability (non-test, test-adjacent)

Timing bugs that unit tests can't catch surface instantly if we just **look**.
Every block maintains counters, maintained with near-zero overhead:

| counter         | meaning                                            |
|-----------------|----------------------------------------------------|
| `samples_in`    | total input samples consumed                       |
| `samples_out`   | total output samples produced                      |
| `underruns`     | `process()` called with no input items             |
| `overruns`      | output buffer full on entry, had to drop           |
| `late_frames`   | frames whose timestamp fell behind wall clock      |
| `process_us`    | EWMA of time spent in last N `process()` calls     |

A **debug panel** in the web app renders these per block for the active
flowgraph. During development, a "channels look wrong" bug manifests as
`samples_in = N, samples_out = 0` on some block — the problem localizes in
seconds.

The same counters are available via a REST endpoint on `ferrited`
(`GET /api/debug/stats`), off by default, on with `--debug-stats`. Node
sidecar prints them on SIGUSR1.

## Fixture provenance

All `fixtures/*.iq` files have:

- A JSON sidecar with rate, center, duration, what's in it.
- An entry in `fixtures/LICENSE.md` stating origin and license.
- A one-line `fixtures/README.md` entry describing the purpose.

Fixtures checked in without provenance **fail CI** (a tiny shell check in
the workflow). This keeps licensing clean and documentation current.

## What CI runs

On every PR and merge to `main`:

1. `cargo fmt --check`, `cargo clippy -- -D warnings` (workspace).
2. `cargo test` (workspace, native).
3. `wasm-pack test --headless --chrome` for `blocks` and `packages/*`.
4. `pnpm lint`, `pnpm check` (svelte-check, strict).
5. `pnpm test` (Vitest — JS/TS unit, WASM unit).
6. Playwright E2E (matrix: one flagship per flowgraph).
7. Wire-protocol conformance (spawn `ferrited --source file://…`).
8. SAB ring-buffer stress (headless Chromium).
9. Fixture provenance check.

Target wall clock on a GitHub Actions `ubuntu-latest` runner: **under 10
minutes**. Over that and contributors stop waiting for it.

## What runs on developer machines

`pnpm test` and `cargo test` must work without any extra setup. No Soapy
install required for the default test suite — replay mode + fixtures cover
the data path. A separate `cargo test --features hardware` gates tests that
require an attached RTL-SDR (used to validate driver changes; not run in CI).
