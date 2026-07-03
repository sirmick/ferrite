# Audit fix plan — branch cleanup + verified defects (2026-07-01)

Source: full-codebase audit (DSP correctness / design / testability), findings
verified against source. This plan is written to be executed step by step by
an implementing agent. Follow the phases **in order** — later phases assume
the tree state produced by earlier ones.

## Status — updated 2026-07-02 (on `main`, through `8aafce8`)

Phases 0–5, 6.1, 6.4, 6.5 are **done, merged to `main`, and pushed**. Every
fix carries a regression test verified to fail without it; each phase passed
the full local CI matrix before push. The RX chain was additionally
runtime-verified live (fresh binary, MCP-driven): real ADS-B decode into the
decoder store, live reconfigure ×2 (Phase-2 path, no crash), a failed source
patch rolling back cleanly (pipeline survived), Squelch on 1.09M real samples,
and RdsDemod on 10M samples with an exact decimation count (no timeline slip).

| Phase | Status | Notes |
|---|---|---|
| 0 — branch cleanup | ✅ done | sherpa-asr split into 2 commits + merged (+2 latent CI breaks fixed); stash triaged & preserved (one genuine unapplied change: `soapy_retry` fail-fast — **Mick's call** to resurrect); trivia sweep |
| 1 — C1 stereo quadrature | ✅ done | live on-air defect; `-2·c·s` + fixed cos→sin fixtures + convention/mirror-polarity test |
| 2 — C2/C3 reconfigure rollback | ✅ done | HuskBlock take/restore + clear-reusable-on-conflict; fault-block tests |
| 3 — H1 NaN scrub | ✅ done | 6 audio blocks + per-block recovery tests |
| 4.1 — assert seg_snr_db | ✅ done | |
| 4.2 — replay smoke test | ✅ done | ADS-B IQ → DecoderStore (`server/tests/replay_smoke.rs`) |
| 4.3 — RealF32Resamp sweep + nx_cap | ✅ done | starved-dst timeline-slip fix + rate sweep |
| 5 — RDS H2 (delay) + H4 (backpressure) | ✅ done | decode now works at 200/240/250 kHz |
| 5 — RDS H3 (symbol timing) | ⏸ deferred | larger; plan marks optional |
| 6.1 — delete client-side graph derivation | ✅ done | browser-half authoritative; −228 lines |
| 6.4 — extract `plan_tune()` | ✅ done | pure fn in `source_policy` + 9 dodge-math tests |
| 6.5 — sdr-tables consolidation | ✅ done | per-driver policy + driver-arg parser into `ferrite-sdr-tables` |
| 6.2 — browser whisper removal | 🚧 blocked | needs sherpa e2e green in CI first (provisioning gap) |
| 6.3 — capture orchestration server-side | ✅ done | `POST /api/capture/*` on ferrited; MCP verbs thinned (−595 lines); antenna-inherit + sidecar-naming fixes, each with a regression test verified to fail without it |
| 6.6 — `AppError` enum | ✅ done | typed `AppError` (status by variant) kills the `msg.contains` 409 + plain-text error bodies; FrameBus poison-recovery applied to DecoderStore + BroadcastSink; poison tests verified to fail without the fix |
| 6.7 — Soapy stream trait seam | ✅ done | `RxStreamLike` trait extracts read/time_ns/deactivate so the reader's overflow/hung/recovery state machine runs against a scripted fake; fixed the spurious-`hung`-on-full-ring leak (refresh liveness on any `Ok(n>0)`), regression-tested. H5 drop-oldest + H6 gap flag deferred as own-phase follow-ups |

The detailed phase write-ups below are retained as the record of what was
built (and, for 6.2/6.3/6.6/6.7, the spec for the remaining work).

## Ground rules for the implementer

- Work on `refactor/mcp-single-layer` (or a worktree branched from it under
  `branches/` for the bigger phases, per repo convention).
- One phase = one or more focused commits. Never batch unrelated phases into
  one commit. Commit messages follow the existing `type(scope): summary` style.
- **Before every push**: run the local CI matrix and get it green —
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`,
  `cargo test --workspace --all-targets`, the wasm32 check, and in `web/`:
  `pnpm lint && pnpm check && pnpm test`. Any change under `blocks/` or
  `runtime/` also requires `pnpm wasm:build` (source `/home/mick/emsdk/emsdk_env.sh`
  first if a build says `emcc not found`).
- **Never** run `git stash drop`, `git branch -D`, `git push --force`, or
  delete files not named in this plan without asking Mick first.
- If a step's verification fails and the fix isn't obvious within the scope
  of that step, stop and report — do not widen the change to make tests pass.

---

## Phase 0 — clean up the branch and uncommitted state

State as of 2026-07-01 (verify with `git status`, `git worktree list`,
`git stash list` before starting; if it differs, stop and report):

- Main worktree on `refactor/mcp-single-layer`, in sync with its origin
  branch, **68 commits ahead of `main`**.
- Uncommitted in the main worktree: `server/src/app_state.rs` (+43/−5) — a
  complete, self-tested fix making the tune dodge deterministic
  (`resolve_offset_ratio` reads `driver=` from the args string instead of
  probing a possibly-busy device; probe kept as fallback; includes the
  `driver_arg` helper + unit test).
- Worktree `branches/sherpa-asr` on `feat/sherpa-asr`: 1 commit ahead of
  this branch (a04b098, server-side sherpa ASR) plus **14 modified + 4
  untracked files uncommitted** — this contains the browser-half endpoint
  and the sherpa browser engine, i.e. two architecture changes recorded as
  "done" that exist nowhere in git history.
- `stash@{0}`: `WIP on main: 00cb6f6 …` — old, predates this branch.

### 0.1 Commit the dodge fix (main worktree)

1. Fix a placement bug in the uncommitted diff first: the new `driver_arg`
   doc comment + function were inserted **between** `overlay_live_params`'s
   doc comment (the block ending "…keep their authored params.") and
   `fn overlay_live_params` (around `server/src/app_state.rs:1064-1091`),
   orphaning that doc. Move the entire `driver_arg` doc + function **above**
   the `overlay_live_params` doc comment so each doc sits directly on its
   own function.
2. `cargo test -p ferrite-server` (must pass, including
   `driver_arg_extracts_soapy_driver`).
3. Commit: `fix(server): deterministic tune dodge — read driver= from args, probe only as fallback`.

### 0.2 Commit and land the sherpa-asr worktree

Everything here happens inside `branches/sherpa-asr/`.

1. `git -C branches/sherpa-asr diff` and read the changes. Expected content,
   split into **two commits**:
   - **Commit A — browser-half authority**: `server/src/routes.rs` +
     `server/src/main.rs` (the `GET /api/flowgraph/browser-half` endpoint),
     `server/src/app_state.rs`, `runtime/src/env_split.rs`,
     `runtime/src/inject_voice_transcribe.rs`, `web/src/lib/api/flowgraph.ts`,
     `web/src/lib/pipeline.svelte.ts`, `web/src/routes/+page.svelte` (the
     parts that fetch the server-derived browser half instead of composing
     client-side), `web/src/lib/presets/wbfmE2E.test.ts`.
     Message: `feat(server,web): server-authoritative browser-half flowgraph endpoint`.
   - **Commit B — sherpa browser engine**: `blocks/native/sherpa/` (new),
     `web/src/lib/transcribe/sherpaEngine.ts`, `sherpaGoldenE2E.test.ts`,
     `__fixtures__/sherpaRunner.mjs`, `transcribeWorker.ts`,
     `store.svelte.ts`, `TranscriptView.svelte`, `web/package.json`,
     `.gitignore`.
     Message: `feat(transcribe): sherpa-onnx browser engine behind the whisper interface`.
   If a file's hunks straddle both concerns, use `git add -p`. If the split
   is genuinely impossible for some file, put it in Commit A and say so in
   the commit body.
2. Run the full local CI matrix **in the worktree** (both Rust and web; the
   sherpa golden e2e may skip when its model is absent — that is fine, the
   wbfm/fldigi e2e must pass).
3. Merge back (repo convention: merge to the integration branch, then remove
   the worktree):
   ```
   git checkout refactor/mcp-single-layer        # main worktree
   git merge --no-ff feat/sherpa-asr -m "Merge feat/sherpa-asr: browser-half authority + sherpa browser engine"
   git worktree remove branches/sherpa-asr
   git branch -d feat/sherpa-asr
   ```
4. Re-run the local CI matrix on the merged result (root worktree).

### 0.3 Triage the stale stash

1. `git stash show -p stash@{0} > /tmp/claude-stash-backup.patch` (keep the
   backup out of the repo).
2. For each hunk, check whether an equivalent change already exists on
   `refactor/mcp-single-layer` (it includes deleted `samples/audio/fm_98.500mhz_*`
   fixtures, `server/src/main.rs` and `web/src/lib/logs/client.ts` edits —
   likely superseded by the decoder-store and log work already merged).
3. **Do not drop the stash.** Report a short superseded/not-superseded
   verdict per file to Mick and let him decide.

### 0.4 Sync `main`

Only after 0.1–0.2 are done and local CI is green:

```
git push origin refactor/mcp-single-layer
git checkout main && git merge --no-ff refactor/mcp-single-layer
git push origin main
git checkout refactor/mcp-single-layer
```

If Mick would rather keep `main` back for now, pushing the feature branch
alone is acceptable — ask only if 68-commits-to-main feels surprising; the
branch has been the de-facto trunk for months.

### 0.5 Trivia sweep (one commit)

- Delete `clean.sh` (byte-identical duplicate of `clean`).
- Remove the `--headless` stub in `tools/ferrite-ctl/src/main.rs:45-48,418-421`
  (prints "not implemented", exits 2).
- Remove the stale `#[allow(dead_code)]` at `server/src/decoder_store.rs:33`
  and `:120` (both items are live now); fix any warnings that surface.
- Commit: `chore: delete clean.sh dup, --headless stub, stale allow(dead_code)`.

---

## Phase 1 — C1: stereo decoder quadrature fix (live on-air defect)

**Defect:** `blocks/src/stereo_decoder.rs:448` regenerates the 38 kHz
subcarrier as `cos(2θ)`, but the PLL (`mix_q = -pilot * s`) locks the NCO in
quadrature to the pilot phase ψ (θ = ψ ∓ π/2), so `cos(2θ) = −cos(2ψ)` —
90° from the broadcast-standard `sin(2ψ)` subcarrier. The coherent product
has zero DC term: L−R nulls and real stations decode as mono with
`pilot_lock = true`.

1. Change line 448:
   ```rust
   // was: let ref_38k = 2.0 * c * c - 1.0;
   let ref_38k = -2.0 * c * s;   // −sin(2θ) = +sin(2ψ) at lock: in phase with the broadcast subcarrier
   ```
   Update the comment above it (it currently derives `cos(2θ)`), and fix the
   adjacent inaccuracy: the comment says "post-step phase" but `(c, s)` are
   read **pre**-step — pre-step is correct, only the comment is wrong.
2. **Fix the fixture in the same commit** — `make_mpx`
   (`stereo_decoder.rs:594-599`) currently synthesizes pilot and 38 kHz
   carrier both as `cos(2π f t)`, which is non-compliant with the FCC/ITU
   convention and self-consistent with the bug. Change both to `sin`:
   pilot `sin(2π·19000·t)`, subcarrier `sin(2π·38000·t)` (same t origin).
   The existing tests will FAIL against the fixed decoder until this is done
   — that is expected and is the proof the fixture was wrong.
3. Add one new regression test that pins the convention independently of the
   fixture helper: build MPX with `sin` pilot / `sin` subcarrier, L = 1 kHz
   tone, R = 2 kHz tone; assert (a) channel separation > 30 dB (Goertzel
   power of the 2 kHz tone in the L output vs the R output), (b) L/R are NOT
   swapped (1 kHz dominates L, 2 kHz dominates R). Also assert the mirrored
   lock polarity: run the same test with the pilot negated (θ locks at the
   other ±π/2 branch) — separation must still hold.
4. `cargo test -p ferrite-blocks stereo` — all pass. Then `pnpm wasm:build`.
5. Commit: `fix(stereo): 38 kHz reference was in quadrature — real stations decoded as mono`.
6. If an SDR is attached, verify live: load `wbfm_stereo` on a strong local
   station and confirm stereo separation is audible / L−R energy nonzero.
   Optional, do not block the commit on hardware.

---

## Phase 2 — C2/C3: reconfigure rollback (`runtime/src/runtime.rs:1227-1290`)

**C3 (do first, it's one line):** in the exclusive-resource-conflict branch
(line 1237), all blocks are rebuilt fresh with an empty skip set — but
phase 2 still drains the old **stopped** instances of every `reusable` id
and `blocks.insert` overwrites the fresh ones; they are then flagged
`carried_over`, so `init()` skips them. Fix: make `reusable` mutable and
`reusable.clear()` inside the conflict arm (after the forced rebuild), so
phase 2 drains nothing and no block is wrongly marked `carried_over`.

**C2:** the docstring promises a failed `init()` on the new graph "leaves
this runtime exactly as it was", but `drain_blocks_by_id(&reusable)` (line
1260) removes blocks from `self` **before** `replacement.init()` (line 1281).
On init failure the drained blocks (including a live SoapySource) are dropped
with `replacement`, and `self` keeps running a half-graph.

Preferred fix shape (adapt to what the code allows — read
`drain_blocks_by_id`, `from_parts`, and the entry structure first):

```rust
if let Err(e) = replacement.init() {
    // Give the drained blocks back before surfacing the error.
    for (id, block) in replacement.drain_blocks_by_id(&reusable) {
        self.restore_block(id, block);   // reinsert into the matching entry slot
    }
    return Err(e);
}
```

`self`'s entries/schedule/specs still describe `old_doc` (nothing else was
mutated), so restoring is purely "put the Box<dyn Block> back in the entry
it was drained from". If `drain_blocks_by_id` currently removes whole
entries rather than leaving husks, either switch it to `Option<Box<dyn Block>>`
take/put semantics or capture enough entry metadata to reinsert — pick
whichever is the smaller diff.

