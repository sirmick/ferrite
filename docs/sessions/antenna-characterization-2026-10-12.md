# Antenna + front-end characterization — 2026-10-12

Station: SDRplay RSPdx on `bigear`, served to `devel` over SoapyRemote
(`driver=remote,remote=bigear`). SF Bay Area, 00:58–01:50 UTC (evening
local). Three antenna ports:

- **A** — cheap 2 m / 70 cm flex whip
- **B** — magnetic loop (passive)
- **C** — longwire on the high-Z input

## How it was done

The in-app AI (explorer mode, Opus) did every measurement through the
`ferrite` MCP tools; a headless driver fed it one band spot per turn with
`ferrite-ctl ai-prompt --wait` (driver files in `.run/charac/`, replies in
`.run/charac/out/`). One AI session per group (HF / VHF / UHF / L-band),
17 spots, 105–190 s per turn, ~40 tool calls each. Fixed protocol per spot:

1. load the survey preset, `tune` to the centre, confirm with `status`.
2. reference: `agc=false, gain_db=40`, `source_readback`.
3. antenna pass A/B/C: `snapshot` 3 s → `signals` + `spectrum` (≤400 kHz
   window), floor + top signals.
4. gain ladder on the best antenna: AGC on, then 20 / 35 / 50 / 60, then
   `rfgain_sel` 0 and 4 at gain 50 — readback, floor, strongest SNR,
   overload judgement at every step.
5. leave the radio on the best antenna + gain.

Survey presets: `wbam` below 30 MHz, `wbfm` at 98 MHz, `nwr` at 162 MHz,
`nbfm` elsewhere — all 2.4 MS/s. **The RSPdx analog filter at 2.4 MS/s is
1.536 MHz**, so each spot really covers ±0.77 MHz, not ±1.2 MHz; the spot
list was re-centred for that part-way (40 m, 2 m, NOAA).

SNR figures below are the `spectrum` tool's peak-over-local-floor (dB) on
the same snapshot, because the `signals` list's floor is pinned (see
"Tool defects"). Single-snapshot numbers on fading HF or bursty bands carry
roughly ±3 dB; only differences larger than that are called.

## Results by spot

