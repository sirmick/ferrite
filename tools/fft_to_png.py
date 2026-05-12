#!/usr/bin/env python3
"""Render an FFT byte capture (`.bin` + `.json` sidecar) to a strip PNG.

Usage:
    python3 tools/fft_to_png.py <bin-path> [<png-path>]

The bin file is `frame_size`-byte windows of u8 dBFS-quantised FFT
bins (see `LogMagU8`'s recording output). The sidecar JSON describes
`frame_size`, `sample_rate_hz`, and (when the runtime starts plumbing
it) `center_freq_hz`. We render time-on-Y, frequency-on-X, brightness =
quantised power.

Default output is `<bin-path>.png` next to the source. The PNG is what
Ferrite's AI assistant reads to "see" the spectrum — keep the output
shape simple and labelled so future tools (and humans) can read it
without reverse-engineering.

Dependencies: numpy, matplotlib (Agg backend; no display server needed).
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: fft_to_png.py <bin-path> [<png-path>]", file=sys.stderr)
        return 2

    bin_path = Path(sys.argv[1])
    if not bin_path.exists():
        print(f"no such file: {bin_path}", file=sys.stderr)
        return 1
    png_path = Path(sys.argv[2]) if len(sys.argv) >= 3 else bin_path.with_suffix(".png")

    sidecar_path = bin_path.with_suffix(".json")
    sidecar = {}
    if sidecar_path.exists():
        try:
            sidecar = json.loads(sidecar_path.read_text())
        except json.JSONDecodeError as e:
            print(f"warning: sidecar parse failed ({e}), using defaults", file=sys.stderr)

    frame_size = int(sidecar.get("frame_size", 16384))
    sample_rate_hz = float(sidecar.get("sample_rate_hz", 0.0)) or 1.0
    center_hz = float(sidecar.get("center_freq_hz", 0.0))

    raw = np.frombuffer(bin_path.read_bytes(), dtype=np.uint8)
    n_frames = len(raw) // frame_size
    if n_frames == 0:
        print(f"no complete frames in {bin_path} (got {len(raw)} B, frame_size={frame_size})", file=sys.stderr)
        return 1
    img = raw[: n_frames * frame_size].reshape(n_frames, frame_size)

    # X axis: frequency in MHz. The bin stream is fft-shifted so DC is
    # at the centre — bin 0 corresponds to (centre - rate/2), bin
    # frame_size-1 to (centre + rate/2 - 1 bin).
    fmin_mhz = (center_hz - sample_rate_hz / 2) / 1e6
    fmax_mhz = (center_hz + sample_rate_hz / 2) / 1e6
    # Y axis: time in seconds. Frame rate ≈ sample_rate / frame_size
    # for an unthrottled FFT; if the FFT block has max_frames_per_second
    # set, this is an upper bound. Good enough for a label.
    nominal_fps = sample_rate_hz / frame_size if frame_size > 0 else 1.0
    duration_s = n_frames / max(nominal_fps, 1.0)

    # Strip height grows with frames; cap so a long capture doesn't
    # produce a 10000-pixel-tall PNG.
    height_in = min(12.0, max(2.0, n_frames / 30.0))
    fig, ax = plt.subplots(figsize=(10, height_in))
    ax.imshow(
        img,
        aspect="auto",
        origin="upper",
        cmap="viridis",
        extent=[fmin_mhz, fmax_mhz, duration_s, 0.0],
        interpolation="nearest",
    )
    ax.set_xlabel("Frequency (MHz)" if center_hz > 0 else "Bin (FFT-shifted, DC at centre)")
    ax.set_ylabel("Time (s)")
    ax.set_title(
        f"{bin_path.name}  —  {n_frames} frames × {frame_size} bins  "
        f"@ {sample_rate_hz/1e6:.3f} MS/s"
    )
    fig.tight_layout()
    fig.savefig(png_path, dpi=100)
    plt.close(fig)
    print(str(png_path))
    return 0


if __name__ == "__main__":
    sys.exit(main())
