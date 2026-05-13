#!/usr/bin/env python3
"""FT8 world map — plot Maidenhead grid squares on a NASA Blue Marble basemap.

Reads FT8 decoder output (or `--grids "DN40,DM79,..."`) and renders a PNG
with each grid as a marker, plus great-circle distances back to the
operator's receive location so the AI doesn't have to compute them.
Replaces the previous hand-drawn continent-polygon basemap.

Usage:
    python3 tools/ft8_worldmap.py --grids "DN40,DM79,EN44" -o /tmp/m.png

    # Pipe decoder logs in:
    ferrite-ctl tail decoder --lookback 300 --category ft8 \\
        | python3 tools/ft8_worldmap.py -o /tmp/m.png

    # Structured stdout for scan loops:
    python3 tools/ft8_worldmap.py --grids "DN40,EN44" --json

    # ASCII fallback (no PNG, prints to stdout):
    python3 tools/ft8_worldmap.py --grids "DN40,EN44" --ascii

Options:
    --grids "G1,G2,..."   Comma-separated 4-char Maidenhead grids. If
                          omitted, reads stdin and extracts grids from
                          decoder text.
    --rx-grid GRID        Operator's grid (default DM04). Used for the
                          map marker AND distance computation. Pass `--`
                          (i.e. empty value via `--rx-grid ""`) to skip.
    --output / -o PATH    PNG output path. Omit to print stdout summary.
    --ascii               Force the ASCII renderer.
    --json                Emit a JSON object with grids + distances on
                          stdout. Skips ASCII art.
    --width / --height    PNG dimensions (default 1600 × 800).
    --basemap PATH        Override the vendored basemap image.

The basemap is `tools/assets/bluemarble_2048.jpg` (NASA, public domain).
"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from collections import defaultdict
from pathlib import Path


TOOLS_DIR = Path(__file__).resolve().parent
DEFAULT_BASEMAP = TOOLS_DIR / "assets" / "bluemarble_2048.jpg"

# Mean Earth radius in km (WGS-84 mean radius). Plenty for HF-DX scale.
EARTH_RADIUS_KM = 6371.0


def grid_to_latlon(grid: str) -> tuple[float, float] | None:
    """Convert a 4-char Maidenhead locator to (lat, lon) at the centre of
    its 1° × 2° square. Returns None if the input doesn't parse."""
    grid = grid.upper().strip()
    if len(grid) < 4:
        return None
    if not (
        grid[0].isalpha()
        and grid[1].isalpha()
        and grid[2].isdigit()
        and grid[3].isdigit()
    ):
        return None
    # Field: 18 × 18 cells, each 20° lon × 10° lat, A starts at -180/-90.
    lon = (ord(grid[0]) - ord("A")) * 20 - 180
    lat = (ord(grid[1]) - ord("A")) * 10 - 90
    # Square: 10 × 10 inside each field.
    lon += int(grid[2]) * 2
    lat += int(grid[3]) * 1
    # Centre of the square.
    lon += 1.0
    lat += 0.5
    return (lat, lon)


