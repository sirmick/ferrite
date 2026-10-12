"""Antenna / gain characterization from captures: ``compare`` (two
captures on one window), ``ladder`` (captures tagged by gain) and
``report`` (markdown tables for a whole campaign directory).

Every number comes from :func:`dsp.spectrum` — the same definitions the
daemon's ``spectrum`` tool uses — so a report built here reads like the
one the in-app AI wrote from live measurements, but is reproducible from
the captures alone.

Levels: ``floor_dbfs`` is the spectrum floor (median of the averaged
bins in the window). A peak's ``snr_db`` is its max-hold over that floor
and ``abs_dbfs`` = floor + snr_db, so two captures compare on absolute
level (which antenna delivers more signal) and on SNR (which hears it
better) separately.
"""
from __future__ import annotations

import json
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from . import dsp, text
from .captures import Capture, find_captures, open_capture

# Default window: the central 60 % of the span — inside every supported
# radio's analog passband (same fallback as SignalList's floor estimate).
DEFAULT_SPAN_FRACTION = 0.6
DEFAULT_RES_HZ = 50.0
# Peaks closer than this are one carrier in the tables: FM/AM modulation
# shows as a cluster of maxima at fine resolution (NBFM: ±5 kHz). At HF a
# whole FT8 sub-band becomes one row — fine for antenna comparison.
MERGE_HZ = 5_000.0
# Antennas whose score is within this are reported as a tie.
TIE_DB = 1.0
# Peaks kept per capture for matching across captures.
MEASURE_PEAKS = 100


@dataclass
class Window:
    lo: float
    hi: float
    res_hz: float = DEFAULT_RES_HZ
    merge_hz: float = MERGE_HZ


def default_window(caps: list[Capture], res_hz: float = DEFAULT_RES_HZ, merge_hz: float = MERGE_HZ) -> Window:
    """The window every capture covers: ``lo_hz``/``hi_hz`` tags when the
    captures carry them, else the central 60 % of each span (the analog
    passband on every supported radio), intersected."""
    tagged = [(c.tags.get("lo_hz"), c.tags.get("hi_hz")) for c in caps]
    if all(lo is not None and hi is not None for lo, hi in tagged):
        lo = max(float(t[0]) for t in tagged)
        hi = min(float(t[1]) for t in tagged)
    else:
        half = DEFAULT_SPAN_FRACTION / 2.0
        lo = max(c.center_hz - half * c.rate_hz for c in caps)
        hi = min(c.center_hz + half * c.rate_hz for c in caps)
    if not hi > lo:
        raise ValueError("captures don't overlap — give --lo/--hi")
    return Window(lo, hi, res_hz, merge_hz)


def dc_guard_for(c: Capture) -> float:
    """1.5 kHz around the LO on a raw source tap (the daemon's rule)."""
    return dsp.DC_GUARD_HZ if c.sidecar.get("wideband") else 0.0


def measure(c: Capture, w: Window, max_peaks: int = MEASURE_PEAKS) -> dict:
    s = dsp.spectrum(
        c.iq(),
        c.rate_hz,
        c.center_hz,
        w.lo,
        w.hi,
        res_hz=w.res_hz,
        max_peaks=max_peaks,
        profile_points=0,
        dc_guard_hz=dc_guard_for(c),
        keying=False,  # tables use level/SNR only; keying costs a pass
    )
    peaks = carriers(
        [{**p, "snr_db": p["peak_db"], "abs_dbfs": s["floor_dbfs"] + p["peak_db"]} for p in s["peaks"]],
        w.merge_hz,
    )
    return {
        "path": str(c.path),
        "tags": c.tags,
        "center_hz": c.center_hz,
        "rate_hz": c.rate_hz,
        "floor_dbfs": s["floor_dbfs"],
        "res_hz": s["res_hz"],
        "frames": s["frames"],
        "peaks": peaks,
    }


def carriers(peaks: list[dict], merge_hz: float) -> list[dict]:
    """Cluster peaks into carriers, strongest first: a peak within
    ``merge_hz`` of a stronger one joins it. Each carrier is its strongest
    peak plus ``span_hz`` (the cluster's extent)."""
    out: list[dict] = []
    for p in sorted(peaks, key=lambda p: -p["snr_db"]):
        for c in out:
            if abs(p["freq_hz"] - c["freq_hz"]) <= merge_hz:
                c["_lo"] = min(c["_lo"], p["freq_hz"])
                c["_hi"] = max(c["_hi"], p["freq_hz"])
                break
        else:
            out.append({**p, "_lo": p["freq_hz"], "_hi": p["freq_hz"]})
    for c in out:
        c["span_hz"] = c.pop("_hi") - c.pop("_lo")
    return out


