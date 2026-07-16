#!/usr/bin/env python3
"""Remote-storage (S3 range-request) access benchmark — issue #176.

Same viewports and protocols as bench_access.py, but over real object
storage instead of the localhost logging server:

  overview path  -- fresh `duckdb` process reads the overview GeoParquet
                    straight from S3 (httpfs, credential_chain).  Request
                    count / bytes come from DuckDB's own HTTPFS HTTP Stats
                    (EXPLAIN ANALYZE); wall time is the query's Total Time.
  pmtiles path   -- python pmtiles reader over presigned HTTPS URLs with a
                    keep-alive session; requests and bytes are counted
                    client-side in the get_bytes hook.

Cold vs warm:
  cold = first access in a fresh client (fresh DuckDB process / fresh
         pmtiles reader + TLS session): pays header + metadata + data.
  warm = the same query/viewport repeated in the same client (DuckDB
         http_metadata_cache on but external_file_cache OFF; pmtiles
         reader + session reused): pays data re-fetch but not
         connection setup or parquet-footer/header metadata reads.
         DuckDB's data cache is disabled so both paths refetch data —
         symmetric with the cacheless python pmtiles reader.

Run:
  uv run --with pmtiles --with requests python3 bench_access_remote.py

Env:
  BENCH_BUCKET      (default tylertoo-bench)
  BENCH_REGION      (default us-east-2)
  BENCH_AWS_PROFILE (default nissim-admin)
  BENCH_RUNS        (default 3; medians reported)
  BENCH_DATASETS    (comma list; default all four)
  BENCH_OVERVIEW_PREFIX (default "overviews"; e.g. "sweep202/rg50k" to
                    point the overview path at a sweep variant prefix)
  BENCH_REPORT_DIR  (default corpus/data/bench/overviews; local dir with
                    <ds>.dup.report.json for the zoom->level mapping)
  BENCH_RESULTS     (default remote_access_results.json; output path)
  BENCH_SKIP_PMTILES=1  (overview path only -- for sweeps where the
                    PMTiles side is unchanged)
"""
import json
import math
import os
import re
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
DATA_ROOT = os.path.join(ROOT, "corpus", "data")

BUCKET = os.environ.get("BENCH_BUCKET", "tylertoo-bench")
REGION = os.environ.get("BENCH_REGION", "us-east-2")
PROFILE = os.environ.get("BENCH_AWS_PROFILE", "nissim-admin")
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))
OV_PREFIX = os.environ.get("BENCH_OVERVIEW_PREFIX", "overviews")
REPORT_DIR = os.environ.get(
    "BENCH_REPORT_DIR", os.path.join(DATA_ROOT, "bench", "overviews")
)
RESULTS_PATH = os.environ.get(
    "BENCH_RESULTS", os.path.join(HERE, "remote_access_results.json")
)
SKIP_PMTILES = os.environ.get("BENCH_SKIP_PMTILES") == "1"

DATASETS = os.environ.get(
    "BENCH_DATASETS",
    "points-nyc-medium,lines-portland-medium,"
    "polygons-portland-medium,polygons-ftw-moldova-large",
).split(",")


# ---- slippy tile math (same as bench_access.py) ---------------------
def lonlat_to_tile(lon, lat, z):
    n = 2 ** z
    x = int((lon + 180.0) / 360.0 * n)
    lat = max(min(lat, 85.05112878), -85.05112878)
    lat_rad = math.radians(lat)
    y = int((1.0 - math.asinh(math.tan(lat_rad)) / math.pi) / 2.0 * n)
    return min(max(x, 0), n - 1), min(max(y, 0), n - 1)


def tiles_for_bbox(bbox, z):
    xmin, ymin, xmax, ymax = bbox
    x0, y1 = lonlat_to_tile(xmin, ymin, z)
    x1, y0 = lonlat_to_tile(xmax, ymax, z)
    out = []
    for x in range(min(x0, x1), max(x0, x1) + 1):
        for y in range(min(y0, y1), max(y0, y1) + 1):
            out.append((z, x, y))
    return out


def zoom_to_level(ds):
    rep = os.path.join(REPORT_DIR, ds + ".dup.report.json")
    with open(rep) as f:
        r = json.load(f)
    return {lvl["zoom"]: lvl["level"] for lvl in r["levels"]}