def haversine_km(
    a: tuple[float, float], b: tuple[float, float]
) -> float:
    """Great-circle distance between two (lat, lon) points in kilometres."""
    lat1, lon1 = math.radians(a[0]), math.radians(a[1])
    lat2, lon2 = math.radians(b[0]), math.radians(b[1])
    dlat = lat2 - lat1
    dlon = lon2 - lon1
    h = math.sin(dlat / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin(dlon / 2) ** 2
    return 2 * EARTH_RADIUS_KM * math.asin(math.sqrt(h))


def initial_bearing_deg(
    a: tuple[float, float], b: tuple[float, float]
) -> float:
    """Initial great-circle bearing from a to b in degrees, 0–360."""
    lat1, lon1 = math.radians(a[0]), math.radians(a[1])
    lat2, lon2 = math.radians(b[0]), math.radians(b[1])
    dlon = lon2 - lon1
    x = math.sin(dlon) * math.cos(lat2)
    y = math.cos(lat1) * math.sin(lat2) - math.sin(lat1) * math.cos(lat2) * math.cos(dlon)
    return (math.degrees(math.atan2(x, y)) + 360.0) % 360.0


def extract_grids_from_text(text: str) -> list[str]:
    """Pull 4-char Maidenhead grids out of FT8 decoder text. Returns a
    deduplicated, order-preserving list."""
    pattern = r"\b([A-R]{2}[0-9]{2})\b"
    seen: dict[str, None] = {}
    for match in re.findall(pattern, text.upper()):
        seen[match] = None
    return list(seen)


def build_records(
    grids: list[str], rx_grid: str | None
) -> list[dict[str, object]]:
    """Build the per-grid record list — coordinates + (if rx_grid is set)
    distance + bearing. Sorted by distance ascending when rx is known,
    else by grid name."""
    rx_coord = grid_to_latlon(rx_grid) if rx_grid else None
    records: list[dict[str, object]] = []
    for g in grids:
        coord = grid_to_latlon(g)
        if coord is None:
            records.append({"grid": g, "valid": False})
            continue
        rec: dict[str, object] = {
            "grid": g,
            "lat": round(coord[0], 3),
            "lon": round(coord[1], 3),
            "valid": True,
        }
        if rx_coord is not None:
            rec["distance_km"] = round(haversine_km(rx_coord, coord), 1)
            rec["bearing_deg"] = round(initial_bearing_deg(rx_coord, coord), 1)
        records.append(rec)

    if rx_coord is not None:
        records.sort(key=lambda r: (not r["valid"], r.get("distance_km", math.inf)))
    else:
        records.sort(key=lambda r: (not r["valid"], r["grid"]))
    return records


def print_distance_table(records: list[dict[str, object]], rx_grid: str | None) -> None:
    """Operator-facing summary on stdout. Distances first when known."""
    valid = [r for r in records if r["valid"]]
    invalid = [r for r in records if not r["valid"]]
    if rx_grid and valid and "distance_km" in valid[0]:
        print(f"\nDistances from {rx_grid} (great-circle):")
        for r in valid:
            print(
                f"  {r['grid']}: {r['distance_km']:>7.1f} km, bearing {r['bearing_deg']:>5.1f}°"
            )
        farthest = max(valid, key=lambda r: r["distance_km"])  # type: ignore[arg-type,return-value]
        print(f"\nFarthest: {farthest['grid']} @ {farthest['distance_km']} km")
    if invalid:
        bad = ", ".join(str(r["grid"]) for r in invalid)
        print(f"\nUnparseable grids skipped: {bad}", file=sys.stderr)


# ---------------------------------------------------------------------------
# PNG renderer — composites markers + paths onto the vendored Blue Marble
# ---------------------------------------------------------------------------


def render_png_map(
    records: list[dict[str, object]],
    output_path: str,
    *,
    rx_grid: str | None,
    width: int,
    height: int,
    basemap_path: Path,
) -> str:
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError:
        return "Error: Pillow not installed. Install with: pip install Pillow"

    if not basemap_path.exists():
        return f"Error: basemap missing at {basemap_path}"

    # Load + resize. Bicubic keeps coastlines crisp at the typical
    # 1600 × 800 output (slight downscale from the 2048 × 1024 source).
    base = Image.open(basemap_path).convert("RGB")
    base = base.resize((width, height), Image.BICUBIC)
    # Dim by ~35 % so coloured markers + text pop.
    overlay = Image.new("RGB", base.size, (0, 0, 0))
    base = Image.blend(base, overlay, 0.35)

    img = base
    draw = ImageDraw.Draw(img, "RGBA")

    def load_font(path_candidates: list[str], size: int) -> ImageFont.ImageFont:
        for p in path_candidates:
            try:
                return ImageFont.truetype(p, size)
            except OSError:
                continue
        return ImageFont.load_default()

    font_small = load_font(
        ["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"], max(10, height // 80)
    )
    font_title = load_font(
        ["/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"], max(18, height // 40)
    )

    def latlon_to_xy(lat: float, lon: float) -> tuple[int, int]:
        x = int((lon + 180) / 360 * width)
        y = int((90 - lat) / 180 * height)
        return (x, y)

    # Faint graticule — every 30° lon and 15° lat.
    grid_color = (255, 255, 255, 25)
    for lon in range(-180, 181, 30):
        x = int((lon + 180) / 360 * width)
        draw.line([(x, 0), (x, height)], fill=grid_color, width=1)
    for lat in range(-75, 76, 15):
        y = int((90 - lat) / 180 * height)
        draw.line([(0, y), (width, y)], fill=grid_color, width=1)

    # Receiver location + faint lines to each station.
    rx_xy: tuple[int, int] | None = None
    if rx_grid:
        rx_coord = grid_to_latlon(rx_grid)
        if rx_coord:
            rx_xy = latlon_to_xy(*rx_coord)

    if rx_xy is not None:
        rx_x, rx_y = rx_xy
        for r in records:
            if not r["valid"]:
                continue
            x, y = latlon_to_xy(float(r["lat"]), float(r["lon"]))  # type: ignore[arg-type]
            draw.line([(rx_x, rx_y), (x, y)], fill=(120, 180, 255, 96), width=1)

    # Station markers — yellow filled circle with a white ring.
    radius = max(4, height // 200)
    label_offset = radius + 3
    for r in records:
        if not r["valid"]:
            continue
        x, y = latlon_to_xy(float(r["lat"]), float(r["lon"]))  # type: ignore[arg-type]
        draw.ellipse(
            [x - radius, y - radius, x + radius, y + radius],
            fill=(255, 200, 50, 230),
            outline=(255, 255, 255, 230),
            width=1,
        )

    # Labels in a second pass so they sit above markers. If we have a
    # distance, include it on the label so the PNG is self-describing
    # for a reader looking at the image alone.
    for r in records:
        if not r["valid"]:
            continue
        x, y = latlon_to_xy(float(r["lat"]), float(r["lon"]))  # type: ignore[arg-type]
        label = str(r["grid"])
        if "distance_km" in r:
            label = f"{label}  {int(round(float(r['distance_km'])))} km"  # type: ignore[arg-type]
        draw.text(
            (x + label_offset, y - label_offset),
            label,
            fill=(255, 255, 200, 230),
            font=font_small,
        )

    # Receiver marker last so it sits on top.
    if rx_xy is not None:
        rx_x, rx_y = rx_xy
        size = max(7, height // 110)
        draw.polygon(
            [
                (rx_x, rx_y - size),
                (rx_x + size, rx_y),
                (rx_x, rx_y + size),
                (rx_x - size, rx_y),
            ],
            fill=(0, 230, 255, 230),
            outline=(255, 255, 255, 230),
        )
        draw.text(
            (rx_x + size + 3, rx_y - size - 2),
            f"RX:{rx_grid}",
            fill=(0, 230, 255, 230),
            font=font_small,
        )

    # Title + legend strip.
    n_valid = sum(1 for r in records if r["valid"])
    title = f"FT8 reception map — {n_valid} grid square{'' if n_valid == 1 else 's'}"
    draw.text((12, 8), title, fill=(255, 255, 255, 230), font=font_title)

    legend_y = height - max(48, height // 16)
    draw.ellipse(
        [16, legend_y, 16 + 2 * radius, legend_y + 2 * radius],
        fill=(255, 200, 50, 230),
        outline=(255, 255, 255, 230),
    )
    draw.text(
        (16 + 2 * radius + 8, legend_y - 2),
        "Station received",
        fill=(255, 255, 255, 230),
        font=font_small,
    )
    if rx_grid:
        diamond_y = legend_y + max(18, height // 50)
        s = max(6, height // 130)
        cx = 16 + s
        draw.polygon(
            [
                (cx, diamond_y - s),
                (cx + s, diamond_y),
                (cx, diamond_y + s),
                (cx - s, diamond_y),
            ],
            fill=(0, 230, 255, 230),
            outline=(255, 255, 255, 230),
        )
        draw.text(
            (cx + s + 8, diamond_y - s),
            f"Receiver ({rx_grid})",
            fill=(255, 255, 255, 230),
            font=font_small,
        )

    img.save(output_path)
    return str(output_path)


# ---------------------------------------------------------------------------
# ASCII fallback — keeps the script useful when Pillow isn't available
# ---------------------------------------------------------------------------


def render_ascii_map(grids: list[str], width: int = 120, height: int = 35) -> str:
    grid_labels: dict[tuple[int, int], list[str]] = {}
    for g in grids:
        coord = grid_to_latlon(g)
        if coord is None:
            continue
        lat, lon = coord
        x = max(0, min(width - 1, int((lon + 180) / 360 * width)))
        y = max(0, min(height - 1, int((90 - lat) / 180 * height)))
        grid_labels.setdefault((y, x), []).append(g)

    canvas = [[" " for _ in range(width)] for _ in range(height)]

    # Rough land outline — ASCII can't do real cartography, this is just
    # a vague visual anchor so the markers don't float in nothing.
    land_points = [
        (50, -125), (55, -120), (60, -140), (70, -160), (70, -100), (60, -95),
        (50, -90), (45, -85), (40, -75), (35, -80), (30, -85), (25, -80),
        (30, -90), (30, -100), (25, -100), (20, -105), (30, -115), (35, -120),
        (40, -125), (45, -125),
        (10, -75), (5, -80), (0, -80), (-5, -80), (-10, -75), (-15, -75),
        (-20, -70), (-25, -65), (-30, -70), (-35, -70), (-40, -70), (-50, -75),
        (-55, -70), (-55, -65), (-45, -65), (-35, -55), (-25, -50), (-20, -45),
        (-15, -45), (-10, -50), (-5, -50), (0, -50), (5, -60), (10, -65),
        (70, 25), (65, 30), (60, 30), (55, 15), (50, 0), (45, -10), (40, -10),
        (35, -5), (40, 0), (45, 10), (50, 20), (55, 25), (60, 25),
        (35, -5), (30, -10), (25, -15), (20, -17), (15, -17), (10, -15),
        (5, -10), (5, 0), (0, 10), (-5, 15), (-10, 25), (-15, 30), (-20, 35),
        (-25, 30), (-30, 25), (-35, 20), (-35, 25), (-30, 30), (-25, 35),
        (70, 70), (70, 100), (70, 140), (70, 180), (60, 160), (55, 160),
        (50, 140), (45, 135), (40, 140), (35, 140), (35, 130), (40, 120),
        (35, 120), (30, 120), (25, 120), (20, 110), (15, 100), (10, 100),
        (5, 100), (0, 105), (-5, 115), (-10, 120), (5, 95), (15, 100),
        (-15, 130), (-20, 115), (-25, 115), (-30, 115), (-35, 120), (-35, 140),
        (-40, 145), (-35, 150), (-30, 155), (-25, 150), (-20, 145), (-15, 145),
    ]
    for lat, lon in land_points:
        x = int((lon + 180) / 360 * width)
        y = int((90 - lat) / 180 * height)
        if 0 <= x < width and 0 <= y < height and canvas[y][x] == " ":
            canvas[y][x] = "."

    for (y, x), _ in grid_labels.items():
        canvas[y][x] = "*"

    lines = ["┌" + "─" * width + "┐"]
    for row in canvas:
        lines.append("│" + "".join(row) + "│")
    lines.append("└" + "─" * width + "┘")

    lines.append("")
    lines.append(f"  * = station received ({len(grids)} unique grids)")
    lines.append(f"  . = land mass (approximate)")
    lines.append("")

    regions: dict[str, list[str]] = defaultdict(list)
    for g in sorted(grids):
        coord = grid_to_latlon(g)
        if coord is None:
            continue
        lat, lon = coord
        if lon < -30:
            regions["North America" if lat > 15 else "South America"].append(g)
        elif lon < 60:
            regions["Europe" if lat > 35 else "Africa"].append(g)
        elif lon < 150:
            regions["Asia" if lat > -10 else "Oceania"].append(g)
        else:
            regions["Pacific"].append(g)
    for region, gs in sorted(regions.items()):
        lines.append(f"  {region}: {', '.join(sorted(gs))}")

    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description="FT8 world-map plotter")
    parser.add_argument("--grids", type=str, help="Comma-separated 4-char Maidenhead grids")
    parser.add_argument(
        "--rx-grid",
        type=str,
        default="DM04",
        help="Receiver grid (default DM04 ≈ Los Angeles). Empty string disables.",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=str,
        help="PNG output path. Omit to print ASCII / summary to stdout.",
    )
    parser.add_argument(
        "--ascii", action="store_true", help="Force ASCII map even if --output is set."
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit a JSON object with grids + distances + bearings on stdout.",
    )
    parser.add_argument("--width", type=int, default=1600, help="PNG width (default 1600)")
    parser.add_argument("--height", type=int, default=800, help="PNG height (default 800)")
    parser.add_argument(
        "--basemap",
        type=Path,
        default=DEFAULT_BASEMAP,
        help=f"Basemap JPEG/PNG (default {DEFAULT_BASEMAP.relative_to(TOOLS_DIR.parent)})",
    )
    args = parser.parse_args()

    if args.grids:
        grids = [g.strip() for g in args.grids.split(",") if g.strip()]
    else:
        text = sys.stdin.read()
        grids = extract_grids_from_text(text)

    if not grids:
        print("No grids found. Provide --grids or pipe FT8 decoder output.", file=sys.stderr)
        return 1

    rx_grid = args.rx_grid.strip() or None
    records = build_records(grids, rx_grid)

    # JSON output is mutually exclusive with the chatty stdout summary.
    if args.json:
        out = {
            "rx_grid": rx_grid,
            "rx_latlon": list(grid_to_latlon(rx_grid)) if rx_grid and grid_to_latlon(rx_grid) else None,
            "grids": records,
        }
        if args.output and not args.ascii:
            png_result = render_png_map(
                records,
                args.output,
                rx_grid=rx_grid,
                width=args.width,
                height=args.height,
                basemap_path=args.basemap,
            )
            if png_result.startswith("Error:"):
                print(png_result, file=sys.stderr)
                return 1
            out["png_path"] = png_result
        print(json.dumps(out, indent=2))
        return 0

    if args.output and not args.ascii:
        result = render_png_map(
            records,
            args.output,
            rx_grid=rx_grid,
            width=args.width,
            height=args.height,
            basemap_path=args.basemap,
        )
        if result.startswith("Error:"):
            print(result, file=sys.stderr)
            return 1
        print(f"Map saved to: {result}")
        print(f"Grids plotted: {', '.join(r['grid'] for r in records if r['valid'])}")
        print_distance_table(records, rx_grid)
        return 0

    # ASCII path.
    print(render_ascii_map(grids))
    print_distance_table(records, rx_grid)
    return 0


if __name__ == "__main__":
    sys.exit(main())