def _match(peaks: list[dict], f: float, tol: float) -> dict | None:
    near = [p for p in peaks if abs(p["freq_hz"] - f) <= tol]
    return max(near, key=lambda p: p["snr_db"]) if near else None


def match_tol(w: Window) -> float:
    return max(3.0 * w.res_hz, w.merge_hz)


def compare(a: Capture, b: Capture, w: Window | None = None, top: int = 12) -> dict:
    """Floor delta + per-carrier level/SNR deltas (B − A)."""
    w = w or default_window([a, b])
    ma, mb = measure(a, w), measure(b, w)
    tol = match_tol(w)
    freqs: list[float] = []
    for p in sorted(ma["peaks"] + mb["peaks"], key=lambda p: -p["snr_db"]):
        if all(abs(p["freq_hz"] - f) > tol for f in freqs):
            freqs.append(p["freq_hz"])
        if len(freqs) >= top:
            break
    rows = []
    for f in sorted(freqs):
        pa, pb = _match(ma["peaks"], f, tol), _match(mb["peaks"], f, tol)
        rows.append(
            {
                "freq_hz": f,
                "a_abs_dbfs": pa and pa["abs_dbfs"],
                "a_snr_db": pa and pa["snr_db"],
                "b_abs_dbfs": pb and pb["abs_dbfs"],
                "b_snr_db": pb and pb["snr_db"],
                "d_abs_db": (pb["abs_dbfs"] - pa["abs_dbfs"]) if pa and pb else None,
                "d_snr_db": (pb["snr_db"] - pa["snr_db"]) if pa and pb else None,
            }
        )
    return {
        "window": {"lo_hz": w.lo, "hi_hz": w.hi, "res_hz": w.res_hz, "merge_hz": w.merge_hz},
        "a": {k: ma[k] for k in ("path", "tags", "floor_dbfs")},
        "b": {k: mb[k] for k in ("path", "tags", "floor_dbfs")},
        "floor_delta_db": mb["floor_dbfs"] - ma["floor_dbfs"],
        "carriers": rows,
    }


def compare_markdown(r: dict, a_name: str = "A", b_name: str = "B") -> str:
    out = [
        f"Window {text.mhz(r['window']['lo_hz'])}–{text.mhz(r['window']['hi_hz'])} MHz, "
        f"{r['window']['res_hz']:.0f} Hz bins. Δ = {b_name} − {a_name}.\n",
        text.table(
            ["", f"{a_name} floor dBFS", f"{b_name} floor dBFS", "Δ dB"],
            [["floor", r["a"]["floor_dbfs"], r["b"]["floor_dbfs"], r["floor_delta_db"]]],
            "lrrr",
        ),
        text.table(
            ["MHz", f"{a_name} dBFS", f"{a_name} SNR", f"{b_name} dBFS", f"{b_name} SNR", "Δ level", "Δ SNR"],
            [
                [
                    f"{c['freq_hz'] / 1e6:.4f}",
                    c["a_abs_dbfs"],
                    c["a_snr_db"],
                    c["b_abs_dbfs"],
                    c["b_snr_db"],
                    c["d_abs_db"],
                    c["d_snr_db"],
                ]
                for c in r["carriers"]
            ],
            "lrrrrrr",
        ),
    ]
    return "\n".join(out)


def _gain(c: Capture) -> float | None:
    g = c.tags.get("gain_db")
    try:
        return float(g) if g is not None else None
    except (TypeError, ValueError):
        return None


def judge(dg: float, df: float, ds: float | None) -> str:
    """One gain step's verdict from the floor / SNR change."""
    if (ds is not None and ds < -3.0) or df > dg + 3.0:
        return "overload"
    if dg > 0 and df >= 0.7 * dg and (ds is None or abs(ds) < 1.5):
        return "antenna-noise-limited"
    if dg > 0 and df < 0.5 * dg and ds is not None and ds > 1.0:
        return "receiver-noise-limited"
    return "mixed"


