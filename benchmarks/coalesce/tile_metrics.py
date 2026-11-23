# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pmtiles>=3.4", "mapbox-vector-tile>=2.1", "shapely>=2.0", "rasterio>=1.4",
#   "pyarrow>=17", "numpy>=1.26", "pillow>=10",
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
  density_step        over every pair of side-adjacent tiles that both hold at
                      least --step-min source features: |ln(kept_a / kept_b)|,
                      where a tile's kept share is the features it draws over
                      the source features whose bbox meets it. p50, p95, max,
                      and the worst pair. A step the merge valve leaves at a
                      tile edge (#768); 0 when both tiles keep the same share.

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
import json
import random
import sys
from pathlib import Path

import numpy as np

# benchmarks/visual/metrics.py owns the reading, decoding and rasterizing (#869).
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "visual"))
from metrics import (POLYGON, decode, pct, rasterize_source, rasterize_tile, read_archive,
                     read_source, seams, tile_shapes)  # noqa: E402


def _varint(b: bytes, i: int) -> tuple[int, int]:
    v = shift = 0
    while True:
        c = b[i]
        i += 1
        v |= (c & 0x7F) << shift
        if c < 0x80:
            return v, i
        shift += 7


def _skip(b: bytes, i: int, wire: int) -> int:
    if wire == 0:
        return _varint(b, i)[1]
    if wire == 2:
        n, i = _varint(b, i)
        return i + n
    return i + (8 if wire == 1 else 4)


def feature_count(raw: bytes) -> int:
    """MVT features in a decoded tile, every layer, read from the protobuf
    framing alone (a full decode of every tile at a zoom is minutes)."""
    n, i = 0, 0
    while i < len(raw):
        key, i = _varint(raw, i)
        if key >> 3 != 3 or key & 7 != 2:  # Tile.layers
            i = _skip(raw, i, key & 7)
            continue
        size, i = _varint(raw, i)
        j, end = i, i + size
        while j < end:
            k, j = _varint(raw, j)
            n += k >> 3 == 2 and k & 7 == 2  # Layer.features
            j = _skip(raw, j, k & 7)
        i = end
    return n


def source_counts(path: Path, zooms: list) -> dict:
    """{(z, x, y): source features whose bbox meets the tile}, from the
    bbox covering column alone."""
    import pyarrow.compute as pc
    import pyarrow.parquet as pq

    from metrics import WORLD

    def mercator(lng, lat):
        lat = np.radians(np.clip(lat, -85.05, 85.05))
        return lng * (WORLD / 360), np.log(np.tan(np.pi / 4 + lat / 2)) * (WORLD / (2 * np.pi))

    counts: dict = {}
    for batch in pq.ParquetFile(path).iter_batches(batch_size=1 << 20, columns=["bbox"]):
        bb = batch.column("bbox")
        x0, y0, x1, y1 = (pc.struct_field(bb, k).to_numpy(zero_copy_only=False) for k in
                          ("xmin", "ymin", "xmax", "ymax"))
        ok = (x1 - x0) < 180  # an antimeridian-crossing bbox would count in every column
        x0, y0, x1, y1 = x0[ok], y0[ok], x1[ok], y1[ok]
        (wx, ny), (ex, sy) = mercator(x0, y1), mercator(x1, y0)
        for z in zooms:
            n = 1 << z
            col = lambda m: np.clip(np.floor((m + WORLD / 2) / WORLD * n), 0, n - 1).astype(np.int64)  # noqa: E731
            row = lambda m: np.clip(np.floor((WORLD / 2 - m) / WORLD * n), 0, n - 1).astype(np.int64)  # noqa: E731
            ax0, ax1, ay0, ay1 = col(wx), col(ex), row(ny), row(sy)
            one = (ax0 == ax1) & (ay0 == ay1)
            keys, c = np.unique(np.column_stack([ax0[one], ay0[one]]), axis=0, return_counts=True)
            for (x, y), k in zip(keys.tolist(), c.tolist()):
                counts[(z, x, y)] = counts.get((z, x, y), 0) + k
            for xa, xb, ya, yb in zip(*(a[~one].tolist() for a in (ax0, ax1, ay0, ay1))):
                for x in range(xa, xb + 1):
                    for y in range(ya, yb + 1):
                        counts[(z, x, y)] = counts.get((z, x, y), 0) + 1
    return counts


def density_steps(tiles: dict, gz: bool, src: dict, z: int, min_src: int) -> dict:
    """|ln(kept_a / kept_b)| over side-adjacent tile pairs at zoom z."""
    kept = {}
    for k, data in tiles.items():
        if k[0] == z and src.get(k, 0) >= min_src:
            kept[k] = feature_count(gzip.decompress(data) if gz else data) / src[k]
    steps = []
    for (_, x, y), ka in kept.items():
        for b in ((z, x + 1, y), (z, x, y + 1)):
            if b in kept and ka > 0 and kept[b] > 0:
                steps.append((abs(float(np.log(ka / kept[b]))), (z, x, y), b))
    vals = [s for s, _, _ in steps]
    worst = max(steps, default=None)
    return {"pairs": len(steps), "p50": pct(vals, 50), "p95": pct(vals, 95),
            "max": worst[0] if worst else None,
            "worst": worst and ["/".join(map(str, worst[1])), "/".join(map(str, worst[2]))],
            "kept": {"/".join(map(str, k)): round(v, 4) for k, v in kept.items()}}


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
    ap.add_argument("--step-min", type=int, default=100,
                    help="source features a tile needs to count in density_step")
    args = ap.parse_args()

    src = read_source(args.source, types=POLYGON)
    geoms, tree = src.geoms, src.tree

    tiles, gz = read_archive(args.pmtiles)
    sizes = {"tiles": len(tiles), "max_raw": 0, "max_gz": 0, "over_raw": 0, "over_gz": 0}
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

    src_counts = source_counts(args.source, zooms)
    for z in zooms:
        at_z = [k for k in tiles if k[0] == z]
        if not at_z:
            continue
        step = density_steps(tiles, gz, src_counts, z, args.step_min)
        report.setdefault("kept", {}).update(step.pop("kept"))
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

        seam = seams(seeds, colour, ref, args.px)
        worst = seam.pop("worst")
        report["zooms"][str(z)] = {
            "tiles_total": len(at_z),
            "tiles_evaluated": len(evaluate),
            "drawn_share": {
                "n": len(shares), "min": min(shares) if shares else None,
                "p1": pct(shares, 1), "p5": pct(shares, 5), "median": pct(shares, 50),
                "mean": float(np.mean(shares)) if shares else None,
            },
            **seam,
            "density_step": step,
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
