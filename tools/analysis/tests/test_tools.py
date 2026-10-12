"""Readers, compare / ladder / report, FFT-capture tools, sidecar
backfill and the CLI — on synthetic captures written to a temp dir."""
from __future__ import annotations

import io
import json
import subprocess
import sys
import tempfile
import unittest
import wave
from contextlib import redirect_stdout
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

from ferrite_analyze import campaign, captures, cli, fftcap  # noqa: E402

RATE = 96_000.0
CENTER = 7_100_000.0


def synth(noise: float, tones: list[tuple[float, float]], secs: float = 1.5, seed: int = 1) -> np.ndarray:
    """Complex noise (rms ``noise``) + steady tones (offset Hz, amplitude)."""
    rng = np.random.default_rng(seed)
    n = int(RATE * secs)
    t = np.arange(n) / RATE
    x = (rng.standard_normal(n) + 1j * rng.standard_normal(n)) * noise / np.sqrt(2)
    for off, amp in tones:
        x = x + amp * np.exp(2j * np.pi * off * t)
    return x.astype(np.complex64)


def write_cf32(path: Path, x: np.ndarray, **tags) -> Path:
    inter = np.empty(2 * len(x), dtype="<f4")
    inter[0::2], inter[1::2] = x.real, x.imag
    inter.tofile(path)
    side = {"kind": "iq", "format": "cf32", "sample_rate_hz": RATE, "center_freq_hz": CENTER}
    if tags:
        side["tags"] = tags
    path.with_suffix(".json").write_text(json.dumps(side))
    return path


class Readers(unittest.TestCase):
    def test_cf32_and_wav_iq_round_trip(self):
        with tempfile.TemporaryDirectory() as td:
            x = synth(0.01, [(1000.0, 0.1)], secs=0.1)
            p = write_cf32(Path(td) / "a.cf32", x)
            c = captures.open_capture(p)
            self.assertEqual((c.kind, c.rate_hz, c.center_hz), ("iq", RATE, CENTER))
            np.testing.assert_allclose(c.iq(), x, rtol=0, atol=0)
            # s16 stereo WAV: L = I, R = Q, ÷ 32768, rate from the header.
            w = Path(td) / "b_iq-s16.wav"
            pcm = np.empty(2 * len(x), dtype="<i2")
            pcm[0::2] = np.round(x.real * 32767)
            pcm[1::2] = np.round(x.imag * 32767)
            with wave.open(str(w), "wb") as f:
                f.setnchannels(2), f.setsampwidth(2), f.setframerate(48_000)
                f.writeframes(pcm.tobytes())
            cw = captures.open_capture(w)
            self.assertEqual((cw.kind, cw.rate_hz), ("iq", 48_000.0))
            np.testing.assert_allclose(cw.iq(), x, atol=1e-4)

    def test_sidecar_forms_and_filename_tags(self):
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "spot80m_antC_g40.cf32"
            synth(0.01, [], secs=0.05).tofile(p)
            p.with_name(p.name + ".json").write_text(json.dumps({"sample_rate_hz": RATE, "tags": {"spot": "80 m"}}))
            c = captures.open_capture(p)
            self.assertEqual(c.tags, {"gain_db": 40.0, "antenna": "C", "spot": "80 m"})

    def test_missing_rate_is_a_clear_error(self):
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "x.cf32"
            p.write_bytes(b"\0" * 64)
            with self.assertRaisesRegex(ValueError, "sample rate"):
                captures.open_capture(p)


