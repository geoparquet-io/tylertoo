# /// script
# requires-python = ">=3.12,<3.14"
# dependencies = [
#   "pmtiles==3.8.1", "mapbox-vector-tile==2.2.0", "pyarrow==21.0.0", "shapely==2.1.1",
#   "numpy==2.5.4", "rasterio==1.5.2", "pillow==13.0.0",
# ]
# ///
"""
gate.py — do tylertoo's tiles look like tippecanoe's? (#869)

For each dataset it tiles the source with `tylertoo tiles --preset tippecanoe`
and with the pinned tippecanoe, and compares them on the same sampled tiles
(metrics.py) in two ways:

  * directly, tile against tile: the pair_* cells (1 - IoU of the drawn
    polygon area, line length apart, points unmatched; 0 = the same), and how
    many features and tile bytes each tool keeps;
  * each against the source: d = tylertoo's error - tippecanoe's error, on
    the worst tiles of each zoom (max, p99, p95).

Each number but the two kept ratios is held to its band in
visual_tolerances.toml; the ratios are reported only. After a dataset
the run prints its distance to tippecanoe and what tylertoo keeps over
tippecanoe; --json holds both under `distance`.

    uv run benchmarks/visual/gate.py                    # every dataset
    uv run benchmarks/visual/gate.py --datasets open-buildings --zooms 12,14
    uv run benchmarks/visual/gate.py --dataset mine=/data/mine.parquet
    uv run benchmarks/visual/gate.py --self-test        # the gate's own test
    uv run benchmarks/visual/gate.py --tylertoo-flags='--preset tippecanoe --simplify-factor 4'

With --tylertoo-flags or a --cap other than the default the run measures
and reports only: the bands are for the preset at the default sample.

A breach prints one line with both values, d and the band; for a per-tile
metric also the worst tile and a PNG of it (source | tylertoo | tippecanoe;
red is source the tile does not draw, blue is drawn where the source has
nothing, orange a wrong attribute). The tippecanoe side is cached under
--work, so a re-run after a tylertoo change only tiles and scores tylertoo.
Runs may share --work: each keeps its tylertoo archives in a directory of
its own.

Exit 0: every band holds. 1: a breach, or the self-test failed. 2: setup
(a tool is missing or failed, an unknown dataset, an unusable source).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
# run_e2e.py owns the dataset registry and the pinned tippecanoe, and
# compare_tippecanoe.py the tippecanoe input and the band lookup: the count
# gate and this one must agree on all of them.
sys.path.insert(0, str(REPO / "benchmarks" / "e2e"))
import compare_tippecanoe as count_gate  # noqa: E402
import metrics as M  # noqa: E402
import mutate  # noqa: E402
import run_e2e  # noqa: E402

TOLERANCES = HERE / "visual_tolerances.toml"
DATASETS = {
    **run_e2e.DATASETS,
    # Not in run_e2e.DATASETS, which would put it in the count gate too.
    "vd-points": {"path": REPO / "tests" / "fixtures" / "variable-depth" / "points.parquet",
                  "layer": "points"},
}
# What `--preset tippecanoe` stands for: tippecanoe's defaults and the size
# valve the preset's `drop` matches (docs/guides/tippecanoe.md). No -P: parallel
# reading reorders features and the output stops being reproducible.
TIPPECANOE_FLAGS = ["--drop-densest-as-needed", "--quiet"]
STATS = {"max": 100, "p99": 99, "p95": 95}
CAP = 100  # tiles sampled per zoom; the bands were measured with it
# What two tiles must draw between them to be one of a zoom's worst tiles: with
# less, a single pixel decides the share. The pooled mean takes every tile.
PAIR_FLOOR = {"pair_iou_loss": M.COVER_FLOOR * M.PX * M.PX, "pair_attr": M.COVER_FLOOR * M.PX * M.PX,
              "pair_apart": M.MIN_LENGTH_PX, "pair_count": M.MIN_POINTS}
SEAM = ("edge_gap", "excess_change")


def digest(obj) -> str:
    return hashlib.sha256(json.dumps(obj, sort_keys=True, default=str).encode()).hexdigest()


def file_sha(path: Path) -> str:
    return hashlib.file_digest(path.open("rb"), "sha256").hexdigest()


def run(argv: list, log: Path) -> None:
    with log.open("w") as f:
        rc = subprocess.run([str(a) for a in argv], stdout=f, stderr=subprocess.STDOUT).returncode
    if rc:
        raise ValueError(f"exit {rc}: {' '.join(map(str, argv))}\n{log.read_text()[-2000:]}")


# --------------------------------------------------------------------------
# archives
# --------------------------------------------------------------------------


def tylertoo_archive(args, did: str, ds: dict, zr: list, extra=()) -> Path:
    dst = args.run_dir / f"{did}{''.join(extra)}.pmtiles"
    run([args.tylertoo, "tiles", ds["path"], dst, *args.tylertoo_flags, "--min-zoom", zr[0],
         "--max-zoom", zr[1], "--layer-name", ds["layer"], "--force", *extra], dst.with_suffix(".log"))
    return dst


def tippecanoe_dir(args, tip: dict, did: str, ds: dict, zr: list) -> Path:
    """The tippecanoe archive of a source: it depends on the source, the
    binary and the flags only, so it is made once and kept."""
    key = digest([file_sha(Path(ds["path"])), tip["version"], TIPPECANOE_FLAGS, zr, ds["layer"],
                  file_sha(Path(count_gate.__file__))])  # which writes tippecanoe's input
    d = args.work / "tippecanoe" / f"{did}-{key[:12]}"
    if not d.exists():
        # Built aside and moved into place whole, so a run beside this one
        # never reads half an archive.
        part = Path(tempfile.mkdtemp(prefix=f"{did}-", dir=args.run_dir))
        geojson = count_gate.to_geojsonseq(Path(ds["path"]), part, did)
        run([tip["binary"], "-o", part / "archive.pmtiles", "-f", "-Z", zr[0], "-z", zr[1], "-l", ds["layer"],
             *TIPPECANOE_FLAGS, geojson], part / "tippecanoe.log")
        geojson.unlink()
        d.parent.mkdir(parents=True, exist_ok=True)
        try:
            part.rename(d)
        except OSError:  # another run got there first, with the same archive
            shutil.rmtree(part)
            if not d.exists():
                raise
    return d


# --------------------------------------------------------------------------
# scoring
# --------------------------------------------------------------------------


def score(archive: Path, src: M.Source, attr: str | None, zooms: list, cap: int) -> dict:
    """{zoom: {"tiles": {"x/y": errors}, "seams": {...}}} for the sampled
    tiles that pass the source-cover floor."""
    tiles, gz = M.read_archive(archive)
    out = {}
    for z in zooms:
        seeds = M.sample_tiles(src, z, cap)
        evaluate = set(seeds)
        if src.kind == "polygon":  # a seam needs the right and the lower neighbour
            evaluate |= {n for (_, x, y) in seeds for n in ((z, x + 1, y), (z, x, y + 1))}
        per, colour, ref = {}, {}, {}
        for k in sorted(evaluate):
            decoded = M.decode(tiles[k], gz)[1] if k in tiles else {}
            if src.kind == "polygon":
                rasters = M.polygon_rasters(decoded, src, attr, *k)
                colour[k], ref[k] = rasters[0], rasters[1] > 0
                errors = M.polygon_errors(decoded, src, attr, *k, rasters=rasters)
            else:
                errors = M.ERRORS[src.kind](decoded, src, attr, *k)
            if errors is not None and k in seeds:
                per[f"{k[1]}/{k[2]}"] = errors
        out[str(z)] = {"tiles": per}
        if src.kind == "polygon":
            seam = M.seams(seeds, colour, ref, M.PX)
            # Without an attribute every feature has one colour: no change to measure.
            out[str(z)]["seams"] = {m: seam[m] for m in SEAM if attr or m == "edge_gap"}
    return out


def cached_score(d: Path, src: M.Source, attr, zooms: list, cap: int) -> dict:
    key = digest([zooms, cap, attr, file_sha(HERE / "metrics.py"), file_sha(Path(__file__))])
    f = d / f"scores-{key[:12]}.json"
    if not f.exists():
        part = f.with_suffix(f".{os.getpid()}.part")
        part.write_text(json.dumps(score(d / "archive.pmtiles", src, attr, zooms, cap)))
        part.rename(f)
    return json.loads(f.read_text())


def pair_score(ty: Path, tip: Path, kind: str, attr: str | None, scored: dict) -> dict:
    """tylertoo's tiles against tippecanoe's, directly, on the tiles `scored`
    names: {zoom: {"tiles": {"x/y": errors}, "kept": {...}}}. `kept` sums the
    features and the stored bytes of those tiles, per tool."""
    archives = [M.read_archive(p) for p in (ty, tip)]
    out = {}
    for z, rec in scored.items():
        per, weights, kept = {}, {}, {"features": [0, 0], "bytes": [0, 0]}
        for t in rec["tiles"]:
            k = (int(z), *(int(v) for v in t.split("/")))
            a, b = (M.decode(tiles[k], gz)[1] if k in tiles else {} for tiles, gz in archives)
            for i, (decoded, (tiles, _)) in enumerate(zip((a, b), archives)):
                kept["features"][i] += sum(len(layer["features"]) for layer in decoded.values())
                kept["bytes"][i] += len(tiles.get(k, b""))
            w = {}
            errors = M.pair_errors(kind, a, b, attr, w)
            if errors is not None:
                per[t], weights[t] = errors, w
        out[z] = {"tiles": per, "weights": weights, "kept": kept}
    return out


def pair_rows(did: str, pair: dict, tol: dict) -> list:
    """One row per zoom and pair metric. The value is itself the difference
    between the tools, so it is its own d; without a band it is reported only."""
    rows = []
    for z, rec in pair.items():
        weights = rec.get("weights", {})
        enough = {t: {m: v for m, v in e.items() if weights.get(t, {}).get(m, PAIR_FLOOR[m]) >= PAIR_FLOOR[m]}
                  for t, e in rec["tiles"].items()}
        for name, (value, worst, metric) in worst_tiles({"tiles": enough}).items():
            band = count_gate.zoom_band(tol, did, name, int(z))
            rows.append({"dataset": did, "zoom": int(z), "metric": name, "pair": value, "d": value,
                         "band": band, "ok": count_gate.in_band(value, band), "tile": f"{z}/{worst}",
                         "error": metric})
        for metric in sorted({m for e in rec["tiles"].values() for m in e}):
            got = [(e[metric], rec.get("weights", {}).get(t, {}).get(metric, 1))
                   for t, e in rec["tiles"].items() if e.get(metric) is not None]
            if got and sum(w for _, w in got):
                # The zoom's tiles pooled: each counts by how much is drawn in it, so a
                # sliver of ten pixels does not outweigh a full tile. What distance() sums up.
                mean = float(np.average([v for v, _ in got], weights=[w for _, w in got]))
                name = f"{metric}_mean"
                band = count_gate.zoom_band(tol, did, name, int(z))
                rows.append({"dataset": did, "zoom": int(z), "metric": name, "pair": mean, "d": mean, "band": band,
                             "ok": count_gate.in_band(mean, band), "tile": None, "error": metric})
        for what, (a, b) in rec.get("kept", {}).items():
            if b:  # what tylertoo keeps over what tippecanoe keeps, in the zoom's sampled tiles
                # Reported, never gated: neither direction is better, and gzip bytes vary by machine.
                rows.append({"dataset": did, "zoom": int(z), "metric": f"{what}_ratio", "tylertoo": a,
                             "tippecanoe": b, "ratio": a / b, "d": a / b, "band": None, "ok": None,
                             "tile": None})
    return rows


def distance(rows: list) -> dict:
    """{dataset: {pair metric: mean over its zooms of the mean tile difference}}:
    how far tylertoo's tiles are from tippecanoe's, 0 when they draw the same.
    The number to drive down when choosing settings."""
    acc, kept = {}, {}
    for r in rows:
        if "pair" in r and r["metric"].endswith("_mean"):
            acc.setdefault(r["dataset"], {}).setdefault(r["error"], []).append(r["pair"])
        if "ratio" in r:
            sums = kept.setdefault(r["dataset"], {}).setdefault(r["metric"], [0, 0])
            sums[0], sums[1] = sums[0] + r["tylertoo"], sums[1] + r["tippecanoe"]
    out = {d: {m: float(np.mean(v)) for m, v in sorted(ms.items())} for d, ms in acc.items()}
    for d, ms in kept.items():  # over all the zooms' sampled tiles: 1 when the tools keep the same amount
        out.setdefault(d, {}).update({m: a / b for m, (a, b) in sorted(ms.items())})
    return out


def worst_tiles(zrec: dict) -> dict:
    """{metric_stat: (value, worst tile)} for one zoom of one archive."""
    out = {}
    for metric in sorted({m for e in zrec["tiles"].values() for m in e}):
        vals = {t: e[metric] for t, e in zrec["tiles"].items() if e.get(metric) is not None}
        if vals:
            worst = max(vals, key=vals.get)
            for stat, q in STATS.items():
                out[f"{metric}_{stat}"] = (float(np.percentile(list(vals.values()), q)), worst, metric)
    for metric, v in zrec.get("seams", {}).items():
        if v is not None:
            out[metric] = (v, None, metric)
    return out


def compare(did: str, ty: dict, tip: dict, tol: dict) -> list:
    """One row per zoom and metric: both values, d, the band, and whether d is in it."""
    rows = []
    for z in ty:
        a, b = worst_tiles(ty[z]), worst_tiles(tip[z])
        for name in a:
            if name not in b:
                continue
            d = a[name][0] - b[name][0]
            band = count_gate.zoom_band(tol, did, name, int(z)) or tol["gate"].get("default_band")
            rows.append({"dataset": did, "zoom": int(z), "metric": name, "tylertoo": a[name][0],
                         "tippecanoe": b[name][0], "d": d, "band": band,
                         "ok": count_gate.in_band(d, band), "tile": a[name][1] and f"{z}/{a[name][1]}",
                         "error": a[name][2]})
        # A band whose metric one side no longer yields (nothing drawn to
        # compare) must not stop gating in silence.
        for entry in tol.get("datasets", {}).get(did, {}).get("zoom_bands", []):
            if entry["zooms"][0] <= int(z) <= entry["zooms"][1]:
                rows += [{"dataset": did, "zoom": int(z), "metric": name, "d": None, "band": band,
                          "ok": False, "tile": None}
                         for name, band in entry.items()
                         if name != "zooms" and not name.startswith("pair_") and not name.endswith("_ratio")
                         and not (name in a and name in b)]
    return rows


def worse(r: dict) -> bool:
    """Outside the band on the high side: tylertoo draws worse than the band
    allows. Outside on the low side it draws better, and the band is stale."""
    return r["ok"] is False and (r["d"] is None or r["d"] > r["band"][1])


def cluster_rows(args, did: str, ds: dict, src: M.Source, zr: list, zooms: list) -> list:
    """With --cluster every source point must be counted exactly once per zoom."""
    tiles, gz = M.read_archive(tylertoo_archive(args, did, ds, zr, extra=("--cluster",)))
    rows = []
    for z in zooms:
        total = M.cluster_sum(M.decode(data, gz)[1] for k, data in tiles.items() if k[0] == z)
        rows.append({"dataset": did, "zoom": z, "metric": "point_count_sum", "tylertoo": total,
                     "source": len(src.geoms), "d": abs(total - len(src.geoms)), "band": [0, 0],
                     "ok": total == len(src.geoms), "tile": None})
    return rows


# --------------------------------------------------------------------------
# worst-tile images
# --------------------------------------------------------------------------


def masks(decoded: dict, src: M.Source, attr, z: int, x: int, y: int):
    """(drawn, source, wrong attribute) pixel masks of one tile."""
    if src.kind == "polygon":
        drawn, ref = M.polygon_rasters(decoded, src, attr, z, x, y)
        wrong = (drawn > 0) & (ref > 0) & (drawn != ref) if src.colours else np.zeros_like(drawn, bool)
        return drawn > 0, ref > 0, wrong
    from affine import Affine
    from rasterio.features import rasterize
    from shapely.geometry import box

    def touched(shapes, transform):
        shapes = [(g, 1) for g in shapes if not g.is_empty]
        if not shapes:
            return np.zeros((M.PX, M.PX), bool)
        return rasterize(shapes, out_shape=(M.PX, M.PX), transform=transform, fill=0,
                         dtype="uint8", all_touched=True).astype(bool)

    feats = list(M.features(decoded, M.KINDS[src.kind]))
    s = (feats[0][2] if feats else M.EXTENT_DEFAULT) / M.PX
    b = M.tile_bounds_3857(z, x, y)
    w = (b[2] - b[0]) / M.PX
    drawn = touched([g for g, _, _ in feats], Affine(s, 0, 0, 0, s, 0))
    ref = touched([src.geoms[i] for i in src.tree.query(box(*b))], Affine(w, 0, b[0], 0, -w, b[3]))
    return drawn, ref, np.zeros_like(drawn)


def write_png(path: Path, src: M.Source, attr, tile: str, archives: list) -> None:
    from PIL import Image

    z, x, y = (int(v) for v in tile.split("/"))
    panels = []
    for archive in archives:
        drawn, ref, wrong = masks(M.read_tile(archive, z, x, y), src, attr, z, x, y)
        if not panels:
            img = np.full((M.PX, M.PX, 3), 255, np.uint8)
            img[ref] = (200, 200, 200)
            panels.append(img)
        img = np.full((M.PX, M.PX, 3), 255, np.uint8)
        img[drawn] = (200, 200, 200)
        img[wrong] = (240, 150, 0)      # drawn, with another feature's attribute
        img[ref & ~drawn] = (220, 30, 30)   # missing: source only
        img[drawn & ~ref] = (30, 80, 220)   # added: tile only
        panels.append(img)
    gap = np.full((M.PX, 4, 3), 0, np.uint8)
    row = np.concatenate([p for panel in panels for p in (panel, gap)][:-1], axis=1)
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(row).resize((row.shape[1] * 2, row.shape[0] * 2), Image.NEAREST).save(path)


# --------------------------------------------------------------------------
# report
# --------------------------------------------------------------------------


def line(r: dict) -> str:
    if r["d"] is None:
        return (f"{r['dataset']} z{r['zoom']} {r['metric']}: has the band {r['band']} but no value: "
                "one of the tools draws nothing this metric can compare")
    band = f"band [{r['band'][0]}, {r['band'][1]}]" if r["band"] else "no band"
    if "ratio" in r:
        values = f"tylertoo {r['tylertoo']} tippecanoe {r['tippecanoe']} ratio {r['ratio']:.3f}"
    elif "pair" in r:
        values = f"tylertoo vs tippecanoe {r['pair']:.4f}"
    else:
        other = f"tippecanoe {r['tippecanoe']:.4f}" if "tippecanoe" in r else f"source {r['source']}"
        values = f"tylertoo {r['tylertoo']:.4f} {other} d {r['d']:+.4f}"
    return (f"{r['dataset']} z{r['zoom']} {r['metric']}: {values} {band}"
            + (f" tile {r['tile']}" if r["tile"] else "")
            + (f" png {r['png']}" if r.get("png") else ""))


def gate_dataset(args, tol: dict, tip: dict, did: str, ds: dict) -> list:
    cfg = tol.get("datasets", {}).get(did, {})
    attr = cfg.get("color_attr")
    zr = tol["gate"]["zoom_range"]
    zooms = args.zooms or cfg.get("zooms") or tol["gate"]["zooms"]
    src = M.read_source(Path(ds["path"]), attr)
    tip_dir = tippecanoe_dir(args, tip, did, ds, zr)
    ty_archive = tylertoo_archive(args, did, ds, zr)
    ty_scores = score(ty_archive, src, attr, zooms, args.cap)
    rows = compare(did, ty_scores, cached_score(tip_dir, src, attr, zooms, args.cap), tol)
    # From the scored tiles, not the rows: a polygon's seam row comes without any tile over the floor.
    empty = [z for z in zooms if not ty_scores[str(z)]["tiles"]]
    # The direct comparison needs no source, so it takes every sampled tile, also
    # those under the source floor (small polygons at low zooms).
    sampled = {str(z): {"tiles": [f"{x}/{y}" for _, x, y in M.sample_tiles(src, z, args.cap)]} for z in zooms}
    pairs = pair_rows(did, pair_score(ty_archive, tip_dir / "archive.pmtiles", src.kind, attr, sampled), tol)
    preset = getattr(args, "preset", True)
    # No tile scored against the source and no direct cell banded (or, measuring
    # other settings, none measured): no verdict, not a pass.
    if empty == list(zooms) and not any("pair" in r and (r["ok"] is not None or not preset) for r in pairs):
        raise ValueError(f"{did}: nothing to score at zooms {zooms}: no sampled tile holds enough of the source")
    if empty:
        print(f"{did}: not scored against the source (no sampled tile holds enough of it): "
              + ", ".join(f"z{z}" for z in empty), flush=True)
    rows += pairs
    if src.kind == "point" and preset:  # the exact sum is the preset's promise
        rows += cluster_rows(args, did, ds, src, zr, zooms)
    written = set()  # max, p99 and p95 of an error share its worst tile: one PNG, new each run
    for r in rows:
        if worse(r) and r["tile"]:
            png = args.work / "png" / f"{did}_z{r['zoom']}_{r['error']}_{r['tile'].replace('/', '-')}.png"
            if png not in written:
                write_png(png, src, attr, r["tile"], [ty_archive, tip_dir / "archive.pmtiles"])
                written.add(png)
            r["png"] = str(png)
    return rows


def self_test(args, tol: dict, tip: dict) -> bool:
    """Every known defect must breach a band, and the unchanged copy must not."""
    cfg = tol["self_test"]
    did, zooms = cfg["dataset"], cfg["zooms"]
    ds = DATASETS[did]
    attr = tol["datasets"][did].get("color_attr")
    zr = tol["gate"]["zoom_range"]
    src = M.read_source(Path(ds["path"]), attr)
    tip_scores = cached_score(tippecanoe_dir(args, tip, did, ds, zr), src, attr, zooms, args.cap)
    original = tylertoo_archive(args, did, ds, zr)
    base = score(original, src, attr, zooms, args.cap)
    z = str(zooms[len(zooms) // 2])
    gated = sorted(base[z]["tiles"])
    empty = (int(z), *(int(v) for v in gated[len(gated) // 2].split("/")))
    ok = True
    for kind in mutate.MUTANTS:
        copy = args.run_dir / f"{did}.{kind}.pmtiles"
        mutate.mutate(original, copy, kind, zooms, attr, empty)
        rows = compare(did, score(copy, src, attr, zooms, args.cap), tip_scores, tol)
        caught = sorted(filter(worse, rows), key=lambda r: r["d"] is None or r["band"][1] - r["d"])
        failed = [r for r in rows if r["ok"] is False]
        if kind == "control":
            good = not failed
            print(f"self-test control: {'passes' if good else 'BREACHES ' + line(failed[0])}")
        else:
            good = bool(caught)
            print(f"self-test {kind}: " + (f"caught, {len(caught)} breaches, worst {line(caught[0])}"
                                           if good else "NOT CAUGHT"))
        ok &= good
        copy.unlink()
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--datasets", help="comma list of dataset ids (default: all)")
    ap.add_argument("--dataset", action="append", default=[], metavar="ID=PATH",
                    help="score another GeoParquet file (repeatable)")
    ap.add_argument("--zooms", help="comma list, instead of each dataset's zooms")
    ap.add_argument("--cap", type=int, default=CAP,
                    help="tiles sampled per zoom and dataset (default: %(default)s; 0: every tile that holds "
                    "source data). The bands were measured at the default, so another value only reports")
    ap.add_argument("--self-test", action="store_true",
                    help="inject known defects and check that each one breaches")
    ap.add_argument("--tylertoo", default=os.environ.get("TYLERTOO", str(REPO / "target" / "release" / "tylertoo")))
    ap.add_argument("--tylertoo-flags", type=shlex.split, default=["--preset", "tippecanoe"],
                    metavar="'FLAGS'", help="tylertoo tiles settings to compare with tippecanoe, in place of "
                    "--preset tippecanoe; the distances are reported and the bands, measured for the preset, "
                    "are not applied (write it as --tylertoo-flags='--flag value')")
    ap.add_argument("--tippecanoe", default=os.environ.get("TIPPECANOE"))
    ap.add_argument("--tolerances", type=Path, default=TOLERANCES)
    ap.add_argument("--work", type=Path, default=Path.home() / "bench-data" / "visual",
                    help="the tippecanoe cache and the PNGs (default: %(default)s)")
    ap.add_argument("--json", type=Path, help="write every row here")
    ap.add_argument("--verbose", action="store_true", help="print every row, not only breaches")
    args = ap.parse_args()
    args.zooms = [int(z) for z in args.zooms.split(",")] if args.zooms else None

    datasets = dict(DATASETS)
    for spec in args.dataset:
        did, _, path = spec.partition("=")
        datasets[did] = {"path": Path(path), "layer": did}
    only = args.datasets.split(",") if args.datasets else [d for d, _, _ in (s.partition("=") for s in args.dataset)]
    unknown = [d for d in only if d not in datasets]
    if unknown:
        print(f"error: unknown dataset(s) {', '.join(unknown)}; known: {', '.join(datasets)}", file=sys.stderr)
        return 2
    datasets = {d: datasets[d] for d in only} if only else datasets
    try:
        tip = run_e2e.resolve_tippecanoe(args.tippecanoe)
    except run_e2e.TippecanoeUnavailable as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    missing = [str(p) for p in [Path(args.tylertoo), *(Path(d["path"]) for d in datasets.values())] if not p.exists()]
    if missing:
        print(f"error: not found: {', '.join(missing)}\n  build with `cargo build --release --package tylertoo`; "
              "fixtures: gh release download fixtures-v1 --dir tests/fixtures/realdata/", file=sys.stderr)
        return 2
    tol = count_gate.load_tolerances(args.tolerances)
    args.preset = preset = args.tylertoo_flags == ["--preset", "tippecanoe"] and args.cap == CAP
    args.cap = args.cap or sys.maxsize
    if args.self_test and not preset:
        print("error: --self-test runs the preset at the default --cap; its bands are for nothing else",
              file=sys.stderr)
        return 2
    if not preset:
        tol["gate"].pop("default_band", None)
        for cfg in tol.get("datasets", {}).values():
            cfg.pop("zoom_bands", None)
    args.work.mkdir(parents=True, exist_ok=True)
    # This run's tylertoo archives: its own directory, so that runs sharing
    # --work (agents in several worktrees) never score each other's binary.
    with tempfile.TemporaryDirectory(prefix="run-", dir=args.work) as run_dir:
        args.run_dir = Path(run_dir)
        try:
            return gate(args, tol, tip, datasets)
        except (ValueError, OSError) as e:  # a tool failed, or a source or --work cannot be used
            print(f"error: {e}", file=sys.stderr)
            return 2


def gate(args, tol: dict, tip: dict, datasets: dict) -> int:
    if args.self_test:
        return 0 if self_test(args, tol, tip) else 1

    rows = []
    for did, ds in datasets.items():
        got = gate_dataset(args, tol, tip, did, ds)
        bad = [r for r in got if r["ok"] is False]
        print(f"{did}: {len(got)} cells, {len(bad)} outside their band", flush=True)
        for r in got if args.verbose else bad:
            tag = ("ok      " if r["ok"] else "BREACH  " if worse(r) else "IMPROVED" if r["ok"] is False
                   else "no band ")
            print(f"  {tag} {line(r)}", flush=True)
        dist = distance(got).get(did, {})
        print("  distance to tippecanoe (mean tile, 0 = same): "
              + ", ".join(f"{m} {v:.4f}" for m, v in dist.items() if not m.endswith("_ratio"))
              + (f"  [IoU {1 - dist['pair_iou_loss']:.4f}]" if "pair_iou_loss" in dist else ""), flush=True)
        if "features_ratio" in dist:
            print(f"  kept, tylertoo over tippecanoe (1 = same): features x{dist['features_ratio']:.2f}, "
                  f"tile bytes x{dist['bytes_ratio']:.2f}", flush=True)
        rows += got
    if args.json:
        args.json.write_text(json.dumps({"tippecanoe": tip, "tolerances": str(args.tolerances),
                                         "tylertoo_flags": args.tylertoo_flags, "distance": distance(rows),
                                         "rows": rows}, indent=2))
    bad = [r for r in rows if r["ok"] is False]
    if bad:
        if not all(map(worse, bad)):
            print("IMPROVED: d is below its band. Tighten the band in visual_tolerances.toml, "
                  "with the new measurement in its comment.")
        for did in sorted({r["dataset"] for r in bad}):
            which = (f"--datasets {did}" if datasets[did] is DATASETS.get(did)
                     else f"--dataset {did}={datasets[did]['path']}")
            zooms = ",".join(str(z) for z in sorted({r["zoom"] for r in bad if r["dataset"] == did}))
            print(f"re-run only this: uv run benchmarks/visual/gate.py {which} --zooms {zooms}")
        return 1
    print("visual gate: every band holds" if args.preset else
          f"measured with {' '.join(args.tylertoo_flags)}, {'all' if args.cap == sys.maxsize else args.cap} tiles per "
          f"zoom: the bands are for the preset at {CAP} tiles and were not applied")
    return 0


if __name__ == "__main__":
    sys.exit(main())