**Tests (same commit):**
- A test block whose `init()` fails on demand (add to the runtime test
  fixtures). Reconfigure a running graph to a doc containing it → assert
  `Err`, AND the runtime still `Running`, AND a subsequent `tick()` still
  produces output from the old graph, AND the reusable block instances were
  not dropped (observable via a drop-counter test block).
- Conflict-branch test: force `is_exclusive_resource_conflict` (test block
  whose constructor returns that error once) → assert every block in the
  new graph had `init()` called (init-counter test block) and none are
  `carried_over`.

Commit: `fix(runtime): reconfigure rollback — restore drained blocks on init failure; no stale instances after conflict retry`.
Run `pnpm wasm:build` after (runtime changed).

---

## Phase 3 — H1: NaN scrub for the audio chain

One NaN sample permanently latches: Squelch power EMA (gate frozen), AGC
envelope (gain pins at max via `NaN.max(EPS)`), Deemph one-pole (NaN forever),
Spectral NR noise floor, and the AmDemod DC tracker.

Implementation: per-block state guards, not an extra chain block.
In each of:
- `blocks/src/squelch.rs:253-266`
- `blocks/src/audio_nr/agc.rs:125-131`
- `blocks/src/audio_nr/deemph.rs:53-57`
- `blocks/src/audio_nr/blanker.rs:117-122`
- `blocks/src/audio_nr/spectral.rs:234-238`
- `blocks/src/am_demod.rs:227` (DC tracker)