| # | Spot (centre) | Best | Evidence | Best setting (agc off) | AGC did |
|---|---|---|---|---|---|
| 01 | MW 0.5–2.1 MHz (1.3) | **C ≈ B** | KGO/KCBS/1170 within ±2 dB; A −55 dB (hears 1170 at 36 dB) | gain 40 (IFGR 46/RFGR 0); **50 overloads on C** (floor +33 dB), 60 phantom carriers | 27 dB (IFGR 59) — fine |
| 02 | 80/75 m (3.8) | **C** | floor +7.5 dB over B, +11 over A at equal peak-over-floor → most antenna signal; A deaf | gain 40 | 50 — ok |
| 03 | 49 m SW (6.3) | **C** | +3–5 dB carriers over B; A weak | gain 40; 50 noise-limited, 60 overload | 60 — overload |
| 03b | 40 m (7.15) | **C** | +8.5 dB on 7.208 LSB phone, +7 dB carriers; A deaf | gain 40 | 65 — overload |
| 04 | 31 m / WWV 10 (10.0) | **B** | WWV +3.6 dB over C, floor 13 dB lower, C's strong carriers smear into 1.4 MHz groups; A WWV only +9 | gain 40 (35 equal) | 60 — overload |
| 05 | 20 m / 19 m (14.2) | **C** | +9 dB FT8, +20 dB 14.330 phone, +37 dB WWV 15; band open at 01:10 UTC | gain 40 | 48 — ok |
| 06 | 15 m / 13 m (21.0) | **C** narrowly | +6 dB FT8 on list, spectrum peaks tie; C carries a 200 kHz comb B lacks | gain 40 | 65 — overload |
| 07 | CB / 10 m (27.5) | **C ≈ B** | CB ch 6 within 1.3 dB (keying noise); A −11 dB | gain 40 | max — smears |
| 08 | FM broadcast (98.0) | **B** | 98.5 MHz: B 37.4, C 33.8, A ~23 dB | **gain 25** (IFGR 59/RFGR 2); **≥35 (RFGR 0) overloads** | 23/59/4 — ok, IFGR-limited |
| 09 | Airband (128.0) | **A** | only port with a real airband AM catch (127.860); B/C just keyed carriers | **gain 20** (IFGR 59/RFGR 7) — the only clean setting; RFGR ≤4 lifts floor 35–50 dB + 127.930 intermod product | 38/48/0 — overload |
| 10 | 2 m / APRS (145.0) | **A** narrowly | repeater 6.5 dB stronger than B at the clean setting; APRS A-vs-B unresolved | **gain 20** (RFGR 7); same overload signature | overload |
| 11 | NOAA / AIS (162.2) | **B** | 162.425: B 53 vs A 36 dB; only port to catch AIS; C close on .450/.550 | gain 35 (RFGR 0 tolerated here); 20 clean fallback | 59 — ok-ish |
| 12 | 70 cm (445.0) | **A** | 444.900: A 56.8, B 55.7 (−13 dB absolute), C 47 and unusable (humps) | gain 40; SNR plateaus 35→, 50 floor +, 60 overload | 59 — ok |
| 13 | FRS/GMRS/LMR (462.6) | **B** narrowly | steady 462.738: A=B (26.5); B +4–6 dB on LMR bursts, only port with 462.500; C −18 | gain 50 (IFGR 36/RFGR 0) | 51 — same |
| 14 | 800 MHz LMR (860.0) | **B** | control channel 860.388: B 32.2, A 30.1, C 12.3 | gain 50; 55–60 for weak sites | 59 — ok |
| 15 | Pagers (929.6) | **B** narrowly | 929.6125 FLEX: B 53.9, A 52.6, C 35.8 peak | gain 50; 60 starts overloading | 53 — same |
| 16 | ADS-B (1090.0) | **A** | Mode S peaks A 28, B 26, C 19 dB | gain 50; SNR flat 35→60 (33.5→37), floor follows gain | 66 — ok, no headroom |
| 17 | GPS L1 floor (1575.4) | **A** weakly | floors A −91.2, C −92.3, B −93.8 (most external noise = best coupling) | gain 50; receiver-noise-limited below 35 | 66 |

## Antenna verdicts

- **C (longwire): every HF band except around 10 MHz**, where the loop's
  13 dB lower floor wins. C is antenna-noise-limited at gain 40 on all of
  80/40/20 m (floor 4–8 dB above B with the same or better peak-over-floor).
  It is worthless above ~200 MHz (its rated limit): 70 cm and up show a
  DC pedestal and broad humps.
- **B (loop): MW ties C, best at 10 MHz, best at FM broadcast and 162 MHz
  (NOAA/AIS, +17 dB over the whip), and within 1–2 dB of the whip from
  445 MHz to 930 MHz**, sometimes ahead. Lowest noise pickup of the three
  (fewest combs/spurs).
- **A (flex whip): airband, 2 m, 70 cm, ADS-B, L-band.** Margins over B are
  small (1–6.5 dB) except airband, where it was the only port with real
  traffic. Deaf below 30 MHz (CB −11 dB, MW −55 dB, nothing on 80/40/20 m).

The station notes' previous claim "NOAA strong on every port, use A" is
wrong by 17 dB: use B for 162 MHz.

## Gain / AGC verdicts (RSPdx, this environment)

Readback `gain_db` = (59 − IFGR) + (27 − RFGR) with IFGR the IF gain
reduction and RFGR the LNA state index (not dB — see defects).