class Campaign(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.td = tempfile.TemporaryDirectory()
        d = Path(cls.td.name)
        # Antenna B hears the 5 kHz carrier 6 dB stronger over the same
        # floor; antenna C has a 10 dB higher floor.
        cls.a = write_cf32(d / "a.cf32", synth(0.001, [(5_000.0, 0.05)], seed=2), antenna="A", gain_db=40, spot="40 m")
        cls.b = write_cf32(d / "b.cf32", synth(0.001, [(5_000.0, 0.1)], seed=3), antenna="B", gain_db=40, spot="40 m")
        cls.c = write_cf32(d / "c.cf32", synth(0.00316, [(5_000.0, 0.1)], seed=4), antenna="C", gain_db=40, spot="40 m")
        # Ladder on B: receiver noise fixed at 0.001; antenna noise
        # (0.003 at gain 40) and the carrier scale with gain; at 60 the
        # front end overloads (noise ×10 on top).
        lad = d / "ladder"
        lad.mkdir()
        for g in (20, 30, 40, 50, 60):
            scale = 10 ** ((g - 40) / 20)
            ant = 0.003 * scale * (10 if g == 60 else 1)
            noise = float(np.hypot(0.001, ant))
            write_cf32(lad / f"b_g{g}.cf32", synth(noise, [(5_000.0, 0.1 * scale)], seed=g), antenna="B", spot="40 m")
        cls.lad = lad

    @classmethod
    def tearDownClass(cls):
        cls.td.cleanup()

    def test_compare_reports_floor_and_level_deltas(self):
        r = campaign.compare(captures.open_capture(self.a), captures.open_capture(self.b))
        self.assertAlmostEqual(r["floor_delta_db"], 0.0, delta=1.0)
        row = min(r["carriers"], key=lambda c: abs(c["freq_hz"] - (CENTER + 5_000)))
        self.assertAlmostEqual(row["d_abs_db"], 6.0, delta=1.0)
        rc = campaign.compare(captures.open_capture(self.a), captures.open_capture(self.c))
        self.assertAlmostEqual(rc["floor_delta_db"], 10.0, delta=1.0)
        md = campaign.compare_markdown(r, "A", "B")
        self.assertIn("| floor |", md)

    def test_ladder_verdicts_and_recommendation(self):
        caps = campaign.captures_from(self.lad)
        r = campaign.ladder(caps)
        gains = [s["gain_db"] for s in r["steps"]]
        self.assertEqual(gains, [20, 30, 40, 50, 60])
        v = {s["gain_db"]: s["verdict"] for s in r["steps"]}
        self.assertEqual(v[30], "receiver-noise-limited")  # floor flat, SNR up
        self.assertEqual(v[50], "antenna-noise-limited")  # floor follows gain
        self.assertEqual(v[60], "overload")
        self.assertIn(r["recommended_gain_db"], (40.0, 50.0))
        self.assertIn("| gain |", campaign.ladder_markdown(r))

    def test_report_picks_the_best_antenna_and_includes_the_ladder(self):
        md = campaign.report(campaign.captures_from(Path(self.td.name)))
        self.assertIn("## 40 m", md)
        self.assertIn("best: **B**", md)
        self.assertIn("Gain ladder, antenna B", md)

    def test_manifest_tags_override(self):
        man = Path(self.td.name) / "manifest.json"
        man.write_text(json.dumps([{"path": "a.cf32", "antenna": "whip", "spot": "S1"}]))
        (c,) = campaign.load_manifest(man)
        self.assertEqual((c.tags["antenna"], c.tags["spot"], c.tags["gain_db"]), ("whip", "S1", 40))


class FftCapture(unittest.TestCase):
    def test_peaks_in_a_logmag_capture(self):
        rows = np.full((20, 1024), 40, dtype=np.uint8)
        rows[:, 700] = 200
        side = {"frame_size": 1024, "sample_rate_hz": 1_024_000.0, "center_freq_hz": 100e6}
        r = fftcap.fft_peaks(rows, side)
        self.assertEqual(len(r["peaks"]), 1)
        self.assertAlmostEqual(r["peaks"][0]["freq_hz"], 100e6 + (700 - 512) * 1000.0)
        self.assertAlmostEqual(r["peaks"][0]["strength_dbfs"], -160 + 200 / 255 * 160)


class SidecarsAndCli(unittest.TestCase):
    def test_backfill_writes_missing_and_keeps_existing(self):
        from ferrite_analyze.sidecars import backfill

        with tempfile.TemporaryDirectory() as td:
            d = Path(td)
            with wave.open(str(d / "POCSAG_1200.wav"), "wb") as f:
                f.setnchannels(1), f.setsampwidth(2), f.setframerate(22050)
                f.writeframes(b"\0\0" * 100)
            (d / "keep.wav.json").write_text('{"name": "hand-made"}')
            with wave.open(str(d / "keep.wav"), "wb") as f:
                f.setnchannels(1), f.setsampwidth(2), f.setframerate(8000)
                f.writeframes(b"\0\0" * 10)
            with redirect_stdout(io.StringIO()):
                backfill(d)
            side = json.loads((d / "POCSAG_1200.wav.json").read_text())
            self.assertEqual((side["mode"], side["modulation"], side["sample_rate_hz"]), ("pocsag", "fm", 22050))
            self.assertEqual(json.loads((d / "keep.wav.json").read_text()), {"name": "hand-made"})

    def test_cli_json_spectrum_and_isolated_wrapper(self):
        with tempfile.TemporaryDirectory() as td:
            p = write_cf32(Path(td) / "s.cf32", synth(0.001, [(2_000.0, 0.1)], secs=1.0))
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = cli.main(["spectrum", str(p), "--lo", "7.098M", "--hi", "7.104M", "--json"])
            self.assertEqual(rc, 0)
            r = json.loads(buf.getvalue())
            self.assertAlmostEqual(r["peaks"][0]["freq_hz"], CENTER + 2_000.0, delta=1.0)
            # A planted json.py next to the capture must not be imported:
            # the wrapper runs under -I from another CWD.
            (Path(td) / "json.py").write_text("raise SystemExit('shadowed')\n")
            out = subprocess.run(
                [sys.executable, "-I", str(HERE.parent / "ferrite-analyze"), "info", "s.cf32", "--json"],
                cwd=td,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(out.stdout)["kind"], "iq")

    def test_cli_errors_cleanly(self):
        with redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(["info", "/nonexistent.cf32"]), 2)


if __name__ == "__main__":
    unittest.main()