guard the recursive state update: treat a non-finite input sample as 0.0 for
the state update (and pass 0.0 downstream for that sample), e.g.
`let x = if x.is_finite() { x } else { 0.0 };` at the top of the per-sample
loop. Do NOT add per-sample branches to blocks that don't latch.

**Tests:** one per block, same shape: feed a valid tone → inject a single
NaN (and one ±Inf) → feed the tone again → assert output returns to the
pre-NaN steady state within a bounded number of samples (e.g. squelch gate
reopens, AGC gain returns to within 1 dB of before, deemph output finite).

Commit: `fix(audio): non-finite samples no longer latch squelch/AGC/deemph/NR/AM-DC state`.
`pnpm wasm:build` after.

---

## Phase 4 — cheap test wins (each its own commit)

1. **Assert the fidelity numbers already computed**: in
   `blocks/tests/modulated_source_e2e.rs`, every test that asserts Pearson ρ
   also gets `assert!(s.seg_snr_db > <threshold>)`. Run each test once,
   print the current `seg_snr_db`, set the threshold ~6 dB below the
   observed value (round down). This closes the flat-gain blind spot (ρ is
   scale-invariant).
2. **Replay smoke test** in `server/tests/replay_smoke.rs`: clone the
   AppState harness pattern from `server/tests/record_smoke.rs`, but instead
   of SineSource, PATCH in a `ModulatedFileSource` over a real `samples/`
   fixture that has a sidecar and a decoding preset (pick one whose decoder
   writes to the decoder store — e.g. a morse/CW or DTMF fixture; check
   `samples/*.json` sidecars for `modulation` + a matching preset slug).
   Assert the decoder store receives at least one expected record within a
   generous deadline (poll-until-deadline, no fixed sleeps).
