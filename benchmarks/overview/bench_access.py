#!/usr/bin/env python3
"""Bytes-per-viewport access benchmark (the headline).

For each (dataset x viewport) measures, over a byte-range HTTP server that
logs every response's byte count:

  overview path  -- a fresh DuckDB process runs the documented read
                    protocol: WHERE level = <k> AND <bbox overlap>, reading
                    only the pruned level band's row groups over httpfs.
  pmtiles path   -- the tippecanoe golden: resolve the z/x/y tiles covering
                    the viewport at the target zoom, range-fetch each tile
                    (plus header + directory) via the python pmtiles reader.

Metrics per path: bytes_fetched, request_count, wall_time_ms (median of N
cold runs -- fresh client process/reader each run), features_returned
(overview only; MVT features are clipped/quantized and not comparable).

Same viewport rectangle + zoom for both paths (from viewports.json).

Run:  uv run --with pmtiles python3 bench_access.py
"""
import json
import math
import os
import statistics
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
DATA_ROOT = os.path.join(ROOT, "corpus", "data")
PORT = int(os.environ.get("BENCH_PORT", "8899"))
BASE = f"http://127.0.0.1:{PORT}"
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))

DATASETS = [
    "points-nyc-medium",
    "lines-portland-medium",
    "polygons-portland-medium",
    "polygons-ftw-moldova-large",
]


# ---- server control -------------------------------------------------
def server_reset():
    urllib.request.urlopen(f"{BASE}/__reset").read()


def server_stats():
    return json.loads(urllib.request.urlopen(f"{BASE}/__stats").read())


def range_get(url, offset, length):
    req = urllib.request.Request(
        url, headers={"Range": f"bytes={offset}-{offset + length - 1}"}
    )
    with urllib.request.urlopen(req) as r:
        return r.read()


# ---- slippy tile math ----------------------------------------------
def lonlat_to_tile(lon, lat, z):
    n = 2 ** z
    x = int((lon + 180.0) / 360.0 * n)
    lat = max(min(lat, 85.05112878), -85.05112878)
    lat_rad = math.radians(lat)
    y = int((1.0 - math.asinh(math.tan(lat_rad)) / math.pi) / 2.0 * n)
    return min(max(x, 0), n - 1), min(max(y, 0), n - 1)


def tiles_for_bbox(bbox, z):
    xmin, ymin, xmax, ymax = bbox
    x0, y1 = lonlat_to_tile(xmin, ymin, z)  # note y flips
    x1, y0 = lonlat_to_tile(xmax, ymax, z)
    out = []
    for x in range(min(x0, x1), max(x0, x1) + 1):
        for y in range(min(y0, y1), max(y0, y1) + 1):
            out.append((z, x, y))
    return out


# ---- overview path (DuckDB httpfs) ---------------------------------
def zoom_to_level(ds):
    rep = os.path.join(
        DATA_ROOT, "bench", "overviews", ds + ".dup.report.json"
    )
    with open(rep) as f:
        r = json.load(f)
    return {lvl["zoom"]: lvl["level"] for lvl in r["levels"]}


def overview_run(ds, bbox, zoom, z2l):
    level = z2l.get(zoom)
    url = f"{BASE}/bench/overviews/{ds}.dup.parquet"
    xmin, ymin, xmax, ymax = bbox
    sql = (
        "INSTALL httpfs;LOAD httpfs;"
        "SET enable_http_metadata_cache=false;"
        f"CREATE TABLE t AS SELECT * FROM read_parquet('{url}') "
        f"WHERE level={level} "
        f"AND bbox.xmin <= {xmax} AND bbox.xmax >= {xmin} "
        f"AND bbox.ymin <= {ymax} AND bbox.ymax >= {ymin};"
        "SELECT count(*) FROM t;"
    )
    server_reset()
    t0 = time.perf_counter()
    out = subprocess.run(
        ["duckdb", "-noheader", "-list", "-c", sql],
        capture_output=True, text=True,
    )
    dt = (time.perf_counter() - t0) * 1000.0
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        raise RuntimeError(f"duckdb failed for {ds}")
    feats = int(out.stdout.strip().splitlines()[-1])
    st = server_stats()
    return {
        "bytes": st["bytes"],
        "requests": st["requests"],
        "get_requests": st["get_requests"],
        "head_requests": st["head_requests"],
        "wall_ms": dt,
        "features": feats,
        "level": level,
    }


# ---- pmtiles path ---------------------------------------------------
def pmtiles_run(ds, bbox, zoom):
    from pmtiles.reader import Reader

    url = f"{BASE}/goldens/tippecanoe/{ds}.pmtiles"
    server_reset()
    t0 = time.perf_counter()
    reader = Reader(lambda off, ln: range_get(url, off, ln))
    reader.header  # forces header + root directory read
    tiles = tiles_for_bbox(bbox, zoom)
    got = 0
    for (z, x, y) in tiles:
        blob = reader.get(z, x, y)
        if blob:
            got += 1
    dt = (time.perf_counter() - t0) * 1000.0
    st = server_stats()
    return {
        "bytes": st["bytes"],
        "requests": st["requests"],
        "get_requests": st["get_requests"],
        "wall_ms": dt,
        "tiles_requested": len(tiles),
        "tiles_present": got,
    }


def median_run(fn, *args):
    runs = [fn(*args) for _ in range(N_RUNS)]
    med = dict(runs[0])
    med["wall_ms"] = statistics.median(r["wall_ms"] for r in runs)
    med["wall_ms_p95"] = max(r["wall_ms"] for r in runs)
    return med


def main():
    results = {}
    for ds in DATASETS:
        z2l = zoom_to_level(ds)
        results[ds] = {}
        for vp in ("world", "regional", "street"):
            cfg = _viewports[ds]["viewports"][vp]
            bbox, zoom = cfg["bbox"], cfg["zoom"]
            ov = median_run(overview_run, ds, bbox, zoom, z2l)
            pm = median_run(pmtiles_run, ds, bbox, zoom)
            results[ds][vp] = {
                "zoom": zoom, "bbox": bbox,
                "overview": ov, "pmtiles": pm,
            }
            print(
                f"{ds:28s} {vp:9s} z{zoom:<2d} | "
                f"ov {ov['bytes']:>10,}B {ov['requests']:>3d}req "
                f"{ov['wall_ms']:>7.1f}ms {ov['features']:>7d}f | "
                f"pm {pm['bytes']:>10,}B {pm['requests']:>3d}req "
                f"{pm['wall_ms']:>7.1f}ms {pm['tiles_present']}/"
                f"{pm['tiles_requested']}t",
                flush=True,
            )
    out = os.path.join(HERE, "access_results.json")
    with open(out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {out}")


with open(os.path.join(HERE, "viewports.json")) as f:
    _viewports = json.load(f)

if __name__ == "__main__":
    main()