# ---- DuckDB HTTPFS stats parsing ------------------------------------
_UNIT = {"bytes": 1, "byte": 1, "KiB": 1024, "MiB": 1024 ** 2,
         "GiB": 1024 ** 3}


def _parse_size(txt):
    m = re.match(r"([\d.]+)\s*(bytes|byte|KiB|MiB|GiB)", txt)
    return float(m.group(1)) * _UNIT[m.group(2)] if m else 0.0


def parse_duckdb_output(out):
    """Return per-EXPLAIN-ANALYZE [(bytes_in, head, get, total_s)]."""
    stats = []
    blocks = re.findall(
        r"HTTPFS HTTP Stats.*?#DELETE:\s*(\d+)", out, re.S
    )
    # re-scan structured: find each stats block's fields in order
    for blk in re.finditer(
        r"HTTPFS HTTP Stats(.*?)#DELETE:\s*\d+", out, re.S
    ):
        b = blk.group(1)
        bytes_in = _parse_size(
            re.search(r"in:\s*([\d.]+\s*\w+)", b).group(1)
        )
        head = int(re.search(r"#HEAD:\s*(\d+)", b).group(1))
        get = int(re.search(r"#GET:\s*(\d+)", b).group(1))
        stats.append({"bytes": bytes_in, "head": head, "get": get})
    times = [
        float(t) for t in re.findall(r"Total Time:\s*([\d.]+)s", out)
    ]
    for s, t in zip(stats, times):
        s["wall_ms"] = t * 1000.0
    assert len(blocks) == len(stats)
    return stats


def overview_run(ds, bbox, zoom, z2l):
    """One fresh DuckDB process: cold query then warm repeat."""
    level = z2l.get(zoom)
    url = f"s3://{BUCKET}/{OV_PREFIX}/{ds}.dup.parquet"
    xmin, ymin, xmax, ymax = bbox
    where = (
        f"WHERE level={level} "
        f"AND bbox.xmin <= {xmax} AND bbox.xmax >= {xmin} "
        f"AND bbox.ymin <= {ymax} AND bbox.ymax >= {ymin}"
    )
    sql = (
        "INSTALL httpfs;LOAD httpfs;"
        "CREATE SECRET (TYPE s3, PROVIDER credential_chain, "
        f"PROFILE '{PROFILE}', REGION '{REGION}');"
        "SET enable_http_metadata_cache=true;"
        # data cache off so warm measures metadata-warm re-access,
        # symmetric with the (cacheless) pmtiles reader
        "SET enable_external_file_cache=false;"
        f"EXPLAIN ANALYZE CREATE TABLE t_cold AS "
        f"SELECT * FROM read_parquet('{url}') {where};"
        f"EXPLAIN ANALYZE CREATE TABLE t_warm AS "
        f"SELECT * FROM read_parquet('{url}') {where};"
        "SELECT count(*) FROM t_cold;"
    )
    t0 = time.perf_counter()
    out = subprocess.run(
        ["duckdb", "-noheader", "-list", "-c", sql],
        capture_output=True, text=True,
    )
    proc_ms = (time.perf_counter() - t0) * 1000.0
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        raise RuntimeError(f"duckdb failed for {ds}")
    stats = parse_duckdb_output(out.stdout)
    assert len(stats) == 2, f"expected 2 stats blocks, got {len(stats)}"
    feats = int(out.stdout.strip().splitlines()[-1])
    cold, warm = stats
    cold.update(level=level, features=feats, proc_ms=proc_ms)
    warm.update(level=level, features=feats)
    return cold, warm


# ---- pmtiles path ----------------------------------------------------
def object_size(key):
    out = subprocess.run(
        ["aws", "s3api", "head-object", "--bucket", BUCKET,
         "--key", key, "--profile", PROFILE, "--region", REGION,
         "--query", "ContentLength", "--output", "text"],
        capture_output=True, text=True, check=True,
    )
    return int(out.stdout.strip())


def presign(key):
    out = subprocess.run(
        ["aws", "s3", "presign", f"s3://{BUCKET}/{key}",
         "--expires-in", "3600", "--profile", PROFILE,
         "--region", REGION],
        capture_output=True, text=True, check=True,
    )
    return out.stdout.strip()


