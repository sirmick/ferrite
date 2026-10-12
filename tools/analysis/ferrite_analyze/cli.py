"""``ferrite-analyze`` — offline analysis of Ferrite captures.

For operators, CI and Claude Code sessions with a shell. The in-app AI
is shell-less by design and must not be pointed at this tool: it has the
same measurements live through the daemon's MCP tools (``spectrum``,
``spectrogram``, ``envelope``), which use the definitions this package
mirrors.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

from . import __version__, campaign, dsp, fftcap, text
from .captures import load_fft_u8, open_capture


def _freq(s: str) -> float:
    """``98.5M`` / ``915k`` / ``7150000`` → Hz."""
    s = s.strip()
    mult = {"k": 1e3, "K": 1e3, "m": 1e6, "M": 1e6, "g": 1e9, "G": 1e9}.get(s[-1:], None)
    return float(s[:-1]) * mult if mult else float(s)


def _json_default(o):
    if isinstance(o, np.ndarray):
        return o.tolist()
    if isinstance(o, (np.floating, np.integer)):
        return o.item()
    raise TypeError(type(o))


def _emit(obj, as_json: bool, text_out: str) -> None:
    if as_json:
        json.dump(obj, sys.stdout, default=_json_default)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(text_out if text_out.endswith("\n") else text_out + "\n")


def _window(args, caps) -> campaign.Window:
    merge = getattr(args, "merge_hz", campaign.MERGE_HZ)
    if args.lo is not None and args.hi is not None:
        return campaign.Window(_freq(args.lo), _freq(args.hi), args.res, merge)
    return campaign.default_window(caps, args.res, merge)


def cmd_info(a) -> int:
    c = open_capture(a.capture)
    n = None
    if c.kind == "iq":
        n = len(c.iq())
    elif c.kind == "audio":
        n = len(c.audio())
    info = {
        "path": str(c.path),
        "kind": c.kind,
        "sample_rate_hz": c.rate_hz,
        "center_freq_hz": c.center_hz,
        "samples": n,
        "duration_s": (n / c.rate_hz) if n else None,
        "tags": c.tags,
        "sidecar": c.sidecar,
    }
    lines = [
        f"{c.path.name}: {c.kind}, {c.rate_hz:.0f} S/s, centre {c.center_hz / 1e6:.6f} MHz",
        f"  {n} samples ({info['duration_s']:.2f} s)" if n else "",
        f"  tags: {c.tags}" if c.tags else "",
    ]
    _emit(info, a.json, "\n".join(x for x in lines if x))
    return 0


def cmd_spectrum(a) -> int:
    c = open_capture(a.capture)
    w = _window(a, [c])
    guard = a.dc_guard if a.dc_guard is not None else campaign.dc_guard_for(c)
    r = dsp.spectrum(c.iq(), c.rate_hz, c.center_hz, w.lo, w.hi, w.res_hz, a.peaks, a.profile, guard)
    peaks = [p for p in r["peaks"] if p["keyed"]] if a.keyed else r["peaks"]
    rows = [
        [f"{p['freq_hz'] / 1e6:.6f}", p["avg_db"], p["peak_db"], p["width_hz"], round(p["duty"], 2), "yes" if p["keyed"] else ""]
        for p in peaks
    ]
    out = (
        f"{text.mhz(w.lo)}–{text.mhz(w.hi)} MHz  res {r['res_hz']:.2f} Hz  frames {r['frames']}  "
        f"floor {r['floor_dbfs']:.1f} dBFS  dc_guard {guard:.0f} Hz\n"
        + (text.sparkline(r) + "\n" if r["profile"] else "")
        + text.table(["MHz", "avg dB", "peak dB", "width Hz", "duty", "keyed"], rows, "lrrrrl")
    )
    _emit({**r, "peaks": peaks, "dc_guard_hz": guard}, a.json, out)
    return 0


def cmd_spectrogram(a) -> int:
    c = open_capture(a.capture)
    lo, hi = _freq(a.lo), _freq(a.hi)
    sg = dsp.spectrogram_of(c.iq(), c.rate_hz, c.center_hz, lo, hi, res_hz=a.res, step_ms=a.step_ms)
    if a.png:
        from .render import spectrogram_png

        spectrogram_png(sg, Path(a.png), title=f"{c.path.name}  {text.mhz(lo)}–{text.mhz(hi)} MHz")
    meta = {k: v for k, v in sg.items() if k != "cells"}
    _emit({**meta, "png": a.png}, a.json, text.spectrogram(sg, a.width, a.height) + (f"→ {a.png}\n" if a.png else ""))
    return 0


def cmd_envelope(a) -> int:
    c = open_capture(a.capture)
    e = dsp.envelope_of(c.iq(), c.rate_hz, c.center_hz, _freq(a.freq), a.bw, a.window_ms)
    if a.png:
        from .render import envelope_png

        envelope_png(e, Path(a.png))
    head = (
        f"{e['freq_hz'] / 1e6:.6f} MHz  window {e['window_s'] * 1e3:.1f} ms  swing {e['swing_db']:.1f} dB  "
        f"duty {e['duty']:.2f}{'  KEYED' if e['keyed'] else ''}  segments {len(e['segments'])}\n"
    )
    _emit({**e, "png": a.png}, a.json, head + text.envelope(e, a.width) + (f"→ {a.png}\n" if a.png else ""))
    return 0


def cmd_compare(a) -> int:
    ca, cb = open_capture(a.a), open_capture(a.b)
    w = _window(a, [ca, cb])
    r = campaign.compare(ca, cb, w, a.top)
    na = str(ca.tags.get("antenna") or ca.path.stem)
    nb = str(cb.tags.get("antenna") or cb.path.stem)
    _emit(r, a.json, campaign.compare_markdown(r, na, nb))
    return 0


def cmd_ladder(a) -> int:
    caps = [c for c in campaign.captures_from(a.source)]
    if a.antenna:
        caps = [c for c in caps if str(c.tags.get("antenna")) == a.antenna]
    w = _window(a, caps) if caps else None
    r = campaign.ladder(caps, w)
    _emit(r, a.json, campaign.ladder_markdown(r))
    return 0


def cmd_report(a) -> int:
    caps = campaign.captures_from(a.source)
    md = campaign.report(caps, a.res, a.merge_hz)
    if a.out:
        Path(a.out).write_text(md)
        print(a.out)
    else:
        sys.stdout.write(md)
    return 0


def cmd_fft_peaks(a) -> int:
    rows, side = load_fft_u8(a.capture)
    r = fftcap.fft_peaks(rows, side, a.sigma, a.min_gap_khz * 1e3, a.max)
    lines = [
        f"frames={r['frames']}  bins/frame={r['frame_size']}  rate={r['sample_rate_hz'] / 1e6:.3f} MS/s  "
        f"centre={r['center_freq_hz'] / 1e6:.4f} MHz  threshold={r['threshold_byte']:.1f} byte"
    ]
    lines += [f"  {p['freq_hz'] / 1e6:>10.4f} MHz  {p['strength_dbfs']:>6.1f} dBFS  bin={p['bin']}" for p in r["peaks"]] or [
        "  (no peaks above threshold)"
    ]
    _emit(r, a.json, "\n".join(lines))
    return 0


def cmd_fft_png(a) -> int:
    from .render import fft_strip_png

    rows, side = load_fft_u8(a.capture)
    out = Path(a.png) if a.png else Path(a.capture).with_suffix(".png")
    fft_strip_png(rows, side, out, Path(a.capture).name)
    print(out)
    return 0


def cmd_sidecars(a) -> int:
    from .sidecars import backfill

    return backfill(Path(a.root))


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="ferrite-analyze",
        description="Offline analysis of Ferrite captures (cf32 / WAV IQ / audio / FFT u8 + sidecars).",
    )
    p.add_argument("--version", action="version", version=f"ferrite-analyze {__version__}")
    sub = p.add_subparsers(dest="cmd", required=True)

    def add(name, fn, help_):
        sp = sub.add_parser(name, help=help_, description=help_)
        sp.add_argument("--json", action="store_true", help="emit JSON")
        sp.set_defaults(fn=fn)
        return sp

    def window(sp, res=campaign.DEFAULT_RES_HZ):
        sp.add_argument("--lo", help="window low edge (Hz, or 98.3M / 915k)")
        sp.add_argument("--hi", help="window high edge")
        sp.add_argument("--res", type=float, default=res, help=f"resolution Hz/bin (default {res:g})")
        sp.add_argument(
            "--merge-hz",
            type=float,
            default=campaign.MERGE_HZ,
            help=f"peaks within this are one carrier in tables (default {campaign.MERGE_HZ:g})",
        )

    sp = add("info", cmd_info, "What a capture is: kind, rate, centre, length, tags")
    sp.add_argument("capture")

    sp = add("spectrum", cmd_spectrum, "Peaks / floor / width / duty / keyed over a window (the daemon's `spectrum`)")
    sp.add_argument("capture")
    window(sp, res=10.0)
    sp.add_argument("--peaks", type=int, default=25)
    sp.add_argument("--profile", type=int, default=120, help="profile points (0 = none)")
    sp.add_argument("--dc-guard", type=float, default=None, help="Hz around the LO to ignore (default: 1500 on a raw source tap)")
    sp.add_argument("--keyed", action="store_true", help="only keyed peaks")

    sp = add("spectrogram", cmd_spectrogram, "Time × frequency of a range (ASCII, optional PNG)")
    sp.add_argument("capture")
    sp.add_argument("lo")
    sp.add_argument("hi")
    sp.add_argument("--res", type=float, default=10.0)
    sp.add_argument("--step-ms", type=float, default=20.0)
    sp.add_argument("--png")
    sp.add_argument("--width", type=int, default=80)
    sp.add_argument("--height", type=int, default=30)

    sp = add("envelope", cmd_envelope, "On/off power timeline at one frequency (ASCII, optional PNG)")
    sp.add_argument("capture")
    sp.add_argument("freq")
    sp.add_argument("--bw", type=float, default=200.0)
    sp.add_argument("--window-ms", type=float, default=10.0)
    sp.add_argument("--png")
    sp.add_argument("--width", type=int, default=100)

    sp = add("compare", cmd_compare, "Two captures (e.g. antenna A vs B) on one window → floor / carrier delta table")
    sp.add_argument("a")
    sp.add_argument("b")
    window(sp)
    sp.add_argument("--top", type=int, default=12, help="carriers to list")

    sp = add("ladder", cmd_ladder, "Captures tagged by gain (dir, manifest or files) → gain-ladder table")
    sp.add_argument("source", help="directory of captures or a manifest .json")
    sp.add_argument("--antenna", help="only captures tagged with this antenna")
    window(sp)

    sp = add("report", cmd_report, "Markdown characterization report straight from captures")
    sp.add_argument("source", help="directory of captures or a manifest .json")
    sp.add_argument("--res", type=float, default=campaign.DEFAULT_RES_HZ)
    sp.add_argument("--merge-hz", type=float, default=campaign.MERGE_HZ)
    sp.add_argument("-o", "--out", help="write here instead of stdout")

    sp = add("fft-peaks", cmd_fft_peaks, "Carrier peaks in a LogMagU8 FFT byte capture (.bin + sidecar)")
    sp.add_argument("capture")
    sp.add_argument("--sigma", type=float, default=3.0)
    sp.add_argument("--min-gap-khz", type=float, default=5.0)
    sp.add_argument("--max", type=int, default=50)

    sp = add("fft-png", cmd_fft_png, "Render a LogMagU8 FFT byte capture to a strip PNG")
    sp.add_argument("capture")
    sp.add_argument("png", nargs="?")

    sp = add("sidecars", cmd_sidecars, "Backfill missing sample sidecars (non-destructive)")
    sp.add_argument("root", nargs="?", default=str(Path(__file__).resolve().parents[3] / "samples"))
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.fn(args)
    except (ValueError, FileNotFoundError) as e:
        print(f"ferrite-analyze: {e}", file=sys.stderr)
        return 2
