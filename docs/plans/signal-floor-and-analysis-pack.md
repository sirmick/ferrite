# Next session prompt — SignalList floor/DC, src-param hygiene, analysis pack

Paste the block below into a fresh Claude Code session in `~/src/ferrite`.

---

Work in a feature worktree under `branches/` (see memory: feature worktree
workflow), merge back to main at the end, run the local CI matrix before any
push. Source `~/.cargo/env` and `. soapysdr/env.sh` before building. The
stack is normally up via `./run.sh` (vite 12000 → ferrited 12001 → ai
sidecar 10002); restart with `kill -TERM -<run.sh PGID>` then
`setsid nohup ./run.sh > .run/stack.log &`. The RSPdx is remote on bigear
(`driver=remote,remote=bigear`); live checks are fine, the current station
guide is `.ferrite-ai-state/setup.md`.

Background: `docs/sessions/antenna-characterization-2026-10-12.md` (read
"Tool defects found" and "After changes"). The in-app AI measured the whole
band with the MCP tools and had to abandon `signals` for every decision.
Fix the five defects below, then build the analysis package. Each fix gets a
test that fails on the old behaviour, and the ones that change what the AI
sees get a short live check against the remote RSPdx.

1. **SignalList noise floor is pinned.** `blocks/src/signal_list.rs` takes
   p10 over all FFT bins; with the 1.536 MHz analog filter at 2.4 MS/s, 36%
   of bins are stopband, so `noise_floor_dbfs` reads −119.84 on every
   antenna at every gain and SNRs scale with the gain setting. Make the
   floor passband-aware: add a `floor_span_hz` (or passband fraction)
   param, have the server's injected tap (`runtime/src/inject_signal_list*`,
   `server/src/app_state.rs`) set it from the source's `bandwidth_hz`
   (`PortMeta` only carries rate/centre today), and keep it live-updated on
   bandwidth/rate changes. Fall back to the central 60% of bins when the
   bandwidth is unknown. Acceptance: at 98 MHz on Antenna B the reported
   floor moves with gain 20 → 35 → 50 the way the `spectrum` floor does.
2. **LO/DC bin listed as the top signal.** The SignalList DC mask was
   removed in favour of the injected `DcBlock`
   (`runtime/src/inject_dc_block.rs`), but source policy disables that
   block on SDRplay (`dcblock enabled:false`). Restore a DC mask in
   SignalList (±25 kHz default, param) regardless of the blocker, or stop
   disabling the blocker on SDRplay — pick one and say why. Acceptance: the
   LO frequency no longer appears in `signals` at HF or VHF on the RSPdx.
3. **Unknown `src` params are accepted silently and force a source
   restart** (`param src agc_enable=false` → `overall: sourceRestart`,
   empty `changes`). Validate keys against the block schema in the
   set-block-param route (`server/src/routes.rs`, `block_schema.rs`) and
   return 400 with the key name and the valid list. Keep `settings`
   sub-keys and the record_* tee keys working.
4. **`settings` writes always restart the source and report empty
   `changes`**, even when nothing changed (`rfgain_sel "0"` with RFGR
   already 0). Diff the new settings against the stored ones, skip no-ops,
   and list settings deltas in `changes`. While there: a later `gain_db`
   write silently overrides an explicit `rfgain_sel`; surface that in the
   response (a note or a readback diff) so the AI sees it.
5. **Health reports ok before samples flow after a source restart**: the
   first `snapshot` fails with `SNAPSHOT_FAILED: src tee wrote no sidecar`
   for 2–8 s. Gate `ready`/`health.radio.state` on the stream delivering
   (samples_pushed advancing) and make the snapshot route wait up to a few
   seconds for that instead of failing.

6. **Analysis package, `tools/analysis/`** (Python, system numpy/scipy,
   `python -I` friendly, for operators/CI/Claude Code — the in-app AI stays
   shell-less and must not be pointed at it; `mcp.rs` tests assert that).
   Replace the loose `tools/fft_to_png.py`, `tools/fft_peaks.py`,
   `samples/gen_sidecars.py` with one package + CLI (`ferrite-analyze`):
   - readers for the capture formats and sidecars (`samples/README.md`,
     `blocks/src/iq_record_tap.rs`, `audio_record_tap.rs`, FFT captures);
   - `spectrum` (floor, peaks, width, duty, keyed) using the same
     definitions as `server/src/analysis*`/the `spectrum` route so numbers
     agree — add a conformance test that runs both on a fixture from
     `samples/` and compares;
   - `spectrogram` and `envelope` PNG/ASCII renderers;
   - `compare` — two captures (e.g. antenna A vs B) on the same window →
     floor/peak delta table;
   - `ladder` — a directory of captures tagged by gain → the gain-ladder
     table used in the characterization report;
   - `report` — markdown tables straight from captures, so the next
     characterization is reproducible without an Opus session.
   Update README/docs references to the old scripts.

Order: 1, 2 (what the AI sees), then 3–5 (what costs it retries), then 6.
Commit per item with the attribution line in use for this repo. Finish by
re-running three spots with `.run/charac/run2.sh` (delete the spot's
`out/<id>.md` first) and confirm the AI now reports a floor that tracks
gain and no LO row.
