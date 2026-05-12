#!/usr/bin/env python3
"""Find carrier-like peaks in a captured FFT byte stream.

A `LogMagU8` capture is one `frame_size`-byte FFT row per tick.
Steady carriers show up as **bright vertical lines** in the PNG —
i.e. bins whose time-averaged level sits well above the rest. This
tool finds those bins and reports them as absolute frequencies, so an
AI / operator can scan a band without having to read the PNG.

Usage:
    python3 tools/fft_peaks.py <bin-path> [opts]

Options:
    --threshold-sigma <s>   peak floor = mean + s × std. Default 3.0.
    --min-gap-khz <g>       merge peaks closer than this (kHz). Default 5.
    --max <n>               cap output at the N strongest. Default 50.
    --json                  emit JSON instead of pretty text.

Reads the `.json` sidecar alongside the bin for `frame_size`,
`sample_rate_hz`, and `center_freq_hz` (the PortMeta-propagated
absolute frequency). Bails with a clear error if any are missing or
zero — those would yield nonsense frequencies otherwise.

Output (text): one line per peak, `<freq_mhz>  <strength_dbfs>  <bin>`.
Output (JSON): `{"sample_rate_hz", "center_freq_hz", "frame_size",
"frames", "peaks": [{"freq_hz", "strength_byte", "strength_dbfs",
"bin"}, ...]}`. dBFS uses the LogMagU8 quantisation window of
[-160, 0] dBFS over [0, 255]; same convention as the PNG.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

# Same window LogMagU8 uses to quantise to u8 (blocks/src/log_mag_u8.rs).
LOGMAG_FLOOR_DBFS = -160.0
LOGMAG_CEIL_DBFS = 0.0


def byte_to_dbfs(b: float) -> float:
    return LOGMAG_FLOOR_DBFS + (b / 255.0) * (LOGMAG_CEIL_DBFS - LOGMAG_FLOOR_DBFS)


def main() -> int:
    p = argparse.ArgumentParser(
        description="Find carrier peaks in a captured FFT byte stream."
    )
    p.add_argument("bin", type=Path, help="captured `.bin` file (LogMagU8 output)")
    p.add_argument(
        "--threshold-sigma",
        type=float,
        default=3.0,
        help="peak floor = mean(avg) + s × std(avg) (default 3.0)",
    )
    p.add_argument(
        "--min-gap-khz",
        type=float,
        default=5.0,
        help="merge peaks closer than this (default 5 kHz)",
    )
    p.add_argument(
        "--max",
        type=int,
        default=50,
        help="cap output at the N strongest peaks (default 50)",
    )
    p.add_argument(
        "--json",
        dest="as_json",
        action="store_true",
        help="emit JSON instead of pretty text",
    )
    args = p.parse_args()

    if not args.bin.exists():
        print(f"no such file: {args.bin}", file=sys.stderr)
        return 1
    sidecar_path = args.bin.with_suffix(".json")
    if not sidecar_path.exists():
        print(
            f"sidecar JSON missing: {sidecar_path}\n"
            "fft_peaks needs frame_size / sample_rate_hz / center_freq_hz",
            file=sys.stderr,
        )
        return 1
    sidecar = json.loads(sidecar_path.read_text())
    frame_size = int(sidecar.get("frame_size", 0))
    rate = float(sidecar.get("sample_rate_hz", 0.0))
    center = float(sidecar.get("center_freq_hz", 0.0))
    if frame_size <= 0:
        print(f"sidecar missing / invalid frame_size: {frame_size}", file=sys.stderr)
        return 1
    if rate <= 0:
        print(
            f"sidecar missing / invalid sample_rate_hz: {rate}\n"
            "If this is 0, the runtime's PortMeta propagation regressed — flag as [REVIEW].",
            file=sys.stderr,
        )
        return 1
    if center <= 0:
        print(
            f"sidecar center_freq_hz is {center} — frequencies in output will be relative to 0 Hz.\n"
            "If you tuned somewhere non-zero, this is a PortMeta propagation regression.",
            file=sys.stderr,
        )
        # Fall through; the caller can decide if relative-to-zero is acceptable.

    raw = np.frombuffer(args.bin.read_bytes(), dtype=np.uint8)
    n_frames = len(raw) // frame_size
    if n_frames == 0:
        print(
            f"no complete frames in {args.bin} (got {len(raw)} B, frame_size={frame_size})",
            file=sys.stderr,
        )
        return 1
    img = raw[: n_frames * frame_size].reshape(n_frames, frame_size).astype(np.float32)
    avg = img.mean(axis=0)

    # Threshold: mean + s × std. Standard "above the noise floor by
    # this many sigmas" rule. Adaptive to band-by-band noise levels.
    threshold = float(avg.mean()) + args.threshold_sigma * float(avg.std())
    above = np.where(avg > threshold)[0]
    if above.size == 0:
        result = {
            "sample_rate_hz": rate,
            "center_freq_hz": center,
            "frame_size": frame_size,
            "frames": n_frames,
            "threshold_byte": threshold,
            "peaks": [],
        }
        emit(result, args.as_json)
        return 0

    # Group adjacent bins → one peak. Take the local max of each run.
    runs: list[tuple[int, int]] = []
    start = int(above[0])
    prev = start
    for b in above[1:]:
        b = int(b)
        if b == prev + 1:
            prev = b
            continue
        runs.append((start, prev))
        start = b
        prev = b
    runs.append((start, prev))

    bin_hz = rate / frame_size
    min_gap_bins = max(1, int(args.min_gap_khz * 1000.0 / bin_hz))

    # For each run, pick the peak bin. Then collapse near-neighbours
    # into single entries using min_gap_bins (so a wide carrier doesn't
    # report as N adjacent "peaks").
    raw_peaks: list[tuple[int, float]] = []
    for lo, hi in runs:
        seg = avg[lo : hi + 1]
        local_max = int(np.argmax(seg)) + lo
        raw_peaks.append((local_max, float(avg[local_max])))

    raw_peaks.sort(key=lambda t: -t[1])  # strongest first
    accepted: list[tuple[int, float]] = []
    for b, v in raw_peaks:
        if any(abs(b - ab) < min_gap_bins for ab, _ in accepted):
            continue
        accepted.append((b, v))
        if len(accepted) >= args.max:
            break

    peaks = []
    for b, v in accepted:
        # FFT-shifted layout: bin 0 ↔ (centre - rate/2), bin
        # frame_size-1 ↔ (centre + rate/2 - 1bin).
        offset_hz = (b - frame_size // 2) * bin_hz
        freq_hz = center + offset_hz
        peaks.append(
            {
                "freq_hz": float(freq_hz),
                "strength_byte": float(v),
                "strength_dbfs": byte_to_dbfs(v),
                "bin": int(b),
            }
        )

    # Sort by frequency for stable, scan-friendly output.
    peaks.sort(key=lambda p: p["freq_hz"])

    result = {
        "sample_rate_hz": rate,
        "center_freq_hz": center,
        "frame_size": frame_size,
        "frames": n_frames,
        "threshold_byte": threshold,
        "peaks": peaks,
    }
    emit(result, args.as_json)
    return 0


def emit(result: dict, as_json: bool) -> None:
    if as_json:
        json.dump(result, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return
    print(
        f"frames={result['frames']}  bins/frame={result['frame_size']}  "
        f"rate={result['sample_rate_hz']/1e6:.3f} MS/s  "
        f"centre={result['center_freq_hz']/1e6:.4f} MHz  "
        f"threshold={result['threshold_byte']:.1f} byte"
    )
    peaks = result["peaks"]
    if not peaks:
        print("  (no peaks above threshold)")
        return
    print(f"  {len(peaks)} peak(s):")
    for p in peaks:
        print(
            f"    {p['freq_hz']/1e6:>10.4f} MHz   "
            f"{p['strength_dbfs']:>6.1f} dBFS   bin={p['bin']}"
        )


if __name__ == "__main__":
    sys.exit(main())
