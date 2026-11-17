# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pmtiles>=3.4", "mapbox-vector-tile>=2.1", "shapely>=2.0", "rasterio>=1.4",
#   "pyarrow>=17", "numpy>=1.26",
# ]
# [tool.uv]
# exclude-newer = "2026-10-22T00:00:00Z"
# ///
"""
shared_borders.py — gaps and overlaps along shared polygon borders (#802).

Scores tiles with vector geometry, not rasters, so slivers far narrower than
a pixel still count. Per tile, in tile units, against the source polygons
clipped to the tile (decoded with mapbox-vector-tile and measured with
shapely, never with tylertoo's own code):

  drawn           source area some feature draws / source area
  missing         source area no feature draws / source area
  missing_inner   the part of `missing` further than --band tile units from
                  the source's outline: holes inside the coverage, which are
                  slivers along shared borders (and any polygon not drawn)
  overlap         area drawn by two or more features / source area
  overlap_new     area drawn by two or more features where no two source
                  polygons overlap / source area: the overlapping slivers
                  (NZ parcels stack unit titles, so `overlap` alone is not)

Each is reported per tile and as the mean over the tiles. `run` tiles the
#802 datasets with a tylertoo binary (convert and export timed apart, with
GNU time) and optionally with tippecanoe, then scores them:

    uv run benchmarks/coalesce/shared_borders.py run --bin target/release/tylertoo \\
        --work ~/bench-data/802 \\
        --nz ~/bench-data/810/inputs/nz-parcels-hilbert.parquet \\
        --madagascar ~/bench-data/810/inputs/madagascar-adm4-hilbert.parquet \\
        --settings default,sf0.25 --tippecanoe --json out.json

    uv run benchmarks/coalesce/shared_borders.py score out.pmtiles \\
        --source src.parquet --tiles 12/4035/2500,12/4036/2499

NZ is the Auckland subset of #745 (`--bbox 174.5,-37.1,175.0,-36.6`, about
400k parcels), scored on its three densest z12 tiles. Madagascar adm4 is
scored on its five densest source tiles at each of z4–z8.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[0] / "visual"))
sys.path.insert(0, str(HERE))
from metrics import (
    EXTENT_DEFAULT,
    POLYGON,
    WORLD,
    _in_tile_units,
    features,
    read_source,
    read_tile,
    tile_bounds_3857,
)
from sweep_simplify import subset, timed

AUCKLAND_TILES = "12/4035/2500,12/4036/2499,12/4036/2500"
DATASETS = {
    "nz": {"bbox": [174.5, -37.1, 175.0, -36.6], "max_zoom": 14, "tiles": AUCKLAND_TILES,
           "caps": ["--max-tile-size", "0", "--max-compressed-tile-size", "2000000"],
           "tippecanoe": ["-Z0", "-z14", "-b", "8", "-l", "parcels", "-M", "2000000",
                          "--coalesce-densest-as-needed"]},
    "madagascar": {"max_zoom": 12, "zooms": [4, 5, 6, 7, 8], "densest": 5, "caps": [],
                   "tippecanoe": ["-Z0", "-z12", "-b", "8", "-l", "adm4", "-M", "500000",
                                  "--coalesce-densest-as-needed"]},
}
SETTINGS = {"default": [], "sf0.5": ["--simplify-factor", "0.5"],
            "sf0.25": ["--simplify-factor", "0.25"], "sf0": ["--simplify-factor", "0"]}
TIPPECANOE = {"tippecanoe": [], "tippecanoe-dsb": ["--detect-shared-borders"],
              "tippecanoe-pn": ["--no-simplification-of-shared-nodes"]}
KEYS = ("drawn", "missing", "missing_inner", "overlap", "overlap_new")


def densest(src, zoom: int, n: int) -> list[tuple[int, int, int]]:
    """The `n` tiles at `zoom` holding the most source vertices."""
    import shapely

    c = shapely.get_coordinates(src.geoms)
    k = 1 << zoom
    xs = np.clip(((c[:, 0] + WORLD / 2) / WORLD * k).astype(np.int64), 0, k - 1)
    ys = np.clip(((WORLD / 2 - c[:, 1]) / WORLD * k).astype(np.int64), 0, k - 1)
    cells, counts = np.unique(np.column_stack([xs, ys]), axis=0, return_counts=True)
    return [(zoom, int(x), int(y)) for x, y in cells[np.argsort(-counts)[:n]]]


def stacked(geoms: list):
    """Where two or more of `geoms` overlap (with positive area)."""
    import shapely

    g = np.array(geoms, dtype=object)
    i, j = shapely.STRtree(g).query(g, predicate="intersects")
    keep = i < j
    inter = shapely.intersection(g[i[keep]], g[j[keep]])
    return shapely.union_all([p for p in inter if p.area > 0])


def score_tile(src, decoded: dict, z: int, x: int, y: int, band: float) -> dict | None:
    import shapely
    from shapely.geometry import box

    # Both in tile units at the decoded layers' extent, y down.
    extent = next((layer.get("extent", EXTENT_DEFAULT) for layer in decoded.values()),
                  EXTENT_DEFAULT)
    tile_box = box(0, 0, extent, extent)
    idx = src.tree.query(box(*tile_bounds_3857(z, x, y)))
    parts = [shapely.make_valid(_in_tile_units(g, z, x, y, extent)) for g in src.geoms[idx]]
    parts = [p for p in shapely.intersection(parts, tile_box) if p.area > 0]
    # The same for every archive scored against this source.
    key = ("stacked", z, x, y, extent)
    if key not in src.rasters:
        src.rasters[key] = stacked(parts) if parts else shapely.Polygon()
    src_stacked = src.rasters[key]
    if not parts:
        return None
    source = shapely.union_all(parts)
    area = source.area
    drawn = [shapely.make_valid(g).intersection(tile_box) for g, _, _ in features(decoded, POLYGON)]
    drawn = [g for g in drawn if g.area > 0]
    union = shapely.union_all(drawn) if drawn else shapely.Polygon()
    gaps = source.difference(union)
    # The outline less the tile edge: the coverage's own edge, where outline
    # simplification moves the border with no neighbour involved.
    outline = source.boundary.difference(tile_box.exterior.buffer(1e-6)).buffer(band)
    return {
        "drawn": source.intersection(union).area / area,
        "missing": gaps.area / area,
        "missing_inner": gaps.difference(outline).area / area,
        "overlap": (sum(g.area for g in drawn) - union.area) / area,
        # Sources overlap themselves (NZ parcels stack unit titles): only
        # overlap where the source has none is the tiler's.
        "overlap_new": stacked(drawn).difference(src_stacked).area / area,
        "source_cover": area / (extent * extent),
    }


def score(archive: Path, src, tiles: list, band: float) -> dict:
    per_tile = {}
    for z, x, y in tiles:
        r = score_tile(src, read_tile(archive, z, x, y), z, x, y, band)
        if r is not None:
            per_tile[f"{z}/{x}/{y}"] = r
    by_zoom: dict = {}
    for k, r in per_tile.items():
        by_zoom.setdefault(k.split("/")[0], []).append(r)
    mean = {z: {m: float(np.mean([r[m] for r in rs])) for m in KEYS} for z, rs in by_zoom.items()}
    return {"archive": str(archive), "tiles": per_tile, "mean": mean}


def parse_tiles(s: str) -> list[tuple[int, int, int]]:
    return [tuple(int(v) for v in t.split("/")) for t in s.split(",") if t]


def tiles_for(name: str, src) -> list:
    ds = DATASETS[name]
    if "tiles" in ds:
        return parse_tiles(ds["tiles"])
    return [t for z in ds["zooms"] for t in densest(src, z, ds["densest"])]


def build_tylertoo(binary: Path, source: Path, out: Path, ds: dict, flags: list[str]) -> dict:
    ov, pm = out.with_suffix(".overview.parquet"), out
    conv = timed([str(binary), "overview", str(source), str(ov), "--min-zoom", "0",
                  "--max-zoom", str(ds["max_zoom"]), "--force", *flags])
    exp = timed([str(binary), "export-pmtiles", str(ov), str(pm), "--force", *ds["caps"]])
    ov.unlink()
    return {"convert": conv, "export": exp, "pmtiles_bytes": pm.stat().st_size}


def build_tippecanoe(binary: str, fgb: Path, out: Path, ds: dict, flags: list[str]) -> dict:
    scratch = out.parent / "tmp"
    scratch.mkdir(exist_ok=True)
    run = timed([binary, "-o", str(out), "--force", "--quiet", "-t", str(scratch),
                 *ds["tippecanoe"], *flags, str(fgb)])
    return {"tile": run, "pmtiles_bytes": out.stat().st_size}


def cmd_run(args) -> int:
    work = args.work.expanduser()
    work.mkdir(parents=True, exist_ok=True)
    report = {"bin": str(args.bin), "band": args.band, "datasets": {}}
    for name, ds in DATASETS.items():
        path = getattr(args, name)
        if path is None:
            continue
        source = path.expanduser()
        if "bbox" in ds:
            sub = work / f"{name}-subset.parquet"
            if not sub.exists():
                subset(source, ds["bbox"], sub)
            source = sub
        src = read_source(source, types=POLYGON)
        tiles = tiles_for(name, src)
        cells = {}
        for setting in args.settings.split(","):
            pm = work / f"{name}-{setting}.pmtiles"
            cells[setting] = ({} if args.score_only
                              else build_tylertoo(args.bin, source, pm, ds, SETTINGS[setting]))
            cells[setting]["score"] = score(pm, src, tiles, args.band)
            print(name, setting, json.dumps(cells[setting]["score"]["mean"]), flush=True)
        if args.tippecanoe:
            fgb = work / f"{name}.fgb"
            if not fgb.exists():
                subprocess.run(["uvx", "--from", "geoparquet-io==1.7.0", "gpio", "convert",
                                "flatgeobuf", str(source), str(fgb)], check=True)
            for key, flags in TIPPECANOE.items():
                pm = work / f"{name}-{key}.pmtiles"
                if not pm.exists() or args.rerun_tippecanoe:
                    cells[key] = build_tippecanoe(args.tippecanoe_bin, fgb, pm, ds, flags)
                else:
                    cells[key] = {"cached": True, "pmtiles_bytes": pm.stat().st_size}
                cells[key]["score"] = score(pm, src, tiles, args.band)
                print(name, key, json.dumps(cells[key]["score"]["mean"]), flush=True)
        report["datasets"][name] = {"source": str(source), "tiles": [f"{z}/{x}/{y}" for z, x, y in tiles],
                                    "cells": cells}
    print_table(report)
    if args.json:
        args.json.write_text(json.dumps(report, indent=2))
    return 0


def print_table(report: dict) -> None:
    for name, d in report["datasets"].items():
        print(f"\n{name}: mean over {len(d['tiles'])} tiles, % of source area per zoom")
        print("| cell | zoom | " + " | ".join(KEYS) + " | convert s / GiB | export s / GiB |")
        print("|---" * (len(KEYS) + 4) + "|")
        for cell, c in d["cells"].items():
            def t(stage, c=c):
                r = c.get(stage)
                return f"{r['wall_s']} / {r['peak_rss_kib'] / 1048576:.2f}" if r else "-"
            for z, m in sorted(c["score"]["mean"].items(), key=lambda kv: int(kv[0])):
                vals = " | ".join(f"{100 * m[k]:.3f}" for k in KEYS)
                conv = t("convert") if "convert" in c else t("tile")
                print(f"| {cell} | {z} | {vals} | {conv} | {t('export')} |")


def cmd_score(args) -> int:
    src = read_source(args.source, types=POLYGON)
    tiles = parse_tiles(args.tiles) if args.tiles else []
    for z in [int(v) for v in args.zooms.split(",")] if args.zooms else []:
        tiles += densest(src, z, args.densest)
    out = score(args.archive, src, tiles, args.band)
    print(json.dumps(out["mean"], indent=2))
    if args.json:
        args.json.write_text(json.dumps(out, indent=2))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--band", type=float, default=16.0,
                    help="tile units either side of a shared border counted as on it")
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run", help="tile the #802 datasets, then score them")
    r.add_argument("--bin", type=Path, required=True, help="tylertoo binary")
    r.add_argument("--work", type=Path, required=True)
    r.add_argument("--nz", type=Path, help="NZ parcels (Hilbert-sorted, bench-coalesce-v1)")
    r.add_argument("--madagascar", type=Path, help="Madagascar adm4 (bench-coalesce-v1)")
    r.add_argument("--settings", default="default,sf0.25", help=f"of {', '.join(SETTINGS)}")
    r.add_argument("--tippecanoe", action="store_true", help="also tile with tippecanoe (cached)")
    r.add_argument("--tippecanoe-bin", default="tippecanoe")
    r.add_argument("--rerun-tippecanoe", action="store_true")
    r.add_argument("--score-only", action="store_true",
                   help="rescore the archives already in --work, building nothing")
    r.add_argument("--json", type=Path)
    s = sub.add_parser("score", help="score one archive")
    s.add_argument("archive", type=Path)
    s.add_argument("--source", type=Path, required=True)
    s.add_argument("--tiles", default="", help="z/x/y,...")
    s.add_argument("--zooms", default="", help="add the densest source tiles at these zooms")
    s.add_argument("--densest", type=int, default=5)
    s.add_argument("--json", type=Path)
    args = ap.parse_args()
    return cmd_run(args) if args.cmd == "run" else cmd_score(args)


if __name__ == "__main__":
    sys.exit(main())