class CountingFetcher:
    def __init__(self, session, url):
        self.session, self.url = session, url
        self.requests = 0
        self.bytes = 0

    def get_bytes(self, offset, length):
        r = self.session.get(
            self.url,
            headers={"Range": f"bytes={offset}-{offset + length - 1}"},
        )
        r.raise_for_status()
        self.requests += 1
        self.bytes += len(r.content)
        return r.content

    def reset(self):
        self.requests = 0
        self.bytes = 0


def pmtiles_run(ds, bbox, zoom, url):
    """Fresh reader+session: cold pass, then warm repeat."""
    import requests
    from pmtiles.reader import Reader

    session = requests.Session()
    fetcher = CountingFetcher(session, url)
    tiles = tiles_for_bbox(bbox, zoom)

    def one_pass(reader):
        got = 0
        t0 = time.perf_counter()
        reader.header
        for (z, x, y) in tiles:
            if reader.get(z, x, y):
                got += 1
        return (time.perf_counter() - t0) * 1000.0, got

    reader = Reader(fetcher.get_bytes)
    cold_ms, got = one_pass(reader)
    cold = {"bytes": fetcher.bytes, "requests": fetcher.requests,
            "wall_ms": cold_ms, "tiles_requested": len(tiles),
            "tiles_present": got}
    fetcher.reset()
    warm_ms, got = one_pass(reader)
    warm = {"bytes": fetcher.bytes, "requests": fetcher.requests,
            "wall_ms": warm_ms, "tiles_requested": len(tiles),
            "tiles_present": got}
    session.close()
    return cold, warm


# ---- driver ----------------------------------------------------------
def median_of(runs):
    med = dict(runs[0])
    med["wall_ms"] = statistics.median(r["wall_ms"] for r in runs)
    med["wall_ms_max"] = max(r["wall_ms"] for r in runs)
    return med


def main():
    with open(os.path.join(HERE, "viewports.json")) as f:
        viewports = json.load(f)

    results = {"bucket": BUCKET, "region": REGION, "runs": N_RUNS,
               "overview_prefix": OV_PREFIX, "datasets": {}}
    for ds in DATASETS:
        z2l = zoom_to_level(ds)
        results["datasets"][ds] = {
            "overview_file_bytes": object_size(
                f"{OV_PREFIX}/{ds}.dup.parquet"
            ),
        }
        if not SKIP_PMTILES:
            pm_url = presign(f"pmtiles/{ds}.pmtiles")
            results["datasets"][ds]["pmtiles_file_bytes"] = object_size(
                f"pmtiles/{ds}.pmtiles"
            )
        for vp in ("world", "regional", "street"):
            cfg = viewports[ds]["viewports"][vp]
            bbox, zoom = cfg["bbox"], cfg["zoom"]

            ov_cold, ov_warm, pm_cold, pm_warm = [], [], [], []
            for _ in range(N_RUNS):
                c, w = overview_run(ds, bbox, zoom, z2l)
                ov_cold.append(c)
                ov_warm.append(w)
                if not SKIP_PMTILES:
                    c, w = pmtiles_run(ds, bbox, zoom, pm_url)
                    pm_cold.append(c)
                    pm_warm.append(w)

            entry = {
                "zoom": zoom, "bbox": bbox,
                "overview": {"cold": median_of(ov_cold),
                             "warm": median_of(ov_warm)},
            }
            if not SKIP_PMTILES:
                entry["pmtiles"] = {"cold": median_of(pm_cold),
                                    "warm": median_of(pm_warm)}
            results["datasets"][ds][vp] = entry
            oc = entry["overview"]["cold"]
            line = (
                f"{ds:28s} {vp:9s} z{zoom:<2d} | "
                f"ov {oc['bytes']:>11,.0f}B "
                f"{oc['head'] + oc['get']:>3d}req "
                f"{oc['wall_ms']:>8.1f}ms {oc['features']:>7d}f"
            )
            if not SKIP_PMTILES:
                pc = entry["pmtiles"]["cold"]
                line += (
                    f" | pm {pc['bytes']:>11,}B {pc['requests']:>3d}req "
                    f"{pc['wall_ms']:>8.1f}ms "
                    f"{pc['tiles_present']}/{pc['tiles_requested']}t"
                )
            print(line, flush=True)

    out = RESULTS_PATH
    with open(out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
