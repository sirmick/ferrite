"""Conformance: ``ferrite_analyze.dsp`` (Python) against the golden the
Rust ``ferrite-analysis`` crate is pinned to (``analysis/tests/
conformance.rs``) — same fixtures from ``samples/``, same parameters.

Default: compare with the committed golden. ``FERRITE_CONFORMANCE_LIVE=1``
runs the Rust test first (``cargo``) and compares with its fresh output.

Tolerances: the Rust side is float32 (rustfft), this port float64, so
levels agree to ~0.05 dB and frequencies to a fraction of a bin. Peaks
are matched by frequency; bursty fixtures produce near-equal maxima whose
rank can swap at the truncation boundary, so the last few of a capped
list may differ.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
REPO = HERE.parents[2]

from ferrite_analyze import dsp  # noqa: E402
from ferrite_analyze.captures import open_capture  # noqa: E402

DB_TOL = 0.05


def golden() -> dict:
    if os.environ.get("FERRITE_CONFORMANCE_LIVE"):
        with tempfile.TemporaryDirectory() as td:
            out = Path(td) / "rust.json"
            subprocess.run(
                ["cargo", "test", "-q", "-p", "ferrite-analysis", "--test", "conformance"],
                cwd=REPO,
                env={**os.environ, "FERRITE_CONFORMANCE_OUT": str(out)},
                check=True,
            )
            return json.loads(out.read_text())
    return json.loads((HERE / "golden" / "conformance.json").read_text())


CASES = {c["name"]: c for c in json.loads((HERE / "conformance_cases.json").read_text())}
GOLDEN = golden()


def run(case: dict) -> dict:
    cap = open_capture(REPO / case["fixture"])
    x = cap.iq()
    if case["tool"] == "spectrum":
        return dsp.spectrum(
            x,
            cap.rate_hz,
            cap.center_hz,
            case["lo_hz"],
            case["hi_hz"],
            case["res_hz"],
            case["max_peaks"],
            case["profile_points"],
            case["dc_guard_hz"],
        )
    if case["tool"] == "envelope":
        return dsp.envelope_of(x, cap.rate_hz, cap.center_hz, case["freq_hz"], case["bw_hz"], case["window_ms"])
    if case["tool"] == "spectrogram":
        sg = dsp.spectrogram_of(
            x, cap.rate_hz, cap.center_hz, case["lo_hz"], case["hi_hz"], res_hz=case["res_hz"], step_ms=case["step_ms"]
        )
        sg["row_mean"] = sg["cells"].mean(axis=1).tolist()
        sg["col_mean"] = sg["cells"].mean(axis=0).tolist()
        return sg
    raise ValueError(case["tool"])


class Conformance(unittest.TestCase):
    def check_spectrum(self, name: str) -> None:
        want, got = GOLDEN[name], run(CASES[name])
        res = want["res_hz"]
        self.assertAlmostEqual(got["res_hz"], res, places=6)
        self.assertEqual(got["frames"], want["frames"])
        self.assertAlmostEqual(got["floor_dbfs"], want["floor_dbfs"], delta=DB_TOL)
        # Profile: same sample points, same max-hold level.
        self.assertEqual(len(got["profile"]), len(want["profile"]))
        for (fg, vg), (fw, vw) in zip(got["profile"], want["profile"]):
            self.assertAlmostEqual(fg, fw, delta=res * 0.01)
            self.assertAlmostEqual(vg, vw, delta=0.11)
        # Peaks: every golden peak above the truncation fringe has a
        # same-frequency twin with the same measurements.
        cap = len(want["peaks"])
        fringe = 3 if cap >= CASES[name]["max_peaks"] else 0
        for w in want["peaks"][: cap - fringe]:
            twins = [g for g in got["peaks"] if abs(g["freq_hz"] - w["freq_hz"]) < res * 0.05]
            self.assertTrue(twins, f"{name}: no Python peak at {w['freq_hz']:.2f} Hz")
            g = twins[0]
            self.assertAlmostEqual(g["avg_db"], w["avg_db"], delta=DB_TOL)
            self.assertAlmostEqual(g["peak_db"], w["peak_db"], delta=DB_TOL)
            self.assertAlmostEqual(g["width_hz"], w["width_hz"], delta=res * 1.01)
            self.assertEqual(g["keyed"], w["keyed"], f"{name} keyed @ {w['freq_hz']}")
            self.assertAlmostEqual(g["duty"], w["duty"], delta=0.02)

    def test_aprs_spectrum(self):
        self.check_spectrum("aprs_spectrum")

    def test_adsb_spectrum_with_dc_guard(self):
        self.check_spectrum("adsb_spectrum")
        c = CASES["adsb_spectrum"]
        center = open_capture(REPO / c["fixture"]).center_hz
        for p in run(c)["peaks"]:
            self.assertGreater(abs(p["freq_hz"] - center), c["dc_guard_hz"])

    def test_aprs_envelope(self):
        want, got = GOLDEN["aprs_envelope"], run(CASES["aprs_envelope"])
        self.assertAlmostEqual(got["window_s"], want["window_s"], places=9)
        self.assertEqual(len(got["power_db"]), len(want["power_db"]))
        diffs = [abs(a - b) for a, b in zip(got["power_db"], want["power_db"])]
        self.assertLess(max(diffs), 0.05)
        self.assertAlmostEqual(got["threshold_db"], want["threshold_db"], delta=DB_TOL)
        self.assertAlmostEqual(got["swing_db"], want["swing_db"], delta=DB_TOL)
        self.assertEqual(got["keyed"], want["keyed"])
        self.assertAlmostEqual(got["duty"], want["duty"], delta=0.005)
        self.assertEqual(len(got["segments"]), len(want["segments"]))

    def test_aprs_spectrogram(self):
        want, got = GOLDEN["aprs_spectrogram"], run(CASES["aprs_spectrogram"])
        for k in ("rows", "cols"):
            self.assertEqual(got[k], want[k], k)
        for k in ("f0_hz", "df_hz", "dt_s", "fft_res_hz"):
            self.assertAlmostEqual(got[k], want[k], delta=abs(want[k]) * 1e-9 + 1e-6, msg=k)
        for k in ("floor_dbfs", "db_lo", "db_hi"):
            self.assertAlmostEqual(got[k], want[k], delta=DB_TOL, msg=k)
        for k in ("row_mean", "col_mean"):
            d = max(abs(a - b) for a, b in zip(got[k], want[k]))
            self.assertLess(d, 0.6, k)


if __name__ == "__main__":
    unittest.main()
