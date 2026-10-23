# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pmtiles>=3.4", "mapbox-vector-tile>=2.1", "shapely>=2.0", "rasterio>=1.4",
#   "geopandas>=1.0", "pyarrow>=17", "numpy>=1.26", "pillow>=10",
# ]
# [tool.uv]
# exclude-newer = "2026-10-22T00:00:00Z"
# ///
"""
tile_metrics.py — what a PMTiles archive actually draws, per tile, against
its source data (#745).

For each sampled tile it rasterizes, at the same resolution and aligned to
the tile:

  * the source polygons that fall in the tile (the reference coverage), and
  * the decoded tile's polygons, each filled with a colour hashed from an
    attribute, in draw order (what a choropleth renderer paints).

and reports per zoom:

  drawn_share         drawn ∩ reference / reference, per tile with at least
                      1% reference cover: min, p1, p5, median, mean.
  edge_gap            the share of reference-covered pixels in a 2-pixel band
                      along each shared tile edge that neither tile draws: a
                      hole at a seam. Should be about 0.
  colour_change       the share of pixel pairs straddling a shared tile edge
                      whose colours differ, both sides drawn...
  control_change      ...and the same share across a control line inside the
                      tile at the same spacing. Neighbouring features differ
                      in colour anyway, and one feature can legitimately
                      absorb different neighbours in different tiles; the
                      control is that base rate.
  excess_change       colour_change − control_change: the colour changes the
                      seam itself causes.

It also scans every tile for its raw and gzip size and counts tiles over the
given limits, and can render the worst seams and named tiles as PNGs for a
person to look at (--render).

    uv run benchmarks/coalesce/tile_metrics.py out.pmtiles \\
        --source ~/bench-data/745/madagascar/hilbert.parquet \\
        --zooms 4,6,8,10,12 --color-attr adm2_name --json metrics.json
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import math
import random
import sys
from pathlib import Path

import numpy as np

WORLD = 20037508.342789244 * 2
EXTENT_DEFAULT = 4096


def tile_bounds_3857(z: int, x: int, y: int) -> tuple[float, float, float, float]:
    w = WORLD / (1 << z)
    x0 = -WORLD / 2 + x * w
    ytop = WORLD / 2 - y * w
    return x0, ytop - w, x0 + w, ytop


def colour_of(value) -> int:
    h = hashlib.blake2b(repr(value).encode(), digest_size=4).digest()
    return (int.from_bytes(h, "little") & 0x7FFFFFFF) or 1


def read_archive(path: Path):
    from pmtiles.reader import MmapSource, Reader, all_tiles

    f = open(path, "rb")
    src = MmapSource(f)
    reader = Reader(src)
    header = reader.header()
    gz = header.get("tile_compression") in (2, "gzip") or str(header.get("tile_compression")).endswith("GZIP")
    tiles = {}
    for (z, x, y), data in all_tiles(src):
        tiles[(z, x, y)] = data
    return tiles, gz


def decode(data: bytes, gz: bool):
    import mapbox_vector_tile

    raw = gzip.decompress(data) if gz else data
    return raw, mapbox_vector_tile.decode(raw, default_options={"y_coord_down": True})


def tile_shapes(decoded: dict, color_attr: str | None):
    from shapely.geometry import shape

    out = []
    extent = EXTENT_DEFAULT
    for layer in decoded.values():
        extent = layer.get("extent", extent)
        for f in layer["features"]:
            g = f["geometry"]
            if g["type"] not in ("Polygon", "MultiPolygon"):
                continue
            key = f["properties"].get(color_attr) if color_attr else None
            if key is None:
                key = f.get("id")
            out.append((shape(g), colour_of(key)))
    return out, extent


def rasterize_tile(shapes, extent: int, px: int) -> np.ndarray:
    from affine import Affine
    from rasterio.features import rasterize

    if not shapes:
        return np.zeros((px, px), dtype=np.int32)
    # Tile coordinates, y down: pixel (col, row) -> (col * s, row * s).
    s = extent / px
    return rasterize(shapes, out_shape=(px, px), transform=Affine(s, 0, 0, 0, s, 0),
                     fill=0, dtype="int32")


def rasterize_source(tree, geoms, z: int, x: int, y: int, px: int) -> np.ndarray:
    from affine import Affine
    from rasterio.features import rasterize
    from shapely.geometry import box

    b = tile_bounds_3857(z, x, y)
    idx = tree.query(box(*b))
    if len(idx) == 0:
        return np.zeros((px, px), dtype=bool)
    w = (b[2] - b[0]) / px
    arr = rasterize(((geoms[i], 1) for i in idx), out_shape=(px, px),
                    transform=Affine(w, 0, b[0], 0, -w, b[3]), fill=0, dtype="uint8")
    return arr.astype(bool)


def pct(vals, q):
    return float(np.percentile(vals, q)) if len(vals) else None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("pmtiles", type=Path)
    ap.add_argument("--source", type=Path, required=True)
    ap.add_argument("--zooms", required=True)
    ap.add_argument("--px", type=int, default=128, help="pixels per tile side")
    ap.add_argument("--color-attr")
    ap.add_argument("--max-tiles", type=int, default=150, help="seed tiles per zoom")
    ap.add_argument("--named", default="", help="z/x/y tiles always evaluated, comma list")
    ap.add_argument("--raw-limit", type=int, default=2097152)
    ap.add_argument("--gz-limit", type=int, default=512000)
    ap.add_argument("--render", type=Path, help="write PNGs of the worst seams and named tiles")
    ap.add_argument("--json", type=Path)
    args = ap.parse_args()

    import geopandas as gpd
    from shapely import STRtree

    src = gpd.read_parquet(args.source)
    if src.crs is None:
        src = src.set_crs(4326)
    src = src.to_crs(3857)
    src = src[src.geometry.geom_type.isin(["Polygon", "MultiPolygon"])]
    geoms = np.array(src.geometry.values)
    tree = STRtree(geoms)

    tiles, gz = read_archive(args.pmtiles)
    sizes = {"tiles": len(tiles), "max_raw": 0, "max_gz": 0, "over_raw": 0, "over_gz": 0}
    raw_cache: dict = {}
    for key, data in tiles.items():
        raw = gzip.decompress(data) if gz else data
        sizes["max_raw"] = max(sizes["max_raw"], len(raw))
        sizes["max_gz"] = max(sizes["max_gz"], len(data))
        sizes["over_raw"] += args.raw_limit > 0 and len(raw) > args.raw_limit
        sizes["over_gz"] += args.gz_limit > 0 and len(data) > args.gz_limit

    named = {tuple(int(v) for v in t.split("/")) for t in args.named.split(",") if t}
    zooms = [int(z) for z in args.zooms.split(",")]
    rng = random.Random(745)
    report = {"pmtiles": str(args.pmtiles), "source": str(args.source), "px": args.px,
              "sizes": sizes, "zooms": {}, "named": {}}
    renders = []

    for z in zooms:
        at_z = [k for k in tiles if k[0] == z]
        if not at_z:
            continue
        # Seeds: the biggest tiles (where the valve fires) and a random
        # sample; each seed brings its right and lower neighbours so seams
        # can be measured.
        by_size = sorted(at_z, key=lambda k: len(tiles[k]), reverse=True)
        seeds = set(by_size[: args.max_tiles // 2])
        rest = [k for k in at_z if k not in seeds]
        seeds |= set(rng.sample(rest, min(len(rest), args.max_tiles - len(seeds))))
        seeds |= {k for k in named if k[0] == z and k in tiles}
        evaluate = set(seeds)
        for (_, x, y) in seeds:
            for n in ((z, x + 1, y), (z, x, y + 1)):
                if n in tiles:
                    evaluate.add(n)

        colour: dict = {}
        ref: dict = {}
        shares = []
        for k in sorted(evaluate):
            _, dec = decode(tiles[k], gz)
            shapes, extent = tile_shapes(dec, args.color_attr)
            colour[k] = rasterize_tile(shapes, extent, args.px)
            ref[k] = rasterize_source(tree, geoms, *k, args.px)
            r = ref[k].sum()
            if r >= 0.01 * args.px * args.px:
                share = float(((colour[k] > 0) & ref[k]).sum() / r)
                shares.append(share)
                if k in named:
                    report["named"]["/".join(map(str, k))] = share

        edge_gap_num = edge_gap_den = 0
        diff_edge = both_edge = diff_ctl = both_ctl = 0
        worst = []
        mid = args.px // 2
        for (zz, x, y) in seeds:
            a = (zz, x, y)
            for b, axis in (((zz, x + 1, y), 1), ((zz, x, y + 1), 0)):
                if b not in colour:
                    continue
                ca, cb = colour[a], colour[b]
                ra, rb = ref[a], ref[b]
                if axis == 1:  # a | b, vertical seam
                    ea, eb = ca[:, -1], cb[:, 0]
                    band_c = np.concatenate([ca[:, -2:], cb[:, :2]], axis=1)
                    band_r = np.concatenate([ra[:, -2:], rb[:, :2]], axis=1)
                    ctl_a, ctl_b = ca[:, mid - 1], ca[:, mid]
                else:  # a over b, horizontal seam
                    ea, eb = ca[-1, :], cb[0, :]
                    band_c = np.concatenate([ca[-2:, :], cb[:2, :]], axis=0)
                    band_r = np.concatenate([ra[-2:, :], rb[:2, :]], axis=0)
                    ctl_a, ctl_b = ca[mid - 1, :], ca[mid, :]
                edge_gap_num += int((band_r & (band_c == 0)).sum())
                edge_gap_den += int(band_r.sum())
                both = (ea > 0) & (eb > 0)
                d = int((both & (ea != eb)).sum())
                diff_edge += d
                both_edge += int(both.sum())
                bc = (ctl_a > 0) & (ctl_b > 0)
                diff_ctl += int((bc & (ctl_a != ctl_b)).sum())
                both_ctl += int(bc.sum())
                if both.sum():
                    worst.append((d / both.sum(), a, b))

        cc = diff_edge / both_edge if both_edge else None
        ctl = diff_ctl / both_ctl if both_ctl else None
        report["zooms"][str(z)] = {
            "tiles_total": len(at_z),
            "tiles_evaluated": len(evaluate),
            "drawn_share": {
                "n": len(shares), "min": min(shares) if shares else None,
                "p1": pct(shares, 1), "p5": pct(shares, 5), "median": pct(shares, 50),
                "mean": float(np.mean(shares)) if shares else None,
            },
            "edge_gap": edge_gap_num / edge_gap_den if edge_gap_den else None,
            "colour_change": cc,
            "control_change": ctl,
            "excess_change": cc - ctl if cc is not None and ctl is not None else None,
        }
        worst.sort(reverse=True)
        renders += [(z, a, b, colour) for _, a, b in worst[:3]]
        print(f"z{z}: {json.dumps(report['zooms'][str(z)])}", flush=True)

    if args.render:
        args.render.mkdir(parents=True, exist_ok=True)
        from PIL import Image

        def rgb(arr):
            img = np.zeros(arr.shape + (3,), dtype=np.uint8)
            m = arr > 0
            v = arr[m].astype(np.uint64)
            img[m] = np.stack([(v * 2654435761) >> 8 & 255, (v * 40503) >> 4 & 255,
                               (v * 2246822519) >> 12 & 255], axis=1).astype(np.uint8)
            return img

        for z, a, b, colour in renders:
            ia, ib = rgb(colour[a]), rgb(colour[b])
            pair = np.concatenate([ia, ib], axis=1 if b[1] != a[1] else 0)
            name = f"seam_z{z}_{a[1]}_{a[2]}__{b[1]}_{b[2]}.png"
            Image.fromarray(pair).resize((pair.shape[1] * 2, pair.shape[0] * 2), Image.NEAREST
                                         ).save(args.render / name)

    if args.json:
        args.json.write_text(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
