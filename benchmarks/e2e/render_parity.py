#!/usr/bin/env python3
"""
render_parity.py — side-by-side renders of two PMTiles archives, so the speed
table has a picture attached to it.

A benchmark that only reports seconds invites the obvious objection: *of course
it is faster, it is doing less*. This script answers that objection with
evidence. It decodes both archives at a set of zooms, plots the decoded tile
geometry (not the source data — what a renderer would actually draw), and
writes one PNG per zoom with tylertoo on the left and tippecanoe on the right,
plus a per-zoom feature and tile count for each.

The counts are *distinct* feature ids within a zoom, so a feature that spans
four tiles is counted once. tippecanoe and tylertoo both duplicate across tile
seams, so raw MVT feature counts would flatter whichever tool cut more tiles.

Dependencies are not part of the harness's hard requirements — run it under uv:

    uv run --with pmtiles --with mapbox-vector-tile --with matplotlib \
        python3 render_parity.py A.pmtiles B.pmtiles --out renders/ --zooms 6,9,12

Nothing here feeds run_e2e.py; it is the qualitative half of the comparison.
"""

from __future__ import annotations

import argparse
import gzip
import json
import math
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt          # noqa: E402
import mapbox_vector_tile                # noqa: E402
from pmtiles.reader import Reader, MmapSource   # noqa: E402
from pmtiles.tile import (Compression, deserialize_directory,  # noqa: E402
                          tileid_to_zxy)

MAX_PLOT = 40_000   # cap plotted features per panel; counts stay exact


def enumerate_tiles(path: Path):
    """Every (z, x, y) addressed by the archive, plus header and reader."""
    data = path.read_bytes()
    reader = Reader(MmapSource(open(path, "rb")))
    header = reader.header()
    out = []

    def walk(off, length):
        for e in deserialize_directory(data[off:off + length]):
            if e.run_length == 0:
                walk(header["leaf_directory_offset"] + e.offset, e.length)
            else:
                for i in range(e.run_length):
                    out.append(tileid_to_zxy(e.tile_id + i))

    walk(header["root_offset"], header["root_length"])
    return out, header, reader


def to_lonlat_factory(z, x, y, extent):
    n = 2.0 ** z

    def fn(px, py):
        wx = (x + px / extent) / n
        wy = (y + py / extent) / n
        return (wx * 360.0 - 180.0,
                math.degrees(math.atan(math.sinh(math.pi * (1.0 - 2.0 * wy)))))
    return fn


def walk_coords(coords, fn):
    if not coords:
        return coords
    if isinstance(coords[0], (int, float)):
        return list(fn(coords[0], coords[1]))
    return [walk_coords(c, fn) for c in coords]


def feature_key(props, mvt_id):
    for k in ("id", "ID", "OGC_FID", "fid", "NE_ID"):
        if props.get(k) is not None:
            return ("p", props[k])
    return ("m", mvt_id)


def decode_zoom(path: Path, z: int):
    """Return (plottable geometries as GeoJSON dicts, distinct ids, tile count)."""
    tiles, header, reader = enumerate_tiles(path)
    at_z = [(x, y) for (tz, x, y) in tiles if tz == z]
    gzipped = header["tile_compression"] == Compression.GZIP
    ids, geoms = set(), []
    for (x, y) in at_z:
        raw = reader.get(z, x, y)
        if raw is None:
            continue
        if gzipped:
            raw = gzip.decompress(raw)
        try:
            dec = mapbox_vector_tile.decode(raw)
        except Exception:
            continue
        for layer in dec.values():
            extent = layer.get("extent", 4096)
            fn = to_lonlat_factory(z, x, y, extent)
            for ft in layer["features"]:
                ids.add(feature_key(ft.get("properties", {}), ft.get("id")))
                if len(geoms) < MAX_PLOT:
                    g = ft["geometry"]
                    geoms.append({"type": g["type"],
                                  "coordinates": walk_coords(g["coordinates"], fn)})
    return geoms, len(ids), len(at_z)


def plot(ax, geoms, color):
    for g in geoms:
        t, c = g["type"], g["coordinates"]
        if t == "Point":
            ax.plot(c[0], c[1], ".", ms=1.2, color=color)
        elif t == "MultiPoint":
            for p in c:
                ax.plot(p[0], p[1], ".", ms=1.2, color=color)
        elif t == "LineString":
            xs, ys = zip(*c) if c else ((), ())
            ax.plot(xs, ys, "-", lw=0.35, color=color)
        elif t == "MultiLineString":
            for ls in c:
                if ls:
                    xs, ys = zip(*ls)
                    ax.plot(xs, ys, "-", lw=0.35, color=color)
        elif t == "Polygon":
            for ring in c:
                if ring:
                    xs, ys = zip(*ring)
                    ax.fill(xs, ys, color=color, lw=0.15, ec="white", alpha=0.85)
        elif t == "MultiPolygon":
            for poly in c:
                for ring in poly:
                    if ring:
                        xs, ys = zip(*ring)
                        ax.fill(xs, ys, color=color, lw=0.15, ec="white",
                                alpha=0.85)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("left", type=Path, help="PMTiles archive A (tylertoo)")
    ap.add_argument("right", type=Path, help="PMTiles archive B (tippecanoe)")
    ap.add_argument("--zooms", default="4,8,12",
                    help="comma-separated zooms to render (default 4,8,12)")
    ap.add_argument("--labels", default="tylertoo,tippecanoe")
    ap.add_argument("--out", type=Path, default=Path("renders"))
    args = ap.parse_args()

    left_label, right_label = args.labels.split(",", 1)
    args.out.mkdir(parents=True, exist_ok=True)
    summary = {}

    for z in [int(s) for s in args.zooms.split(",")]:
        lg, lids, ltiles = decode_zoom(args.left, z)
        rg, rids, rtiles = decode_zoom(args.right, z)
        summary[z] = {left_label: {"features": lids, "tiles": ltiles},
                      right_label: {"features": rids, "tiles": rtiles}}

        fig, axes = plt.subplots(1, 2, figsize=(14, 7))
        for ax, geoms, label, ids, tiles, color in (
                (axes[0], lg, left_label, lids, ltiles, "#1f6feb"),
                (axes[1], rg, right_label, rids, rtiles, "#c9510c")):
            plot(ax, geoms, color)
            ax.set_title(f"{label} · z{z} · {ids:,} features · {tiles:,} tiles",
                         fontsize=10)
            ax.set_aspect("equal")
            ax.set_xticks([])
            ax.set_yticks([])
        # share the same window so the panels are visually comparable
        xl = [a.get_xlim() for a in axes]
        yl = [a.get_ylim() for a in axes]
        for a in axes:
            a.set_xlim(min(v[0] for v in xl), max(v[1] for v in xl))
            a.set_ylim(min(v[0] for v in yl), max(v[1] for v in yl))
        fig.tight_layout()
        png = args.out / f"z{z:02d}.png"
        fig.savefig(png, dpi=130)
        plt.close(fig)
        print(f"z{z}: {left_label} {lids:,} feats/{ltiles} tiles · "
              f"{right_label} {rids:,} feats/{rtiles} tiles -> {png}")

    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
