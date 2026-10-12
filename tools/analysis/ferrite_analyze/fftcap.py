"""Carrier peaks in a ``LogMagU8`` FFT byte capture (was
tools/fft_peaks.py): bins whose time-averaged level sits ``sigma``
standard deviations above the mean, grouped into runs, merged within
``min_gap_hz``, reported as absolute frequencies."""
from __future__ import annotations

import numpy as np

from .captures import byte_to_dbfs


def fft_peaks(rows: np.ndarray, side: dict, sigma: float = 3.0, min_gap_hz: float = 5_000.0, max_peaks: int = 50) -> dict:
    n_frames, frame = rows.shape
    rate = float(side.get("sample_rate_hz", 0.0))
    center = float(side.get("center_freq_hz", 0.0))
    if rate <= 0:
        raise ValueError("sidecar sample_rate_hz missing/0")
    avg = rows.astype(np.float32).mean(axis=0)
    thr = float(avg.mean()) + sigma * float(avg.std())
    res = {
        "sample_rate_hz": rate,
        "center_freq_hz": center,
        "frame_size": frame,
        "frames": n_frames,
        "threshold_byte": thr,
        "peaks": [],
    }
    above = np.nonzero(avg > thr)[0]
    if above.size == 0:
        return res
    runs = []
    start = prev = int(above[0])
    for b in above[1:]:
        b = int(b)
        if b == prev + 1:
            prev = b
            continue
        runs.append((start, prev))
        start = prev = b
    runs.append((start, prev))
    bin_hz = rate / frame
    gap = max(1, int(min_gap_hz / bin_hz))
    raw = sorted(
        ((lo + int(np.argmax(avg[lo : hi + 1])), float(avg[lo + int(np.argmax(avg[lo : hi + 1]))])) for lo, hi in runs),
        key=lambda t: -t[1],
    )
    acc: list[tuple[int, float]] = []
    for b, v in raw:
        if any(abs(b - a) < gap for a, _ in acc):
            continue
        acc.append((b, v))
        if len(acc) >= max_peaks:
            break
    res["peaks"] = sorted(
        (
            {
                "freq_hz": center + (b - frame // 2) * bin_hz,
                "strength_byte": v,
                "strength_dbfs": float(byte_to_dbfs(v)),
                "bin": b,
            }
            for b, v in acc
        ),
        key=lambda p: p["freq_hz"],
    )
    return res
