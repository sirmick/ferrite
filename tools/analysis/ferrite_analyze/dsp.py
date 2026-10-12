"""Measurements over IQ — a numpy port of the ``ferrite-analysis`` crate
(``analysis/src/{spectrum,slice,spectrogram,envelope}.rs``), which is what
the daemon's ``spectrum`` / ``spectrogram`` / ``envelope`` routes and the
in-app AI's MCP tools run. Same definitions, same defaults, same output
shape, so a number from a capture here matches the number the AI saw;
``tests/test_conformance.py`` pins the two against shared golden output.

Keep this file in lock-step with the Rust: a change to a definition there
(window, floor percentile, peak rule, keying thresholds) must land here
too, and the golden regenerated (see ``tests/README.md``).
"""
from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

# spectrum.rs
MAX_FFT = 1 << 20
ON_RATIO = 8.0  # a bin is "on" in a frame 9 dB over the floor
KEY_SWING_DB = 18.0
KEY_WINDOW_S = 0.02
# envelope.rs
STEADY_SWING_DB = 6.0
# server/src/analysis.rs
DC_GUARD_HZ = 1_500.0


def db(p):
    """Power → dB with a floor so silence doesn't produce -inf."""
    return 10.0 * np.log10(np.maximum(np.asarray(p, dtype=np.float64), 1e-30))


def _round_half_away(x: float) -> float:
    return math.floor(x + 0.5) if x >= 0 else -math.floor(-x + 0.5)


def _next_pow2(n: int) -> int:
    return 1 if n <= 1 else 1 << (int(n) - 1).bit_length()


def _hann(n: int) -> np.ndarray:
    i = np.arange(n, dtype=np.float64)
    return 0.5 - 0.5 * np.cos(2.0 * np.pi * i / n)


def fft_size(rate: float, res_hz: float) -> int:
    want = int(max(rate / max(res_hz, 0.1), 64.0))
    return min(_next_pow2(want), MAX_FFT)


def spectrum(
    x: np.ndarray,
    rate: float,
    center: float,
    lo: float,
    hi: float,
    res_hz: float = 10.0,
    max_peaks: int = 25,
    profile_points: int = 120,
    dc_guard_hz: float = 0.0,
    keying: bool = True,
) -> dict:
    """Long-FFT spectrum of ``[lo, hi]``: floor, peaks (freq, avg/peak dB
    over the floor, duty, -6 dB width, keyed) and a max-hold profile.
    Mirrors ``spectrum::analyze``; route defaults are res 10 Hz, 25 peaks,
    120 profile points, DC guard 1.5 kHz on a raw source tap."""
    n = fft_size(rate, res_hz)
    res = rate / n
    half = rate / 2.0
    if lo < center - half or hi > center + half:
        raise ValueError(
            f"range {lo:.0f}–{hi:.0f} Hz is outside the tapped span "
            f"{center - half:.0f}–{center + half:.0f} Hz"
        )
    b_lo = int(_round_half_away((lo - center) / res))
    b_hi = int(_round_half_away((hi - center) / res))
    bins = np.arange(b_lo, b_hi + 1) % n

    win = _hann(n)
    wsum = win.sum()
    norm = 1.0 / (wsum * wsum)
    hop = n // 2
    starts = range(0, len(x) - n + 1, hop) if len(x) >= n else range(0)
    frames = []
    for s in starts:
        seg = x[s : s + n].astype(np.complex128) * win
        spec = np.fft.fft(seg)
        frames.append((np.abs(spec[bins]) ** 2) * norm)
    if not frames:
        raise ValueError(
            f"not enough IQ for one {n}-point frame ({n / rate:.2f} s at {rate:.0f} Hz) "
            "— use a longer capture or coarser res_hz"
        )
    fr = np.asarray(frames)
    freq_of = lambda i: center + (b_lo + i) * res  # noqa: E731
    out = _reduce(fr, res, freq_of, max_peaks, profile_points)
    out["frames"] = len(frames)
    out["peaks"] = [p for p in out["peaks"] if abs(p["freq_hz"] - center) > dc_guard_hz]
    if keying:
        _keying_pass(x, rate, center, out["peaks"])
    return out