3. **RealF32Resamp sweep** in `blocks/src/real_resamp.rs` tests: loop over
   rate pairs `(2_048_000→48_000, 2_400_000→62_500, 250_000→48_000,
   240_000→48_000, 48_000→44_100)`; for each: output count within ±2 of
   `in_len * out_rate / in_rate`, in-band tone SINAD > 40 dB (Goertzel),
   out-of-band tone rejected by ≥ `stopband_db − 6`. While here, fix the
   `nx_cap` off-by-one at `real_resamp.rs:228-245` (bound can exceed
   `dst.len()`, output silently discarded while input is reported consumed)
   — add a starved-dst test: `dst.len() < expected_out` must consume only
   the matching share of input (no timeline slip).

---

## Phase 5 — RDS trio (`blocks/src/rds_demod.rs`) — one commit each

1. **H2 delete the 80-sample data delay line** (`:329-332`, buffer at
   `:233`). The pilot NCO already tracks the *filtered* pilot (80-sample
   BPF delay) and the RDS band gets its own 80-sample BPF — the extra
   explicit delay double-counts, masked only because at exactly 240 kHz the
   error is 19.0 whole carrier cycles. Test: run the existing RDS decode
   test at 200 kHz and 250 kHz sample rates (currently only 240 kHz) —
   decode must succeed at all three post-fix, and (as a demonstration) the
   250 kHz case should fail pre-fix.
