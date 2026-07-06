#!/usr/bin/env python3
"""Layout benchmark, SQL path: DuckDB over S3 for the three layouts.

Companion to bench_layout_reads.py (same three files). Measures what the
"a GeoParquet file is still a GeoParquet file" story costs under each
layout, with DuckDB's own HTTPFS HTTP Stats (EXPLAIN ANALYZE) providing
requests/bytes and Total Time the wall clock. Every run is a fresh DuckDB
process (cold: TLS + footer + data).

Queries per file:
  agg_naive      SELECT count(*), avg(height)  -- no predicate.
                 On `dup` this double-counts (duplicated coarse rows):
                 the returned count is recorded to document the footgun.
  agg_correct    dup only: the same aggregate restricted to the canonical
                 level (WHERE level = <canonical>), i.e. what a correct
                 reader must write instead.
  win_street     full-resolution feature fetch in the street window:
                 count + geometry bytes, bbox predicate (+ level filter
                 on dup, where full resolution lives only in the
                 canonical band).
  win_regional   same for the regional window.

Run:
  uv run python3 bench_layout_sql.py     (needs duckdb CLI + aws creds)
"""
import json
import os
import re
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BUCKET = "gpq-tiles-bench"
REGION = "us-east-2"
PROFILE = "nissim-admin"
PREFIX = "layoutbench"
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))
RESULTS = os.environ.get("BENCH_RESULTS", os.path.join(HERE, "layout_sql_results.json"))

FILES = {
    "dup": "buildings-de-central.dup.parquet",
    "part": "buildings-de-central.part.parquet",
    "gpo": "buildings-de-central.gpo.parquet",
    "cpz": "buildings-de-central.cpz.parquet",
}
CANONICAL_LEVEL = {"dup": 7}  # level index of the canonical band (z14 is index 7)

WINDOWS = {
    "win_street": [8.76, 50.22, 8.78, 50.24],
    "win_regional": [8.7, 50.2875, 9.3, 50.8125],
}


def bbox_where(b):
    x0, y0, x1, y1 = b
    return (f"bbox.xmin <= {x1} AND bbox.xmax >= {x0} "
            f"AND bbox.ymin <= {y1} AND bbox.ymax >= {y0}")


def queries_for(tag):
    lvl = CANONICAL_LEVEL.get(tag)
    q = {"agg_naive": ("SELECT count(*), avg(height) FROM t", None)}
    if lvl is not None:
        q["agg_correct"] = (
            f"SELECT count(*), avg(height) FROM t WHERE level = {lvl}", None)
    for name, b in WINDOWS.items():
        where = bbox_where(b)
        if lvl is not None:
            where += f" AND level = {lvl}"
        q[name] = (
            "SELECT count(*), sum(octet_length(geometry::BLOB)) "
            f"FROM t WHERE {where}", None)
    return q


def parse_stats(out):
    m = re.search(
        r"HTTPFS HTTP Stats.*?in:\s*([\d.]+)\s*(\w+).*?#HEAD:\s*(\d+).*?#GET:\s*(\d+)",
        out, re.S)
    if not m:
        raise RuntimeError("no HTTPFS stats block:\n" + out[-2000:])
    val, unit, head, get = m.groups()
    mult = {"bytes": 1, "B": 1, "KiB": 2 ** 10, "MiB": 2 ** 20, "GiB": 2 ** 30}[unit]
    t = re.search(r"Total Time:\s*([\d.]+)s", out)
    return {"bytes": float(val) * mult, "head": int(head),
            "get": int(get), "wall_s": float(t.group(1))}


def run_query(url, select_sql):
    sql = (
        "INSTALL httpfs;LOAD httpfs;"
        "CREATE SECRET (TYPE s3, PROVIDER credential_chain, "
        f"PROFILE '{PROFILE}', REGION '{REGION}');"
        f"CREATE VIEW t AS FROM read_parquet('s3://{BUCKET}/{PREFIX}/{{}}');"
        .format(url) +
        f"EXPLAIN ANALYZE {select_sql};"
        f"{select_sql};"
    )
    out = subprocess.run(["duckdb", "-noheader", "-list", "-c", sql],
                         capture_output=True, text=True)
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        raise RuntimeError("duckdb failed")
    stats = parse_stats(out.stdout)
    stats["result"] = out.stdout.strip().splitlines()[-1]
    return stats


def main():
    results = {"runs": N_RUNS, "windows": WINDOWS, "files": {}}
    for tag, fname in FILES.items():
        entry = {}
        for qname, (qsql, _) in queries_for(tag).items():
            runs = [run_query(fname, qsql) for _ in range(N_RUNS)]
            med = {k: statistics.median(r[k] for r in runs)
                   for k in ("bytes", "head", "get", "wall_s")}
            med["result"] = runs[0]["result"]
            entry[qname] = med
            print(f"{tag:5s} {qname:13s} req={med['get']:5.0f} "
                  f"bytes={med['bytes'] / 1e6:8.2f}MB wall={med['wall_s']:6.2f}s "
                  f"result={med['result']}", flush=True)
        results["files"][tag] = entry
    with open(RESULTS, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {RESULTS}")


if __name__ == "__main__":
    main()
