# SDR opening presets

One file per SoapySDR `driver_key` (the lowercase short name Soapy
reports — `rtlsdr`, `hackrf`, `sdrplay`, …). Each file pins a sweet-spot
sample rate and (optionally) an analog filter bandwidth that the
device's heuristic defaults would get wrong.

Loaded into `optionsModel.defaultsFor` via Vite's eager glob import, so
adding a new file or editing an existing one needs no code change — the
dev server hot-reloads, prod build picks them up at compile time.

Schema (per file):

```json
{
  "driver_key": "<matches Soapy's driver_key>",
  "label": "<human label, currently informational only>",
  "sample_rate_hz": 2000000,
  "bandwidth_hz": 5000000,
  "notes": "<freeform; no UI use yet>"
}
```

`bandwidth_hz` is optional — omit it to let the rate-driven heuristic
pick one (smallest filter ≥ 0.8 × sample rate).
