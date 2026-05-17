# Phase 5 — Ham digital modes

**Status: SHIPPED.** The fork-in-the-road below was resolved in favour
of **vendoring fldigi** (v4.2.11 curated cores, `blocks/native/fldigi`).
RTTY, PSK31, CW, MT63, Olivia, Contestia, DominoEX, Throb and NAVTEX
ship as per-mode blocks/presets, plus **RSID auto-detect** (`FldigiAuto`
hot-swaps the inner modem on a detected burst). FT8/FT4 (`ft8_lib`) and
WSPR (vendored `wsprd`) also shipped. All decode native *and*
browser-side (fldigi via the link-vs-bridge Emscripten path; D28), with
a live fldigi text console + FT8/WSPR table/map advanced views. RDS
shipped earlier in Phase 1. The historical analysis below is kept as
the decision record; the "Rust-from-scratch" ship list did **not**
win — read it as superseded.

## Goal

Ship the audio-domain ham digital modes: PSK31 / PSK63 / PSK125, RTTY,
Olivia, Contestia, MFSK, MT63, Hellschreiber, Domino, SSTV, WEFAX,
NAVTEX, DSC. Plus RDS for broadcast FM (same audio-domain feel).

This is a long tail — ~15 modes — and upstream fldigi is the obvious
codebase but the hardest C-vendor target in the set (C++ with
`progdefaults` globals, `<iostream>`, FLTK coupling that leaks into
base class).

## Fork in the road: vendor fldigi vs re-implement in Rust

Per `research/WASM_PORT_ASSESSMENT.md`, fldigi PSK31 is ~1.4k LOC of
pure DSP + ~450 LOC of integration glue. A clean Rust PSK31 — Costas
loop + varicode + tiny filter chain, using `liquid-dsp-sys` from
Phase 2 — is a plausible 500–800 LOC Rust block.

**Recommendation:** start this phase with a time-boxed **one-mode
experiment**: port PSK31 both ways (one engineer vendors fldigi, one
writes Rust using liquid-dsp primitives). Compare:

- Time to working decoder.
- Time to first clean fixture pass.
- Code size (WASM bytes).
- Maintenance story — what happens when PSK31 has a bug?

Pick the winner; pattern the remaining ~14 modes accordingly.

This is the only place in the roadmap I'm advocating for explicit
bake-off work. It's worth it — if clean-Rust wins PSK31, it probably
wins RTTY (~300 LOC reference DSP), Olivia, MT63, etc. That changes
~10 weeks of vendor work into ~4 weeks of Rust.

## Ship list (assuming Rust-from-scratch path wins)

| Block              | Core DSP summary                                  |
|--------------------|---------------------------------------------------|
| `PskDecoder`       | Costas loop + varicode + convolutional FEC (63/125) |
| `RttyDecoder`      | FSK demod + Baudot decoder                        |
| `OliviaDecoder`    | MFSK with Walsh-Hadamard FEC                      |
| `ContestiaDecoder` | Olivia variant, different alphabet                |
| `MfskDecoder`      | MFSK-8/16/32/64 — family of related decoders      |
| `Mt63Decoder`      | OFDM-like with BCH FEC                            |
| `HellDecoder`      | Feld-Hell column-scan pixel decoder              |
| `DominoDecoder`    | Incremental-frequency-shift MFSK                  |
| `SstvDecoder`      | Image decoder — vendor `slowrx` if we go vendor path |
| `WefaxDecoder`     | FM-based image, vendor or clean-Rust              |
| `NavtexDecoder`    | SITOR-B FSK + CCIR-476 — small, worth clean-Rust  |
| `DscDecoder`       | Marine DSC FSK + ITU-R M.493                      |
| `RdsDecoder`       | 57 kHz pilot on FM broadcast — clean Rust, small  |

## Architectural reuse

- **All audio-in.** Chain is almost always
  `FmDemod | AmDemod | SsbDemod → Resample(N) → ModeDecoder`. The
  demod+resample half is already Phase 1 work.
- **Events + image outputs.** Text modes emit `events`. SSTV, WEFAX
  emit a new port type `image_events` or reuse `events` with
  image-blob payloads. Decide once; apply consistently.
- **liquid-dsp primitives.** Every text-mode decoder wants `nco_crcf`,
  `firfilt_*`, `symsync_*`, `fec_*`. Good demonstration of the
  Phase 2 substrate earning its keep.

## Risks

- **Bake-off cost.** Doing both ports is ~2 weeks of overhead before
  the rest of the phase starts. Justified only if it gives a clear
  answer. If PSK31 is a draw, default to Rust (easier to maintain
  long-term in this project).
- **Mode correctness without elmers.** Ham digital modes have subtle
  quirks (waterfall hooks, signal reports embedded in callsign
  formatting, contest modes). Need ham-community fixture libraries
  (fldigi's own test assets, WebSDR recordings) to validate.
- **SSTV color formats.** ~15 SSTV submodes (Martin, Scottie, Robot,
  PD, PasoKon, …). Plan to ship 3–4 common ones first, grow.

## Estimated effort

- Bake-off: 2 weeks.
- Text modes (assuming Rust path): ~1 week each, several in parallel
  possible — call it 6 weeks total.
- SSTV + WEFAX: 2 weeks.
- NAVTEX + DSC + RDS: 1 week each.

Total: ~12–15 weeks. Longest phase. Can ship mode-by-mode — each one
is a demo-able commit.