2. **H4 fix the backpressure back-out** (`:478-492`). Simplest correct
   shape: don't back out at all — before `pump()`, check
   `data_dst` has room for the worst case this call can produce and shrink
   `n` up front (the pattern used in `decimator.rs`). Kills all three state
   corruptions (double-pumped sample, average-restored-into-sum, lost
   `accum_q`/`decim_phase`). Test: tick the block with a 1-slot data dst and
   assert bit output is identical to the unconstrained run.
3. **H3 symbol timing** — larger; do last, optional for this plan. Add a
   zero-crossing early/late nudge to `BitSync` (`:580-606`): on each
   detected transition in the integrated symbol stream, nudge the flywheel
   phase ±(gain × error). Test: decode with a synthetic ±50 ppm clock offset
   applied to the input (resample the fixture by 1.00005) — sync must hold
   for ≥ 60 s of samples.

---

## Phase 6 — structural (separate worktrees, per repo convention)

In priority order; each gets its own `branches/<name>` worktree and plan-of-record
commit series. Scope details are in the audit report (see memory
`project_audit_2026_07.md` and the session transcript).

1. **Delete client-side graph derivation** (unblocked by Phase 0.2):
   remove `web/src/lib/flowgraph.ts:90-194` (`composeSource`,
   `injectVoiceTranscribe`), their tests, and the compose effect in
   `+page.svelte` — the browser must consume `GET /api/flowgraph/browser-half`
   exclusively. Grep web/ for remaining callers before deleting.
2. **Browser whisper removal** (after sherpa e2e is green in CI):
   `whisperEngine.ts`, `whisper.d.ts`, whisper golden e2e, the emscripten
   `build.sh`, the model-fetch steps in `.github/workflows/web.yml:62-99`,
   the 6.8 MB wasm artifact. Keep `blocks/native/whisper` (node-side STT
   uses it) but make it feature-gated in `blocks/Cargo.toml:74` like every
   other native decoder.