| Range | Setting | Why |
|---|---|---|
| HF < 30 MHz | **agc off, gain 40** (IFGR 46 / RFGR 0); 35 equally clean when big broadcasters are in band | from 50 up the floor rises dB-for-dB with no SNR gain (antenna-noise-limited); 60 overloads; on MW with C, 50 already overloads |
| 88–150 MHz | **agc off, gain 20** (IFGR 59 / RFGR 7) for airband / 2 m; **25** (RFGR 2) for FM broadcast | any dial value that sets RFGR 0 (28, 35, 50, 60) overloads the front end: floor +30–50 dB, intermod products (127.930 MHz), stations vanish — on every antenna |
| 162 MHz | agc off, gain 35 (RFGR 0 tolerated) | SNR flat 35–60 |
| 400–1000 MHz | agc off, **gain 40 at 445**, **50 at 462–930** | SNR plateaus from 35–40; 60 lifts floor / overloads |
| > 1 GHz | agc off, gain 50 | receiver-noise-limited below 35, floor follows gain above |

**AGC:** unusable on HF (goes to IFGR 21–26 = gain 60–65 and overloads on
6 of 8 HF spots). Useless against the VHF overload because it only moves
IFGR, never RFGR. Acceptable above 400 MHz, where it lands at 51–66 — the
same point as manual 50 but without headroom.

**rfgain_sel / RFGR:** the `gain_db` dial sets RFGR itself, out of order
(20→7, 25→2, 28/35/50/60→0; with a stale `rfgain_sel:"4"` in settings,
40 reads back as 36 with RFGR 4). An explicit `settings {"rfgain_sel"}`
only takes effect when sent alone, costs a source restart, and the next
`gain_db` write silently overrides it. Measured attenuation of index 4 is
8–14 dB at HF, 12 dB at MW — the readback counts it as 4 dB. Practical
rule: pick the dial value that lands the RFGR you want and verify with
readback; use `rfgain_sel` only when you need an LNA state the dial won't
give (it restores ~10 dB of headroom on MW).

**Hardware notches:** policy toggled `rfnotch_ctrl` off inside 88–108 MHz
and on elsewhere; the airband/2 m overload happened with the notch on, so
the FM notch alone does not protect the LNA here.

## Local interference catalogue (identical on all ports unless noted)

| Line | Seen | Likely |
|---|---|---|
| 3.000 / 21.000 / 27.000 MHz steady | all ports | harmonic family of a 3 MHz reference |
| 27.9328 MHz steady | all ports, ignores rfgain_sel | internal / local |
| 5.832 MHz near-constant | all ports | local |
| 10.836 MHz (10.83–10.92 cluster at high gain) | all ports | known |
| 144.000 MHz | all ports | 6 × 24 MHz internal |
| 1089.600 MHz narrow | all ports | internal |
| 55.7 kHz comb around 3.1 MHz | B, C | switching supply |
| 200 kHz comb 20.2–21.6 MHz | C only | switching supply picked up by the wire |
| 6.837 MHz strong carrier | C (44 dB), B weak | unidentified |
| 7.0764 MHz steady 110 Hz carrier inside the FT8 segment | B | unidentified |
| 127.930 MHz | all, only with RFGR ≤ 4 | **overload product**, not a station |
| 97.7 MHz | — | a real FM station, not the spur the notes claimed |

## Tool defects found (Ferrite side)

Fixed during setup:

1. `SoapySource::apply_live_params` wrote `gain_db` **before** switching
   AGC off, so `{agc:false, gain_db:N}` was ignored by the driver and a
   bare `agc:false` left the hardware at AGC's last pick while the stored
   dial said otherwise (readback 60 dB vs stored 35). Now AGC first, and
   the dial is re-pushed whenever AGC flips off.
2. The explorer prompt, the sdrplay operator notes and four docs said
   `agc_enable`; the source key is `agc`.

