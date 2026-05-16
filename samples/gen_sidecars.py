#!/usr/bin/env python3
"""Backfill consistent `<file>.json` sidecars for every sample the
sample picker (`GET /api/captures`) serves.

Re-runnable and non-destructive: a sidecar that already exists is left
as-is (the hand-curated rich ones — aprs/ctcss/wspr — win), except that
a missing `name` key is patched in so the picker always has a label.

The picker only reads {name, format, sample_rate_hz, center_freq_hz,
modulation}; the rest is for humans. `modulation` is the *carrier to
replay the clip on* (am|fm|ssb), i.e. the ModulatedFileSource default.

This is the "minimal: unblock picker" pass — it does NOT rewrite preset
links or move files; preset/e2e slug unification is a later arc.
"""
from __future__ import annotations
import json
import sys
import wave
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SIGWIKI = "https://www.sigidwiki.com/wiki/"

# stem-substring → (display name, sigwiki slug, carrier modulation, mode)
# Ordered: first match wins (so "POCSAG_Sound" hits POCSAG before a
# bare fallback). Carrier = how the mode actually rides on air.
SIGNALS: list[tuple[str, tuple[str, str, str, str]]] = [
    ("Olivia_8-500", ("Olivia 8/500", "Olivia", "ssb", "olivia")),
    ("Contestia_8-500", ("Contestia 8/500", "Contestia", "ssb", "contestia")),
    ("RTTY_170Hz_45", ("RTTY 170 Hz 45.45 bd", "RTTY", "ssb", "rtty")),
    ("BPSK31", ("PSK31 (BPSK31)", "PSK31", "ssb", "psk31")),
    ("MT63-1000L", ("MT63-1000L", "MT63", "ssb", "mt63")),
    ("DominoEX_16Bd", ("DominoEX 16 Bd", "DominoEX", "ssb", "dominoex")),
    ("THROB4", ("THROB4", "THROB", "ssb", "throb")),
    ("NAVTEX_SITOR-B", ("NAVTEX / SITOR-B", "NAVTEX", "ssb", "navtex")),
    ("AFSK1200_Sound", ("APRS AFSK1200 packet", "Automatic_Packet_Reporting_System_(APRS)", "fm", "packet")),
    ("1200_variant", ("AFSK1200 variant", "Automatic_Packet_Reporting_System_(APRS)", "fm", "packet")),
    ("POCSAG_512", ("POCSAG 512 bps", "POCSAG", "fm", "pocsag")),
    ("POCSAG_1200", ("POCSAG 1200 bps", "POCSAG", "fm", "pocsag")),
    ("POCSAG_2400", ("POCSAG 2400 bps", "POCSAG", "fm", "pocsag")),
    ("POCSAG_Sound", ("POCSAG pager", "POCSAG", "fm", "pocsag")),
    ("POCSAG", ("POCSAG pager", "POCSAG", "fm", "pocsag")),
    ("FLEX_2-LVL_1600", ("FLEX 2-level 1600 bps", "FLEX", "fm", "flex")),
    ("FLEX_6400", ("FLEX 6400 bps", "FLEX", "fm", "flex")),
    ("Flex_3200", ("FLEX 3200 bps", "FLEX", "fm", "flex")),
    ("FLEX", ("FLEX pager", "FLEX", "fm", "flex")),
    ("EAS_Alert", ("EAS — tornado warning alert", "Emergency_Alert_System_(EAS)", "fm", "eas")),
    ("EAS", ("EAS / SAME header", "Emergency_Alert_System_(EAS)", "fm", "eas")),
    ("Cw_morse", ("Morse code (CW)", "Morse_Code_(CW)", "ssb", "morse")),
    ("FT8_websdr_test", ("FT8 (websdr capture)", "FT8", "ssb", "ft8")),
    ("ISM_BALDR_Weather", ("ISM 433 — BALDR weather station", "ISM_Band_device", "am", "rtl_433")),
    ("AIS_IQ_5s", ("AIS — complex baseband IQ", "Automatic_Identification_System_(AIS)", "n/a", "ais")),
    ("AM_IQ_5s", ("AM broadcast — complex baseband IQ", "Amplitude_Modulation_(AM)", "n/a", "am")),
]


def classify(name: str) -> str:
    n = name.lower()
    if name.endswith((".iq", ".cf32")) or "_iq-" in n or "-iq" in n or "iq_" in n:
        return "iq"
    return "audio"


def wav_meta(p: Path) -> tuple[int, int, int]:
    with wave.open(str(p), "rb") as w:
        return w.getframerate(), w.getnchannels(), w.getsampwidth() * 8


def lookup(stem: str):
    for key, val in SIGNALS:
        if key in stem:
            return val
    return None


def main() -> int:
    served = sorted(
        p
        for p in ROOT.rglob("*")
        if p.is_file() and p.suffix in (".wav", ".iq", ".cf32")
    )
    wrote, patched, skipped = 0, 0, 0
    for p in served:
        rel = p.relative_to(ROOT)
        sidecar = p.with_suffix(p.suffix + ".json")
        legacy = p.with_suffix(".json")  # `<stem>.json` form
        existing = sidecar if sidecar.exists() else (legacy if legacy.exists() else None)

        kind = classify(p.name)
        hit = lookup(p.stem)
        if hit:
            disp, wiki, carrier, mode = hit
        else:
            disp, wiki, carrier, mode = p.stem, None, ("am" if kind == "iq" else "ssb"), "unknown"

        if existing:
            # Non-destructive: only ensure a `name` exists for the picker.
            doc = json.loads(existing.read_text())
            if "name" not in doc:
                doc["name"] = doc.get("description", disp)
                existing.write_text(json.dumps(doc, indent=2) + "\n")
                print(f"patched name  {existing.relative_to(ROOT)}")
                patched += 1
            else:
                skipped += 1
            continue

        if p.suffix == ".wav":
            rate, ch, bits = wav_meta(p)
            fmt = f"wav-pcm-s{bits}-{'stereo' if ch == 2 else 'mono'}"
            if kind == "iq":
                fmt += "-iq"
        else:  # raw cf32 / .iq — only WSPR, which already has a sidecar
            rate, fmt = 0, "f32"

        doc = {
            "name": f"{disp} — {'IQ capture' if kind == 'iq' else 'sigidwiki audio fixture'}",
            "file": p.name,
            "kind": kind,
            "mode": mode,
            "format": fmt,
            "sample_rate_hz": rate,
            "center_freq_hz": 0,
            "modulation": carrier,
            "sigwiki_url": (SIGWIKI + wiki) if wiki else None,
            "source": {
                "origin": "sigidwiki demo clip, transcoded for Ferrite e2e + picker"
                if "sigidwiki" in str(rel)
                else "Ferrite pipeline regression fixture",
            },
            "license": {
                "name": "CC0",
                "url": "https://creativecommons.org/publicdomain/zero/1.0/",
            },
        }
        sidecar.write_text(json.dumps(doc, indent=2) + "\n")
        print(f"wrote  {sidecar.relative_to(ROOT)}  ({kind}, {rate} Hz, carrier={carrier})")
        wrote += 1

    print(f"\n{wrote} written, {patched} name-patched, {skipped} left untouched")
    return 0


if __name__ == "__main__":
    sys.exit(main())