def _reduce(fr: np.ndarray, res: float, freq_of, max_peaks: int, profile_points: int) -> dict:
    nf, nb = fr.shape
    avg = fr.mean(axis=0)
    mx = fr.max(axis=0)
    floor = max(float(np.sort(avg)[nb // 2]), 1e-30)
    floor_db = float(db(floor))
    avg_db = db(avg) - floor_db
    mx_db = db(mx) - floor_db
    duty = (fr > floor * ON_RATIO).sum(axis=0) / nf

    peaks = []
    cand = np.nonzero((avg_db >= 6.0) | (mx_db >= 12.0))[0]
    for i in cand:
        i = int(i)
        lo_i, hi_i = max(i - 2, 0), min(i + 2, nb - 1)
        if any(
            j != i and (mx_db[j] > mx_db[i] or (mx_db[j] == mx_db[i] and j < i))
            for j in range(lo_i, hi_i + 1)
        ):
            continue
        delta = 0.0
        if 0 < i < nb - 1:
            a, b, c = mx_db[i - 1], mx_db[i], mx_db[i + 1]
            den = a - 2.0 * b + c
            if abs(den) > 1e-6:
                delta = min(max(0.5 * (a - c) / den, -0.5), 0.5)
        edge = max(avg_db[i] - 6.0, 3.0)
        l = i
        while l > 0 and avg_db[l - 1] > edge:
            l -= 1
        r = i
        while r + 1 < nb and avg_db[r + 1] > edge:
            r += 1
        peaks.append(
            {
                "freq_hz": float(freq_of(i) + delta * res),
                "avg_db": float(avg_db[i]),
                "peak_db": float(mx_db[i]),
                "duty": float(duty[i]),
                "width_hz": float((r - l + 1) * res),
                "keyed": False,
            }
        )
    peaks.sort(key=lambda p: -p["peak_db"])
    peaks = peaks[:max_peaks]

    profile = []
    if profile_points:
        step = max(nb / profile_points, 1.0)
        s = 0.0
        while int(s) < nb:
            a = int(s)
            b = max(min(int(s + step), nb), a + 1)
            seg = mx_db[a:b]
            # Rust's max_by keeps the *last* of equal maxima.
            k = a + (len(seg) - 1 - int(np.argmax(seg[::-1])))
            profile.append([float(freq_of(k)), _round_half_away(float(mx_db[k]) * 10.0) / 10.0])
            s += step
    return {
        "res_hz": res,
        "frames": 0,
        "floor_dbfs": floor_db,
        "peaks": peaks,
        "profile": profile,
    }


def _keying_pass(x: np.ndarray, rate: float, center: float, peaks: list[dict]) -> None:
    """Per peak: mix to DC, power in 20 ms windows; keyed = p90/p10 swing
    > 18 dB with 3–95 % on-time (duty then = on-time).

    The windowed mix is one matrix product: with n = m·w + r,
    |Σ_r x[m·w+r]·e^{-jω(m·w+r)}|² = |Σ_r x[m·w+r]·e^{-jωr}|², so every
    peak's envelope is ``|X @ E|²`` for X = windows × w and E = w × peaks
    — identical to the Rust running oscillator, without a full-length
    exp per peak."""
    if not peaks:
        return
    w = max(int(rate * KEY_WINDOW_S), 16)
    nw = len(x) // w
    if nw < 10:
        return
    xw = x[: nw * w].reshape(nw, w)
    r = np.arange(w, dtype=np.float64)
    offs = np.array([p["freq_hz"] - center for p in peaks])
    # Chunk the peaks so E (w × k complex) stays small.
    for k0 in range(0, len(peaks), 16):
        e = np.exp(-2j * np.pi * np.outer(r, offs[k0 : k0 + 16]) / rate).astype(np.complex128)
        env_all = np.abs(xw.astype(np.complex128) @ e) ** 2
        for j, p in enumerate(peaks[k0 : k0 + 16]):
            es = np.sort(env_all[:, j])
            p10 = max(float(es[len(es) // 10]), 1e-30)
            p90 = max(float(es[len(es) * 9 // 10]), 1e-30)
            mid = math.sqrt(p10 * p90)
            on = float((es > mid).sum()) / len(es)
            swing = 10.0 * math.log10(p90 / p10)
            p["keyed"] = bool(swing > KEY_SWING_DB and 0.03 < on < 0.95)
            if p["keyed"]:
                p["duty"] = on


@dataclass
class Slice:
    """A decimated baseband slice: RF centre at DC, rate, samples."""

    center_hz: float
    rate_hz: float
    samples: np.ndarray


def _lowpass(n: int, cutoff: float) -> np.ndarray:
    m = n - 1
    i = np.arange(n, dtype=np.float64)
    xx = i - m / 2.0
    with np.errstate(divide="ignore", invalid="ignore"):
        sinc = np.where(xx == 0, 2.0 * cutoff, np.sin(2.0 * np.pi * cutoff * xx) / (np.pi * xx))
    w = 0.42 - 0.5 * np.cos(2.0 * np.pi * i / m) + 0.08 * np.cos(4.0 * np.pi * i / m)
    t = sinc * w
    return t / t.sum()


def _convolve(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    try:
        from scipy.signal import oaconvolve

        return oaconvolve(a, b)
    except ImportError:  # pragma: no cover - scipy is in the system set
        return np.convolve(a, b)


def slice_iq(x: np.ndarray, rate: float, center: float, lo: float, hi: float) -> Slice:
    """Mix ``[lo, hi]`` to DC and decimate by the largest integer keeping
    ~25 % guard band, through a Blackman-windowed sinc (``slice::slice``)."""
    half = rate / 2.0
    if not hi > lo or lo < center - half or hi > center + half:
        raise ValueError(
            f"range {lo:.0f}–{hi:.0f} Hz is outside the snapshot span "
            f"{center - half:.0f}–{center + half:.0f} Hz"
        )
    bw = hi - lo
    c = (lo + hi) / 2.0
    decim = max(int(math.floor(rate / (bw * 1.25))), 1)
    rate_out = rate / decim
    cutoff = min((bw / 2.0) * 1.1 / rate, 0.49)
    ntaps = min(8 * decim + 1, 16_001) | 1
    taps = _lowpass(ntaps, cutoff)
    t = np.arange(len(x), dtype=np.float64)
    mixed = x.astype(np.complex128) * np.exp(-2j * np.pi * (c - center) * t / rate)
    y = _convolve(mixed, taps)[: len(x)]
    out = y[decim - 1 :: decim]
    warm = min(ntaps // decim, len(out))
    return Slice(center_hz=c, rate_hz=rate_out, samples=out[warm:].astype(np.complex64))


def spectrogram(
    sl: Slice,
    lo: float,
    hi: float,
    res_hz: float = 10.0,
    step_ms: float = 20.0,
    max_rows: int = 400,
    max_cols: int = 400,
) -> dict:
    """Time × frequency of a slice, max-pooled, cells quantised 0–255
    between floor-3 dB and the p99.9 (``spectrogram::spectrogram``)."""
    half = sl.rate_hz / 2.0
    if lo < sl.center_hz - half or hi > sl.center_hz + half or not hi > lo:
        raise ValueError(f"range {lo:.0f}–{hi:.0f} Hz is outside the slice")
    n = min(max(_next_pow2(int(sl.rate_hz / max(res_hz, 0.5))), 16), 1 << 16)
    res = sl.rate_hz / n
    hop = max(int(step_ms / 1000.0 * sl.rate_hz), 1)
    s = sl.samples
    if len(s) < n:
        raise ValueError(
            f"slice too short for a {n}-point FFT ({n / sl.rate_hz:.2f} s) — "
            "coarser res_hz or a longer capture"
        )
    b_lo = int(_round_half_away((lo - sl.center_hz) / res))
    b_hi = int(_round_half_away((hi - sl.center_hz) / res))
    bins = np.arange(b_lo, b_hi + 1) % n
    win = _hann(n)
    norm = 1.0 / (win.sum() ** 2)
    raw = []
    for st in range(0, len(s) - n + 1, hop):
        spec = np.fft.fft(s[st : st + n].astype(np.complex128) * win)
        raw.append(db(np.abs(spec[bins]) ** 2 * norm))
    raw = np.asarray(raw)
    r0, c0 = raw.shape
    rp = -(-r0 // max(max_rows, 1))
    cp = -(-c0 // max(max_cols, 1))
    rows, cols = -(-r0 // rp), -(-c0 // cp)
    pooled = np.full((rows, cols), -np.inf)
    for ri in range(rows):
        blk = raw[ri * rp : (ri + 1) * rp]
        for ci in range(cols):
            pooled[ri, ci] = blk[:, ci * cp : (ci + 1) * cp].max()
    flat = np.sort(pooled.ravel())
    floor = float(flat[len(flat) // 2])
    top = float(flat[min(len(flat) * 999 // 1000, len(flat) - 1)])
    db_lo, db_hi = floor - 3.0, max(top, floor + 20.0)
    cells = np.round(np.clip((pooled - db_lo) / (db_hi - db_lo), 0.0, 1.0) * 255.0).astype(np.uint8)
    return {
        "f0_hz": sl.center_hz + b_lo * res + (cp - 1.0) * res / 2.0,
        "df_hz": res * cp,
        "dt_s": hop / sl.rate_hz * rp,
        "rows": rows,
        "cols": cols,
        "db_lo": db_lo,
        "db_hi": db_hi,
        "floor_dbfs": floor,
        "fft_res_hz": res,
        "cells": cells,
    }


def envelope(sl: Slice, window_ms: float = 10.0) -> dict:
    """Power per window at the slice centre → on/off segments
    (``envelope::envelope``)."""
    w = max(int(_round_half_away(window_ms / 1000.0 * sl.rate_hz)), 1)
    s = sl.samples
    if len(s) < w * 10:
        raise ValueError(f"slice too short for an envelope at {window_ms} ms windows")
    nw = len(s) // w
    p = db((np.abs(s[: nw * w].astype(np.complex128)) ** 2).reshape(nw, w).sum(axis=1) / w)
    srt = np.sort(p)
    p10, p90 = float(srt[len(srt) // 10]), float(srt[len(srt) * 9 // 10])
    swing = p90 - p10
    thr = p10 - 3.0 if swing < STEADY_SWING_DB else (p10 + p90) / 2.0
    window_s = w / sl.rate_hz
    on_mask = p > thr
    segs = []
    start = None
    for i, on in enumerate(on_mask):
        if on and start is None:
            start = i
        elif not on and start is not None:
            segs.append({"start_s": start * window_s, "end_s": i * window_s})
            start = None
    if start is not None:
        segs.append({"start_s": start * window_s, "end_s": len(p) * window_s})
    on = float(on_mask.mean())
    return {
        "freq_hz": sl.center_hz,
        "window_s": window_s,
        "power_db": [float(v) for v in p],
        "threshold_db": thr,
        "swing_db": swing,
        "keyed": bool(swing > 18.0 and 0.03 < on < 0.95),
        "duty": on,
        "segments": segs,
    }


def spectrogram_of(x, rate, center, lo, hi, **kw) -> dict:
    """The route's recipe: slice 10 % wider than asked, then spectrogram."""
    pad = (hi - lo) * 0.1
    return spectrogram(slice_iq(x, rate, center, lo - pad, hi + pad), lo, hi, **kw)


def envelope_of(x, rate, center, freq_hz, bw_hz=200.0, window_ms=10.0) -> dict:
    """The route's recipe: slice ``freq ± bw/2`` (bw clamped 20 Hz–20 kHz)."""
    bw = min(max(bw_hz, 20.0), 20_000.0)
    return envelope(slice_iq(x, rate, center, freq_hz - bw / 2, freq_hz + bw / 2), window_ms)
