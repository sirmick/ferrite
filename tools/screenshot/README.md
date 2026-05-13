# ferrite-screenshot

Reproducible Ferrite UI screenshots + Playwright e2e harness.

Spawns its own `ferrited` on a random loopback port, points
`FERRITE_STATIC_ROOT` at the built SvelteKit bundle, drives the UI via
REST (`/api/preset`, `/api/source`, `/api/pipeline/start`) and a
headless Chromium, then captures PNGs to disk.

The default source is `SineSource` so the script works in CI with no
SDR hardware attached. Pass `--source SoapySource` (and matching params
via `--source-params`) when you want a real-radio shot.

## Prerequisites

```bash
./build.sh                       # builds ferrited + WASM artifacts
pnpm -C web build                # builds the static UI into web/build/
pnpm -C tools/screenshot install # installs Playwright + tsx
pnpm -C tools/screenshot exec playwright install chromium
```

## One-shot screenshot

```bash
# default: wbfm preset, SineSource tone at center+1kHz
pnpm -C tools/screenshot shot

# pick preset + output
pnpm -C tools/screenshot shot -- --preset wbam --output docs/images/wbam.png

# attach to an already-running ./run.sh instead of spawning
pnpm -C tools/screenshot shot -- --no-spawn --port 10001
```

## E2E tests

```bash
pnpm -C tools/screenshot test           # headless
pnpm -C tools/screenshot test:headed    # watch it drive the browser
```

Artifacts land in `tools/screenshot/test-results/` (gitignored). Failed
runs also keep traces and video — open with
`pnpm -C tools/screenshot exec playwright show-trace <trace.zip>`.

## Notes on headless Chromium

- **WASM**: works out of the box.
- **WebGL2** (waterfall): SwiftShader software renderer, ~5–10 fps in
  headless — enough for a screenshot, not enough for live use. The
  `waitForWaterfall` helper polls actual canvas pixels rather than
  trusting a fixed sleep.
- **SharedArrayBuffer** (audio ring): requires COOP/COEP, which ferrited
  already sets on every response.
- **AudioContext**: gated behind a user gesture by default; the
  `--autoplay-policy=no-user-gesture-required` launch flag bypasses
  that.