def ladder(caps: list[Capture], w: Window | None = None) -> dict:
    """Captures of one spot/antenna at several gains → per-step floor,
    tracked-carrier SNR, verdict, and the recommended setting."""
    tagged = sorted(((g, c) for c in caps if (g := _gain(c)) is not None), key=lambda t: t[0])
    if len(tagged) < 2:
        raise ValueError("ladder needs ≥ 2 captures tagged with gain_db (sidecar tags or …_g40_… names)")
    w = w or default_window([c for _, c in tagged])
    ms = [(g, measure(c, w)) for g, c in tagged]
    tol = match_tol(w)
    # Track the strongest carrier of the middle step through the ladder.
    ref = ms[len(ms) // 2][1]["peaks"]
    f_ref = ref[0]["freq_hz"] if ref else None
    steps = []
    prev = None
    for g, m in ms:
        p = _match(m["peaks"], f_ref, tol) if f_ref is not None else None
        snr = p["snr_db"] if p else None
        step = {
            "gain_db": g,
            "path": m["path"],
            "floor_dbfs": m["floor_dbfs"],
            "carrier_hz": f_ref,
            "carrier_snr_db": snr,
            "carrier_abs_dbfs": p["abs_dbfs"] if p else None,
            "carriers_over_10db": sum(1 for q in m["peaks"] if q["snr_db"] >= 10.0),
            "verdict": None,
        }
        if prev is not None:
            dg = g - prev["gain_db"]
            df = step["floor_dbfs"] - prev["floor_dbfs"]
            ds = (snr - prev["carrier_snr_db"]) if snr is not None and prev["carrier_snr_db"] is not None else None
            step["d_floor_db"], step["d_snr_db"] = df, ds
            step["verdict"] = judge(dg, df, ds)
        steps.append(step)
        prev = step
    # Best: the lowest gain within 1 dB of the best SNR seen before the
    # first overload step.
    usable = []
    for s in steps:
        if s["verdict"] == "overload":
            break
        usable.append(s)
    best = None
    snrs = [s["carrier_snr_db"] for s in usable if s["carrier_snr_db"] is not None]
    if snrs:
        top = max(snrs)
        best = next(s["gain_db"] for s in usable if s["carrier_snr_db"] is not None and s["carrier_snr_db"] >= top - 1.0)
    return {"window": {"lo_hz": w.lo, "hi_hz": w.hi, "res_hz": w.res_hz}, "steps": steps, "recommended_gain_db": best}


def ladder_markdown(r: dict) -> str:
    f = r["steps"][0]["carrier_hz"]
    head = (
        f"Tracked carrier {f / 1e6:.4f} MHz. " if f is not None else "No carrier to track. "
    ) + (
        f"Recommended: gain {r['recommended_gain_db']:g}.\n"
        if r["recommended_gain_db"] is not None
        else "No usable step.\n"
    )
    rows = [
        [
            s["gain_db"],
            s["floor_dbfs"],
            s.get("d_floor_db"),
            s["carrier_snr_db"],
            s.get("d_snr_db"),
            s["carriers_over_10db"],
            s["verdict"] or "—",
        ]
        for s in r["steps"]
    ]
    return head + "\n" + text.table(
        ["gain", "floor dBFS", "Δ floor", "carrier SNR", "Δ SNR", "carriers ≥10 dB", "verdict"], rows, "rrrrrrl"
    )


# ── report ──────────────────────────────────────────────────────────────


def load_manifest(path: Path) -> list[Capture]:
    """``[{path, spot?, antenna?, gain_db?, …}]`` — paths relative to the
    manifest; the extra keys become the capture's tags."""
    entries = json.loads(Path(path).read_text())
    out = []
    for e in entries:
        c = open_capture(Path(path).parent / e["path"])
        c.sidecar = {**c.sidecar, "tags": {**(c.sidecar.get("tags") or {}), **{k: v for k, v in e.items() if k != "path"}}}
        out.append(c)
    return out


def _spot_key(c: Capture) -> str:
    return str(c.tags.get("spot") or f"{c.center_hz / 1e6:.3f} MHz")


def report(caps: list[Capture], res_hz: float = DEFAULT_RES_HZ, merge_hz: float = MERGE_HZ) -> str:
    """Markdown: per spot an antenna table (at the most common gain) and a
    gain ladder per antenna with ≥ 2 gains."""
    if not caps:
        raise ValueError("no IQ captures found")
    spots: dict[str, list[Capture]] = defaultdict(list)
    for c in caps:
        spots[_spot_key(c)].append(c)
    out = [
        "# Characterization report\n",
        f"{len(caps)} capture(s), {len(spots)} spot(s). Levels from the `spectrum` "
        f"definitions (Hann long-FFT, floor = median of averaged bins, SNR = max-hold over "
        f"floor), {res_hz:.0f} Hz bins over the central {DEFAULT_SPAN_FRACTION:.0%} of each span "
        f"(or the spot's `lo_hz`/`hi_hz` tags); peaks within {merge_hz / 1e3:.1f} kHz are one carrier; "
        f"antennas within {TIE_DB:g} dB tie.\n",
    ]
    for spot in sorted(spots, key=lambda s: (spots[s][0].center_hz, s)):
        cs = spots[spot]
        out.append(f"## {spot}\n")
        w = default_window(cs, res_hz, merge_hz)
        gains = [g for c in cs if (g := _gain(c)) is not None]
        common = max(set(gains), key=gains.count) if gains else None
        ant_caps: dict[str, Capture] = {}
        for c in cs:
            ant = str(c.tags.get("antenna") or c.path.stem)
            if common is None or _gain(c) == common:
                ant_caps.setdefault(ant, c)
        if ant_caps:
            ms = {a: measure(c, w) for a, c in sorted(ant_caps.items())}
            tol = match_tol(w)
            # Carriers seen on ≥ 2 antennas carry the comparison.
            allp = sorted((p for m in ms.values() for p in m["peaks"]), key=lambda p: -p["snr_db"])
            shared: list[float] = []
            for p in allp:
                f = p["freq_hz"]
                if all(abs(f - s) > tol for s in shared) and sum(
                    1 for m in ms.values() if _match(m["peaks"], f, tol)
                ) >= min(2, len(ms)):
                    shared.append(f)
                if len(shared) >= 5:
                    break
            rows = []
            score = {}
            for a, m in ms.items():
                hits = [_match(m["peaks"], f, tol) for f in shared]
                snr = [h["snr_db"] for h in hits if h]
                score[a] = (sum(snr) / len(snr) if snr else float("-inf"), -m["floor_dbfs"])
                top = m["peaks"][0] if m["peaks"] else None
                rows.append(
                    [
                        a,
                        m["floor_dbfs"],
                        f"{top['freq_hz'] / 1e6:.4f}" if top else "—",
                        top["snr_db"] if top else None,
                        score[a][0] if snr else None,
                        sum(1 for p in m["peaks"] if p["snr_db"] >= 10.0),
                    ]
                )
            top_a = max(score, key=lambda a: score[a])
            tied = [a for a in sorted(score) if score[top_a][0] - score[a][0] <= TIE_DB]
            best = " ≈ ".join(tied) if len(tied) > 1 else top_a
            g_txt = f" at gain {common:g}" if common is not None else ""
            why = (
                f"mean SNR on shared carriers within {TIE_DB:g} dB"
                if len(tied) > 1
                else "highest mean SNR on carriers heard on ≥ 2 antennas"
            )
            out.append(f"Antennas{g_txt} — best: **{best}** ({why}).\n")
            out.append(
                text.table(
                    ["antenna", "floor dBFS", "strongest MHz", "its SNR", "mean SNR (shared)", "carriers ≥10 dB"],
                    rows,
                    "lrlrrr",
                )
            )
        by_ant: dict[str, list[Capture]] = defaultdict(list)
        for c in cs:
            if _gain(c) is not None:
                by_ant[str(c.tags.get("antenna") or "?")].append(c)
        for a, group in sorted(by_ant.items()):
            if len({_gain(c) for c in group}) < 2:
                continue
            out.append(f"Gain ladder, antenna {a}:\n")
            out.append(ladder_markdown(ladder(group, w)))
    return "\n".join(out)


def captures_from(root_or_manifest: Path) -> list[Capture]:
    p = Path(root_or_manifest)
    if p.is_file() and p.suffix == ".json":
        return load_manifest(p)
    if p.is_dir():
        return find_captures(p, ("iq",))
    return [open_capture(p)]
