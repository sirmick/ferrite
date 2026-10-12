"""PNG renderers (matplotlib, Agg — no display needed). Imported lazily
so the text/JSON paths work on a box without matplotlib."""
from __future__ import annotations

from pathlib import Path

import numpy as np

_PANEL_BG = "#0b1020"  # the UI waterfall's panel colour


def _plt():
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    plt.style.use("dark_background")
    return plt


def spectrogram_png(sg: dict, out: Path, title: str = "") -> Path:
    plt = _plt()
    rows, cols = sg["rows"], sg["cols"]
    f0 = sg["f0_hz"] / 1e6
    f1 = (sg["f0_hz"] + sg["df_hz"] * (cols - 1)) / 1e6
    fig, ax = plt.subplots(figsize=(10, min(12.0, max(3.0, rows / 40.0))))
    fig.patch.set_facecolor(_PANEL_BG)
    ax.set_facecolor(_PANEL_BG)
    ax.imshow(
        sg["cells"],
        aspect="auto",
        origin="upper",
        cmap="viridis",
        extent=[f0, f1, sg["dt_s"] * rows, 0.0],
        interpolation="nearest",
    )
    ax.set_xlabel("Frequency (MHz)")
    ax.set_ylabel("Time (s)")
    ax.set_title(
        title
        or f"spectrogram  {sg['fft_res_hz']:.1f} Hz FFT, floor {sg['floor_dbfs']:.1f} dBFS, "
        f"{sg['db_lo']:.0f}…{sg['db_hi']:.0f} dB"
    )
    fig.tight_layout()
    fig.savefig(out, dpi=100, facecolor=_PANEL_BG)
    plt.close(fig)
    return Path(out)


def envelope_png(e: dict, out: Path, title: str = "") -> Path:
    plt = _plt()
    p = np.asarray(e["power_db"])
    t = np.arange(len(p)) * e["window_s"]
    fig, ax = plt.subplots(figsize=(10, 3))
    fig.patch.set_facecolor(_PANEL_BG)
    ax.set_facecolor(_PANEL_BG)
    ax.plot(t, p, lw=0.8)
    ax.axhline(e["threshold_db"], color="#f59e0b", lw=0.8, ls="--")
    for s in e["segments"]:
        ax.axvspan(s["start_s"], s["end_s"], color="#22c55e", alpha=0.15)
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("dBFS")
    ax.set_title(
        title
        or f"envelope @ {e['freq_hz'] / 1e6:.6f} MHz  swing {e['swing_db']:.1f} dB, "
        f"duty {e['duty']:.2f}{', keyed' if e['keyed'] else ''}"
    )
    fig.tight_layout()
    fig.savefig(out, dpi=100, facecolor=_PANEL_BG)
    plt.close(fig)
    return Path(out)


def fft_strip_png(rows: np.ndarray, side: dict, out: Path, name: str = "") -> Path:
    """A ``LogMagU8`` capture as a time-down strip (was tools/fft_to_png.py)."""
    plt = _plt()
    n_frames, frame = rows.shape
    rate = float(side.get("sample_rate_hz", 0.0))
    center = float(side.get("center_freq_hz", 0.0))
    if rate <= 0:
        raise ValueError("sidecar sample_rate_hz missing/0 — the axis would be nonsense")
    if "capture_duration_s" in side:
        dur = float(side["capture_duration_s"])
    else:  # unthrottled estimate — usually too short for real captures
        dur = n_frames / max(rate / frame, 1.0)
    fig, ax = plt.subplots(figsize=(10, min(12.0, max(2.0, n_frames / 30.0))))
    fig.patch.set_facecolor(_PANEL_BG)
    ax.set_facecolor(_PANEL_BG)
    ax.imshow(
        rows,
        aspect="auto",
        origin="upper",
        cmap="viridis",
        extent=[(center - rate / 2) / 1e6, (center + rate / 2) / 1e6, dur, 0.0],
        interpolation="nearest",
    )
    ax.set_xlabel("Frequency (MHz)" if center > 0 else "Bin (FFT-shifted, DC at centre)")
    ax.set_ylabel("Time (s)")
    where = f"centred at {center / 1e6:.4f} MHz" if center > 0 else "(centre unknown)"
    ax.set_title(f"{name}  —  {n_frames} frames × {frame} bins @ {rate / 1e6:.3f} MS/s  {where}")
    fig.tight_layout()
    fig.savefig(out, dpi=100, facecolor=_PANEL_BG)
    plt.close(fig)
    return Path(out)
