"""Text renderings (port of ``analysis/src/text.rs``) plus markdown
tables — for terminals, CI logs and Claude Code transcripts."""
from __future__ import annotations

RAMP = " .:-=+*#%@"
BARS = "▁▂▃▄▅▆▇█"


def sparkline(spec: dict) -> str:
    """One-line sparkline of a spectrum's max-hold profile."""
    prof = spec.get("profile") or []
    mx = max([p[1] for p in prof] + [1.0])
    return "".join(BARS[int(round(max(p[1], 0.0) / mx * 7.0))] for p in prof)


def spectrogram(sg: dict, width: int = 80, height: int = 30) -> str:
    rows, cols, cells = sg["rows"], sg["cols"], sg["cells"]
    cw, ch = max(min(cols, width), 1), max(min(rows, height), 1)
    hi = sg["f0_hz"] + sg["df_hz"] * (cols - 1)
    out = [
        f"{sg['f0_hz'] / 1e6:.4f} MHz {'':>{max(cw - 26, 0)}} {hi / 1e6:.4f} MHz  "
        f"({sg['dt_s'] * rows:.0f} s, {sg['df_hz'] * cols / cw:.1f} Hz/col)"
    ]
    for r in range(ch):
        r0 = r * rows // ch
        r1 = max((r + 1) * rows // ch, r0 + 1)
        line = []
        for c in range(cw):
            c0 = c * cols // cw
            c1 = max((c + 1) * cols // cw, c0 + 1)
            v = int(cells[r0:r1, c0:c1].max())
            line.append(RAMP[v * (len(RAMP) - 1) // 255])
        out.append("".join(line))
    return "\n".join(out) + "\n"


def envelope(e: dict, width: int = 100) -> str:
    """``#`` on / ``.`` off per window, wrapped with time labels."""
    w = max(width, 10)
    marks = ["#" if v > e["threshold_db"] else "." for v in e["power_db"]]
    lines = []
    for i in range(0, len(marks), w):
        lines.append(f"{i * e['window_s']:6.2f}s " + "".join(marks[i : i + w]))
    return "\n".join(lines) + "\n"


def table(headers: list[str], rows: list[list], align: str | None = None) -> str:
    """A GitHub markdown table. ``align`` is one char per column
    (``l``/``r``), default numbers right."""

    def fmt(v):
        if v is None:
            return "—"
        if isinstance(v, float):
            return f"{v:.1f}"
        return str(v)

    srows = [[fmt(v) for v in r] for r in rows]
    if align is None:
        align = "".join(
            "r" if rows and all(isinstance(r[i], (int, float)) or r[i] is None for r in rows) else "l"
            for i in range(len(headers))
        )
    sep = ["---:" if a == "r" else "---" for a in align]
    out = ["| " + " | ".join(headers) + " |", "| " + " | ".join(sep) + " |"]
    out += ["| " + " | ".join(r) + " |" for r in srows]
    return "\n".join(out) + "\n"


def mhz(f: float) -> str:
    return f"{f / 1e6:.4f}"
