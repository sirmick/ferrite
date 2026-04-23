# SDR opening presets

One file per SoapySDR `driver_key` (the lowercase short name Soapy
reports — `rtlsdr`, `hackrf`, `sdrplay`, …). Each file pins a sweet-spot
sample rate for that driver. That's it — sample rate is the only
cross-driver knob we surface as a preset default.

Loaded into `optionsModel.defaultsFor` via Vite's eager glob import, so
adding a new file or editing an existing one needs no code change — the
dev server hot-reloads, prod build picks them up at compile time.

Schema (per file):

```json
{
  "driver_key": "<matches Soapy's driver_key>",
  "label": "<human label, currently informational only>",
  "sample_rate_hz": 2000000,
  "sample_rate_choices_hz": [500000, 1000000, 2000000, 4000000, 6000000, 8000000, 10000000],
  "max_sample_rate_hz": 10000000,
  "if_filter_ladder_hz": [200000, 300000, 600000, 1536000, 5000000, 6000000, 7000000, 8000000],
  "notes": "<freeform; no UI use yet>"
}
```

All fields except `driver_key` and `sample_rate_hz` are optional:

- `sample_rate_choices_hz` — short curated list for the **main UI
  dropdown next to the tuning display**. The advanced panel still binds
  to the full device probe. Entries outside the advertised capability
  ranges are silently dropped (so a stale preset can't offer an
  unreachable rate). Omit to fall back to the full probe list.
- `max_sample_rate_hz` — practical upper bound, tighter than the
  device's advertised max. Used where the driver's advertised ceiling
  is unreachable in practice (SDRplay advertises 10.66 MS/s but
  `activateStream()` fails above 10 MS/s). Clamps both the quick list
  and the advanced panel.
- `hidden_settings` — `getSettingInfo` keys to suppress in the
  advanced panel. Used when a driver surfaces the same underlying knob
  twice (e.g. SDRplay's `rfgain_sel` duplicates the `RFGR` gain
  element). Keeps "one control per capability" without device-specific
  code in the UI.
- `if_filter_ladder_hz` — for drivers whose IF filter behaviour needs
  an explicit pick. When present the UI chooses the largest ladder
  entry ≤ `sample_rate_hz` and forwards it as `bandwidth_hz`; when
  absent no `set_bandwidth` call is made and the driver default
  stands. Cases today:
  - `sdrplay` — ladder present. Driver default is 200 kHz (brick-walls
    anything wider); a filter wider than Fs makes the driver silently
    upclock Fs. Deriving from the ladder avoids both.
  - `hackrf` — ladder omitted. Driver auto-selects ~0.75·Fs, which is
    correct.
  - `rtlsdr` — ladder omitted. R820T has no real IF filter.