3. **Capture orchestration server-side** — ✅ **done** (`630d962`). The job
   registry + both capture state machines moved into `server/src/capture.rs`
   behind `POST /api/capture/{iq,fft,audio}` + `GET /api/capture/jobs[/:id]`;
   the background tasks drive `AppState` directly (tune/start/patch/stop)
   instead of looping HTTP from ferrite-ctl. The MCP verbs are now thin
   POST/GET wrappers (`mcp.rs` −595 lines). Two behavioural fixes, each with
   a regression test verified to fail without it:
   - **antenna-inherit** (`capture_source_config`): the preset-swap capture
     reuses the live `SourceConfig` (antenna/notch/args) and overrides only
     the tuning knobs, instead of rebuilding from {freq,rate,bw,gain};
   - **sidecar-naming**: recording blocks write `<path>.json`, but ferrite-ctl
     read `<path>.<ext>.json`, so `capture_status.sidecar` was always null —
     ferrited now reads its own sidecar with the block's naming.

   Note (not a regression): the non-disruptive **live-tee** IQ/FFT path
   couldn't be endpoint-tested with the SineSource harness — a
   FileAudioSink-terminated graph with no real-time source pacing stalls
   after its initial burst under *any* live-reconfigure (a plain
   `freq_shift_hz` VFO change reproduces it), so the tee engages recording
   after the graph has already backed up. Production sources drain
   continuously, so this is a harness limitation, not a capture defect; the
   endpoint smoke test drives the wideband `Source→FileIqSink` path (which
   drains continuously) and the live tee stays covered by the channelizer's
   own `live_record_path_writes_cf32_and_sidecar` block test.
4. **`plan_tune()` extraction**: the ~230 lines of dodge/keepout/DC-guard
   math inline in `AppState::tune` (`server/src/app_state.rs:850-995`)
   become a pure function in `source_policy.rs` with unit tests.
5. **sdr-tables consolidation**: move `dc_block_default_enabled` +
   `driver_from_args` (`runtime/src/inject_dc_block.rs:120-140`) and the
   notch bands + `tune_offset_ratio_for` (`server/src/source_policy.rs:175,231`)
   into `sdr-tables`; one shared args parser (the `driver_arg` from Phase 0.1).
6. **`AppError` enum** for the server: typed status mapping (kills the
   `msg.contains("pipeline is running")` 409 at `routes.rs:623` and the
   plain-text error bodies at `:1232/:1337/:1438`); apply FrameBus's
   poison-recovery pattern to `DecoderStore` and `BroadcastSink`.
7. **Soapy stream trait seam** — ✅ **done**. Extracted the three ops the
   reader loop drives (`read` / `time_ns` / `deactivate`) behind an
   `RxStreamLike` trait, with a `SoapyRx` hardware adapter and a
   `ScriptedRx` fake; `run_reader` is generic over the trait and takes
   `hung_stall` as a parameter, so the overflow / hung / recovery /
   backpressure state machine runs deterministically with no SDR (5 tests).
   Fixed the flagged bug: liveness (`last_sample_at_ns`, the `hung` clear)
   now refreshes on any `Ok(n>0)` **before** the ring write, so a
   healthy-but-backpressured device (full ring) is no longer flagged `hung`
   and leaked on Drop; regression test verified to fail without it.
   `open/retune` were left on the concrete `SoapySource` (they aren't part
   of the reader state machine the seam targets). **Deferred as own-phase
   follow-ups** (the plan's "consider"): H5 drop-oldest ring policy (changes
   backpressure semantics — needs live validation) and H6 a `PortMeta` gap
   flag (cross-cutting through the runtime).

## Deferred / explicitly not planned

- Generic injection-pass framework, splitting `runtime.rs`, growing
  `blocks-macros` — audited and rejected; do not do these.
- H7 release-mode Work clamp, decimator cutoff-vs-factor validation,
  AudioNrStereo shared-gain AGC, FFT window-gain calibration, denormal
  flushing, RDS/stereo tap scaling — real but lower priority; pick up after
  Phase 6.1–6.2.
