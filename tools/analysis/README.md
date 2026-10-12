# tools/analysis — `ferrite-analyze`

Offline analysis of Ferrite captures, for operators, CI and Claude Code
sessions with a shell. One package and one CLI replace the old loose
scripts (`tools/fft_to_png.py`, `tools/fft_peaks.py`,
`samples/gen_sidecars.py`).

**Not for the in-app AI.** The in-app assistant is shell-less by design
and gets the same measurements live through the daemon's MCP tools
(`spectrum`, `spectrogram`, `envelope`). Don't mention this tool in MCP
tool descriptions or prompts; `tools/ferrite-ctl/src/mcp.rs` tests
enforce that.

## Run

```sh
tools/analysis/ferrite-analyze <command> …          # or
python3 -I tools/analysis/ferrite-analyze <command> …
```

Needs the system `numpy`, plus `scipy` (fast slicing) and `matplotlib`
(PNG output only). The wrapper puts the package on `sys.path` from its own
location, never the working directory, so it is safe under `python3 -I`
next to untrusted captures. Every command takes `--json`.

| Command | What it does |
|---|---|
| `info <capture>` | Kind, rate, centre, length, tags |
| `spectrum <capture> [--lo --hi --res]` | Floor, peaks (Hz-accurate), width, duty, keyed |
| `spectrogram <capture> <lo> <hi> [--png]` | Time × frequency, ASCII or PNG |
| `envelope <capture> <freq> [--png]` | On/off timeline at one frequency |
| `compare <a> <b>` | Two captures on one window: floor and per-carrier deltas |
| `ladder <dir or manifest>` | Captures tagged by gain: gain-ladder table and recommendation |
| `report <dir or manifest> [-o out.md]` | Markdown characterization report |
| `fft-peaks <capture.bin>` | Carrier peaks in a `LogMagU8` FFT byte capture |
| `fft-png <capture.bin> [out.png]` | That capture as a strip PNG |
| `sidecars [root]` | Backfill missing sample sidecars (default `samples/`) |

## Same numbers as the daemon

`ferrite_analyze/dsp.py` is a numpy port of the `ferrite-analysis` crate
(`analysis/src/`), which the daemon's `spectrum`, `spectrogram` and
`envelope` routes run. Same window, floor, peak rule, keying thresholds,
defaults and output shape. A capture analysed here reads the same as the
AI's live measurement. `tests/test_conformance.py` and
`analysis/tests/conformance.rs` pin both against one golden file, built
from `samples/` fixtures. See `tests/README.md`.

`spectrum` defaults follow the route: 10 Hz bins, 25 peaks, and a 1.5 kHz
DC guard on raw source taps (`wideband: true` in the sidecar).

The `compare`, `ladder` and `report` commands use different defaults:

- 50 Hz bins over the central 60 % of the span, which is inside the
  analog passband. `--lo`/`--hi`, or `lo_hz`/`hi_hz` tags, override it.
- Peaks within 5 kHz count as one carrier (`--merge-hz`), so an NBFM
  station is one row.
- Antennas whose mean SNR on shared carriers is within 1 dB are reported
  as a tie.

## Captures and tags

Reads cf32 (`.cf32`, `.iq`, `.f32`), 16-bit stereo IQ WAV (L = I,
R = Q), 16-bit audio WAV and `LogMagU8` `.bin` rows. Each needs a JSON
sidecar, either `<file>.json` or `<stem>.json` (the tees write the
latter), carrying `sample_rate_hz` and `center_freq_hz`.

`compare`, `ladder` and `report` group captures by `antenna`, `gain_db`
and `spot`. Those come from, in order of precedence:

1. a manifest: `[{"path": "b_g40.cf32", "antenna": "B", "gain_db": 40, "spot": "40 m", "lo_hz": 7.0e6, "hi_hz": 7.3e6}, …]`;
2. a `tags` object in the sidecar;
3. the filename: `…_antB_…` → antenna `B`, `…_g40_…` or `…gain40…` → gain 40.

A characterization run then looks like this: snapshot each antenna and
gain with the daemon's tee (`record_path` on `src`), name the files
`<spot>_ant<X>_g<gain>.cf32`, and run
`ferrite-analyze report <dir> -o report.md`.

Gain-ladder verdicts per step:

- **overload**: the floor rose more than the gain step plus 3 dB, or the
  tracked carrier's SNR fell more than 3 dB.
- **antenna-noise-limited**: the floor followed the gain (at least 70 %)
  and SNR stayed flat (within 1.5 dB).
- **receiver-noise-limited**: the floor rose less than half the step and
  SNR rose.
- **mixed**: anything else.

The recommended gain is the lowest step within 1 dB of the best SNR
before the first overload.

## Tests

```sh
python3 -I tools/analysis/tests/test_conformance.py
python3 -I tools/analysis/tests/test_tools.py
cargo test -p ferrite-analysis --test conformance
```