Open (ordered by impact on the AI's ears):

3. **`signals.noise_floor_dbfs` is pinned** (−119.84 exactly on every
   antenna at every spot): the floor is p10 over *all* FFT bins, and with
   a 1.536 MHz filter at 2.4 MS/s 36 % of bins are stopband at the ADC
   floor. Consequences: SNRs scale with the gain setting, strong carriers
   merge into 1–2 MHz "signals", broad noise humps fill the list at high
   gain. Fix: estimate the floor over the passband only (the injected tap
   needs the source bandwidth, which `PortMeta` lacks) or a higher
   percentile.
4. **The LO/DC bin is the top "signal" on nearly every spot.** The
   SignalList's own DC mask was removed in favour of the injected
   `DcBlock`, but source policy disables that block for SDRplay. Either
   keep a DC mask in SignalList or enable the blocker on SDRplay.
5. `top_k` = 16 caps the list, so signal count never discriminates.
6. An unknown `src` param key (e.g. `agc_enable`) is accepted silently and
   forces a **source restart** (`overall: sourceRestart`, empty changes).
7. `settings` writes always restart the source and report empty
   `changes`; `gain_db` silently overrides `rfgain_sel`; readback adds the
   RFGR LNA *index* as dB.
8. `sdrplay.json` operator notes: `rfgain_sel` is 0–27 on the RSPdx (not
   0–9), rates are 62.5 k–10.66 M (not "0.5/1/2/4/6/8/10"), and the "VHF
   default agc on or gain 25–35" advice is wrong above RFGR 0 here.
9. Bursty bands (pagers, LMR, ADS-B, AIS) need longer `snapshot`s or
   SignalList peak-hold; 3 s gives ±3 dB timing noise.

## How the AI feature held up

Every turn finished (105–190 s), kept the fixed table shape, confirmed each
hardware change with `source_readback`, recognised the pinned floor by
itself and switched to `spectrum` for decisions, identified the 127.930
MHz line as an overload product from its gain dependence, caught the
stale `rfgain_sel` re-applied on retune, and left the radio in a sane
state after each spot. Weak points: the fixed reference gain (40)
overloaded VHF, so the A/B/C pass there was unfair until it re-checked at
gain 20 on its own initiative; it could not run a decoder-level comparison
(FT8/APRS) because survey presets carry none; `view_snapshot` 504s with no
browser tab open.

## Sample rate over SoapyRemote

Ladder 2.4 / 2.0 / 6 / 8 / 10 / 1 / 0.5 / 0.25 MS/s (filter = largest ≤
rate: 1.536 M / 5 M / 7 M / 8 M / 600 k / 300 k / 200 k) at 98 MHz on B
and at 7.1 MHz on C, each step checked with `status`, `source_readback`,
`flowdiag`, driver logs and `signals`.

- **Every rate is stable end-to-end**: readback matched the request every
  time, source throughput matched the rate (10.0 MS/s held over 10 s), zero
  overflow/ring drops or gaps, empty ring, no `O`/`U` driver warnings, no
  radio-lost events. SoapyRemote on bigear's gigabit link and this
  pipeline keep up at the RSPdx's top rate. The driver logs one harmless
  warning per restart (`StreamEndpoint resize socket buffer: set 43008
  KiB, got 208/4096 KiB`); raising `net.core.rmem_max` would clear it.
- **Max usable rate: 10 MS/s with the 8 MHz filter.** The visible span is
  always the full rate but only the filter width is real.
- **Low rates work on the link but break the presets:** the DC-dodge
  offset stays at +168 kHz regardless of rate, so at 0.5 / 0.25 MS/s the
  listen frequency falls at or beyond the filter edge, and health does not
  flag it. The channelizer decimates by integers only: wbfm's channel
  comes out at 250 k (2.0 / 1 / 0.5 / 0.25 M), 242.4 k (8 M) or 238.1 k
  (10 M) instead of 240 k, which the demod/RDS assume (up to 4 % timing
  error; wbam's 12 k channel is under 1 %). Health only reports
  "preset rate ≠ source rate".
- **Rate-specific front-end fault in the FM band (verified by hand after
  the AI flagged it):** at 97–99 MHz with RFGR 0, rates **2.2, 2.4 and
  3.0 MS/s (1.536 MHz filter) deliver a flat ≈ −58 dBFS floor and the
  97.7 MHz station is gone**, while **2.0 MS/s (same 1.536 filter) and
  6.0 MS/s (6 MHz filter) are clean** at the same gain, antenna and LO.
  RFGR 2 or 4 makes 2.4 MS/s clean again. This is the "overload" seen in
  spot 08; 2.0 MS/s is presumably the driver's low-IF mode, the broken
  rates are zero-IF with the narrow filter. The airband/2 m overload is
  **not** rate-specific: at 128 MHz with RFGR 0 the floor sits 30–40 dB
  above the RFGR-7 floor at 2.0, 2.4 and 6.0 MS/s alike, and the 127.930
  MHz product drops ≈ 45 dB for a 20 dB gain cut (intermod).
- **Floor vs rate:** HF ≈ −110 dBFS at 2–2.4 M, −99…−107 at 6–10 M,
  −115…−122 at ≤ 1 M; VHF noisier and not following 10·log10(rate), which
  is the pinned `signals` floor again — treat as rough.

**Recommendation:** keep the 2.4 MS/s survey/decoder rates (they are
preset-declared and the channelizers are exact there), keep RFGR ≥ 2 on
88–150 MHz, and reach for 6–10 MS/s when hunting across a band — the link
is not the limit.

## Settings summary for the station notes

| Band | Antenna | Setting (agc off) | Readback |
|---|---|---|---|
| MW / LW | B or C | gain 40 | IFGR 46 / RFGR 0 — 50 overloads on C |
| 3–8 MHz, 14–30 MHz | C | gain 40 | IFGR 46 / RFGR 0 |
| 9–11 MHz | B | gain 40 | IFGR 46 / RFGR 0 |
| FM broadcast | B | gain 25 | IFGR 59 / RFGR 2 |
| airband, 2 m | A | gain 20 | IFGR 59 / RFGR 7 |
| 162 MHz NOAA / AIS | B | gain 35 | IFGR 51 / RFGR 0 |
| 70 cm | A (B −1 dB) | gain 40 | IFGR 46 / RFGR 0 |
| 460–930 MHz | B (A −1…2 dB) | gain 50 | IFGR 36 / RFGR 0 |
| ADS-B, L-band | A | gain 50 | IFGR 36 / RFGR 0 |

## After changes — 02:15–02:35 UTC re-run of 7 spots

Mick's changes between the runs: shed fan **off**, common-mode **choke on
the longwire (C)**, external **FM bandstop on the whip (A)**. (He also
identified a 10G NIC with audible coil whine in the shed, running off a
car battery over optical backhaul.) Same protocol, same session shape;
replies in `.run/charac/out2/`.

| Spot | Floor delta at gain 40 (spectrum, dB) | What changed |
|---|---|---|
| 80 m | A −8.8, B −9.3, C −5.7 | 55.5 kHz comb on C down ~11 dB (37 → 26), still present; 3.000 MHz carrier unchanged |
| 40 m | A −5.9, B −5.9, C −1.8 | the strong 6.837 MHz carrier on C is **gone**; C still +15 dB on 41 m broadcasters, +3.5 dB FT8 |
| 20 m | C now 13 dB above B (was 8) | C FT8 +37 dB over floor, nothing on B in the window; WWV 15 C +5 dB SNR / +22 dB absolute |
| 15 m | C 8.4 dB above B (was 4.4) | **200 kHz comb on C gone**; 21.000 MHz carrier identical on all ports (−95 dBFS) → internal; new weak 8 kHz-spaced carriers 21.23–21.38 MHz on C |
| FM bc | A floor at RFGR 0 −57.7 | filter on A: 97.7 MHz −40…47 dB, 98.5 MHz only partly rejected (filter edge) |
| airband | A −65.9 → −75.8 (filter) | **overload threshold unchanged**: clean only at RFGR 7 (gain 20), onset at RFGR 2 (gain 25) |
| 2 m | A −60.4 → −74.2 (filter) | same: clean only at gain 20; APRS 52 dB there vs 57 dB at gain 50 with the burst peaking near −6 dBFS |

Conclusions:

- **The fan was radiating into all three antennas**: A and B, which were
  not touched, dropped 6–9 dB on 80/40 m. C dropped less because its
  floor is dominated by the wire's own pickup (the 400 kV line it runs
  parallel to). C's floor advantage over B therefore *grew* 4–5 dB with
  the fan off, yet C still wins every HF band except 9–11 MHz by signal.
- **Choke on C:** at most ~11 dB off the 55.5 kHz comb and the 6.837 MHz
  carrier; the broadband floor barely moved (−1.8 dB on 40 m). The
  remaining floor is the power line. The 200 kHz comb vanished with
  fan-off + choke (not separable in this run); the whining 10G NIC remains
  the suspect for the residual 55.5 kHz comb and the new 8 kHz carriers.
- **FM bandstop on A:** worth keeping (10–14 dB lower floor at RFGR 0,
  97.7 MHz −40 dB), but **it does not change the 118–150 MHz overload
  threshold**. The 127.930 MHz product sits at the same absolute level
  (≈ −51 dBFS) on A, B *and* C, vanishes only at RFGR 7, and rises with
  IFGR — a front-end product independent of the antenna connected, i.e.
  the RSPdx itself at LNA state 0–4 in this band. No external filter
  fixes it; gain 20 (RFGR 7) stays the rule for airband / 2 m.
- Internal lines confirmed by "identical on every port": 3.000, 21.000,
  27.000 MHz, 144.000 MHz, 1089.600 MHz.
- Tool note: after a `settings` write restarts the source, `health` says
  ok before samples flow again and the first `snapshot` fails with
  `SNAPSHOT_FAILED: src tee wrote no sidecar` (retry 2–8 s later works).
  Writing `rfgain_sel "0"` restarts the source even when RFGR is already 0.

**Hardware recommendation:** swap the 10G NIC for a fanless 1 GbE card
(10 MS/s is < 40 MB/s on the wire), keep the fan off or move it, keep the
FM filter on A and the choke on C, and if the longwire can be re-routed
away from the 400 kV line that is the only thing left that lowers C's
HF floor.

## Follow-up — tool fixes and re-check (03:15–03:45 UTC)

Branch `feat/signal-floor` fixed open defects 3, 4, 6 and 7. In the same
order, the fixes are:

- **Passband floor.** The `signals` floor is the p10 over the analog
  passband, set from the source `bandwidth_hz` and re-pushed when it
  changes.
- **LO mask.** A ±25 kHz LO mask is back in SignalList.
- **Unknown keys.** An unknown `src` key returns 400 with the valid list.
- **Settings writes.** They merge per key and skip no-ops, a hardware
  no-op included. They list `settings.<key>` in `changes`, and the reply
  says when `gain_db` overrode `rfgain_sel`.

Health now reads `starting` until samples flow, and `snapshot` waits for
them. `tools/analysis` (`ferrite-analyze`) rebuilds these tables from
captures.

The driver re-ran three spots. Every `signals` floor below is the AI's
own reading:

| Spot | Floor at gain 20 / 35 / 50 / 60 (dBFS) | LO row |
|---|---|---|
| 40 m, C | −118.0 / −102.9 / −87.8 / −77.8 | none |
| FM 98, A (FM bandstop fitted) | −117.3 / −102.9 / −88.5 / −66.5 | none |
| NOAA 162, A | −117.3 / −110.4 / −96.6 / −86.0 | none |

The AI used `signals` for floors and SNR on its own, which it couldn't do
before.

What it found:

- On FM broadcast, A with its new bandstop is clean at RFGR 0 up to
  gain 50, while B and C still overload at gain 40.
- At 162 MHz, A beat B on most NOAA channels in this run. Last night B
  won by 17 dB on 162.425, and that channel was missing on B tonight.
  Treat the NOAA verdict as unsettled until a longer capture repeats it.

Remaining items it raised:

- After a preset load and tune, `health` briefly reported the previous
  LO. The driver's frequency getter lags over SoapyRemote; the readback
  is a live query.
- A stored `rfgain_sel` is re-applied when a preset load restarts the
  source. That is intended, but surprising. A same-value `gain_db` write
  now goes through when the readback disagrees.
