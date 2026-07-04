#!/usr/bin/env python3
"""Parallel-reader remote benchmark — issue #201.

Runs parallel_reader.py (the purpose-built concurrent range-request
reader) over the SAME S3 artifacts, datasets, viewports, and
runs/median protocol as bench_access_remote.py, and verifies that
the feature counts match the DuckDB numbers recorded in
remote_access_results.json (same predicate => same rows).

Per cell (dataset x viewport), per run:
  cold          -- fresh requests.Session + reader: pays TLS,
                   footer fetch, concurrent data fetch, decode.
  footer-cached -- the same reader immediately re-runs the
                   viewport: metadata + connections held, only
                   data ranges re-fetched (the map-session case).

The zoom->level mapping comes from the .dup.report.json objects in
the bucket (the worktree may not have corpus/data locally).

Run:
  uv run --with pyarrow --with requests python3 bench_parallel.py

Env:
  BENCH_BUCKET      (default gpq-tiles-bench)
  BENCH_REGION      (default us-east-2)
  BENCH_AWS_PROFILE (default nissim-admin)
  BENCH_RUNS        (default 3; medians reported)
  BENCH_DATASETS    (comma list; default all four)
"""
import json
import os
import statistics
import subprocess
import sys

from parallel_reader import ParallelReader, make_session

HERE = os.path.dirname(os.path.abspath(__file__))

BUCKET = os.environ.get("BENCH_BUCKET", "gpq-tiles-bench")
REGION = os.environ.get("BENCH_REGION", "us-east-2")
PROFILE = os.environ.get("BENCH_AWS_PROFILE", "nissim-admin")
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))

DATASETS = os.environ.get(
    "BENCH_DATASETS",
    "points-nyc-medium,lines-portland-medium,"
    "polygons-portland-medium,polygons-ftw-moldova-large",
).split(",")


def presign(key):
    out = subprocess.run(
        ["aws", "s3", "presign", f"s3://{BUCKET}/{key}",
         "--expires-in", "3600", "--profile", PROFILE,
         "--region", REGION],
        capture_output=True, text=True, check=True,
    )
    return out.stdout.strip()


def zoom_to_level(session, ds):
    url = presign(f"overviews/{ds}.dup.report.json")
    r = session.get(url)
    r.raise_for_status()
    rep = r.json()
    return {lvl["zoom"]: lvl["level"] for lvl in rep["levels"]}


def median_of(runs):
    med = dict(runs[0])
    for k in ("wall_ms", "footer_ms", "fetch_ms", "decode_ms"):
        med[k] = statistics.median(r[k] for r in runs)
    med["wall_ms_max"] = max(r["wall_ms"] for r in runs)
    return med


def main():
    with open(os.path.join(HERE, "viewports.json")) as f:
        viewports = json.load(f)
    with open(
        os.path.join(HERE, "remote_access_results.json")
    ) as f:
        baseline = json.load(f)

    meta_session = make_session()
    results = {"bucket": BUCKET, "region": REGION, "runs": N_RUNS,
               "pool": 16, "datasets": {}}
    mismatches = []

    for ds in DATASETS:
        z2l = zoom_to_level(meta_session, ds)
        url = presign(f"overviews/{ds}.dup.parquet")
        results["datasets"][ds] = {}
        for vp in ("world", "regional", "street"):
            cfg = viewports[ds]["viewports"][vp]
            bbox, zoom = cfg["bbox"], cfg["zoom"]
            level = z2l[zoom]
            expected = (
                baseline["datasets"][ds][vp]["overview"]["cold"]
                ["features"]
            )

            cold_runs, cached_runs = [], []
            for _ in range(N_RUNS):
                rd = ParallelReader(url)  # fresh session = cold
                c = rd.read_viewport(level, bbox)
                w = rd.read_viewport(level, bbox)
                for st in (c, w):
                    if st["features"] != expected:
                        mismatches.append(
                            (ds, vp, st["mode"],
                             st["features"], expected)
                        )
                cold_runs.append(c)
                cached_runs.append(w)
                rd.session.close()

            entry = {
                "zoom": zoom, "bbox": bbox, "level": level,
                "expected_features_duckdb": expected,
                "cold": median_of(cold_runs),
                "footer_cached": median_of(cached_runs),
            }
            results["datasets"][ds][vp] = entry
            c = entry["cold"]
            w = entry["footer_cached"]
            print(
                f"{ds:28s} {vp:9s} z{zoom:<2d} | "
                f"cold {c['wall_ms']:>7.0f}ms {c['requests']:>2d}req "
                f"{c['bytes']:>11,}B | "
                f"cached {w['wall_ms']:>6.0f}ms "
                f"{w['requests']:>2d}req | "
                f"{c['features']:>6d}f "
                f"(duckdb {expected}) "
                f"rg {c['rg_selected']}/{c['rg_total']}",
                flush=True,
            )

    out = os.path.join(HERE, "parallel_reader_results.json")
    with open(out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {out}")

    if mismatches:
        for m in mismatches:
            print(
                f"FEATURE COUNT MISMATCH: {m[0]}/{m[1]} ({m[2]}) "
                f"got {m[3]}, duckdb said {m[4]}",
                file=sys.stderr,
            )
        sys.exit(1)
    print("all feature counts match the DuckDB baseline")


if __name__ == "__main__":
    main()
