#!/usr/bin/env python3
"""DuckDB httpfs client-knob sweep for remote overview reads — issue #203.

Same bucket, viewports, and 3-run-median protocol as
bench_access_remote.py (issue #176 / PR #200), but instead of comparing
formats it compares DuckDB client configurations on the overview path:

Part 1 — cold, per-knob:
  a fresh `duckdb` process runs ONE viewport query per config.
  Configs: pure defaults, each candidate knob flipped individually,
  and the recommended stack.  Request/byte counts come from DuckDB's
  own HTTPFS HTTP Stats (EXPLAIN ANALYZE); wall is the query's
  Total Time.  Median of BENCH_RUNS process launches.

Part 2 — session behavior:
  one process runs cold -> exact repeat -> adjacent viewport (bbox
  panned east by one width, same level).  Two configs:
    sym200 -- the PR #200 symmetric-benchmark config
              (http metadata cache ON, external file (data) cache OFF;
              chosen there so DuckDB re-fetched data like the
              cacheless python pmtiles reader did)
    real   -- the real-user map-session config (both metadata caches
              ON, external file cache ON i.e. left at default)

Knobs verified present in DuckDB v1.4.1 via duckdb_settings().

Run:
  python3 bench_duckdb_knobs.py            # stdlib only

Env:
  BENCH_BUCKET      (default tylertoo-bench)
  BENCH_REGION      (default us-east-2)
  BENCH_AWS_PROFILE (default nissim-admin)
  BENCH_RUNS        (default 3; medians reported)
  BENCH_DATASETS    (comma list; default the two knob-sweep datasets)
"""
import json
import os
import re
import statistics
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

BUCKET = os.environ.get("BENCH_BUCKET", "tylertoo-bench")
REGION = os.environ.get("BENCH_REGION", "us-east-2")
PROFILE = os.environ.get("BENCH_AWS_PROFILE", "nissim-admin")
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))

DATASETS = os.environ.get(
    "BENCH_DATASETS", "points-nyc-medium,polygons-ftw-moldova-large"
).split(",")

# zoom -> overview level, from the #200 run (corpus dup.report.json;
# recorded in remote_access_results.json so the sweep does not need
# the local corpus artifacts).
ZOOM_TO_LEVEL = {
    "points-nyc-medium": {8: 8, 11: 11, 14: 14},
    "lines-portland-medium": {8: 8, 11: 11, 14: 14},
    "polygons-portland-medium": {8: 8, 11: 11, 14: 14},
    "polygons-ftw-moldova-large": {6: 3, 9: 6, 14: 11},
}

# ---- configs ---------------------------------------------------------
# Part 1: cold single-query configs (fresh process each).
COLD_CONFIGS = {
    "defaults": [],
    "threads_64": ["SET threads=64;"],
    "no_parquet_prefetch": ["SET disable_parquet_prefetching=true;"],
    "prefetch_all": ["SET prefetch_all_parquet_files=true;"],
    "keepalive_off": ["SET http_keep_alive=false;"],
    "metadata_caches_on": [
        # expected no-op on a single cold query; included to prove it
        "SET enable_http_metadata_cache=true;",
        "SET parquet_metadata_cache=true;",
    ],
    "stack": [
        "SET threads=64;",
        "SET enable_http_metadata_cache=true;",
        "SET parquet_metadata_cache=true;",
        # enable_external_file_cache=true is the default; stated for
        # clarity since PR #200 benchmarked with it OFF
        "SET enable_external_file_cache=true;",
    ],
}

# Part 2: session configs (one process: cold, repeat, adjacent pan).
SESSION_CONFIGS = {
    "sym200": [
        "SET enable_http_metadata_cache=true;",
        "SET enable_external_file_cache=false;",
    ],
    "real": [
        "SET threads=64;",
        "SET enable_http_metadata_cache=true;",
        "SET parquet_metadata_cache=true;",
        "SET enable_external_file_cache=true;",
    ],
}

_UNIT = {"bytes": 1, "byte": 1, "KiB": 1024, "MiB": 1024 ** 2,
         "GiB": 1024 ** 3}
MARK = "===Q{}==="


def _parse_size(txt):
    m = re.match(r"([\d.]+)\s*(bytes|byte|KiB|MiB|GiB)", txt)
    return float(m.group(1)) * _UNIT[m.group(2)] if m else 0.0


def parse_segment(seg):
    """HTTPFS stats + Total Time from one query's output segment.

    A fully cache-served query performs no HTTP, so the stats block
    may be absent -> zeros.
    """
    out = {"bytes": 0.0, "head": 0, "get": 0, "wall_ms": None}
    blk = re.search(r"HTTPFS HTTP Stats(.*?)#DELETE:\s*\d+", seg, re.S)
    if blk:
        b = blk.group(1)
        out["bytes"] = _parse_size(
            re.search(r"in:\s*([\d.]+\s*\w+)", b).group(1))
        out["head"] = int(re.search(r"#HEAD:\s*(\d+)", b).group(1))
        out["get"] = int(re.search(r"#GET:\s*(\d+)", b).group(1))
    t = re.search(r"Total Time:\s*([\d.]+)s", seg)
    if t:
        out["wall_ms"] = float(t.group(1)) * 1000.0
    return out


def where_clause(level, bbox):
    xmin, ymin, xmax, ymax = bbox
    return (
        f"WHERE level={level} "
        f"AND bbox.xmin <= {xmax} AND bbox.xmax >= {xmin} "
        f"AND bbox.ymin <= {ymax} AND bbox.ymax >= {ymin}"
    )


def run_duckdb(sets, queries):
    """One duckdb process; returns per-query parsed stats + counts.

    queries: list of WHERE clauses; each becomes
    EXPLAIN ANALYZE CREATE TABLE t<i> AS SELECT * ... <where>
    followed by a marker + count(*).
    """
    url = f"s3://{BUCKET}/overviews/{DS_CURRENT}.dup.parquet"
    sql = [
        "INSTALL httpfs;LOAD httpfs;",
        "CREATE SECRET (TYPE s3, PROVIDER credential_chain, "
        f"PROFILE '{PROFILE}', REGION '{REGION}');",
    ] + list(sets)
    for i, where in enumerate(queries):
        sql.append(f"SELECT '{MARK.format(i)}';")
        sql.append(
            f"EXPLAIN ANALYZE CREATE TABLE t{i} AS "
            f"SELECT * FROM read_parquet('{url}') {where};"
        )
        sql.append(f"SELECT count(*) FROM t{i};")
    out = subprocess.run(
        ["duckdb", "-noheader", "-list", "-c", "".join(sql)],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        raise RuntimeError("duckdb failed")
    parts = re.split(r"===Q\d+===", out.stdout)[1:]
    assert len(parts) == len(queries), (len(parts), len(queries))
    results = []
    for seg in parts:
        s = parse_segment(seg)
        # last non-empty line of the segment is the count(*) result
        s["features"] = int(
            [ln for ln in seg.strip().splitlines() if ln.strip()][-1])
        assert s["wall_ms"] is not None
        results.append(s)
    return results


def median_runs(runs):
    """runs: list of per-run dicts. Median wall; req/bytes from the
    median-wall run (they are near-deterministic anyway)."""
    walls = sorted(runs, key=lambda r: r["wall_ms"])
    med = dict(walls[len(walls) // 2])
    med["wall_ms"] = statistics.median(r["wall_ms"] for r in runs)
    med["wall_ms_min"] = min(r["wall_ms"] for r in runs)
    med["wall_ms_max"] = max(r["wall_ms"] for r in runs)
    return med


def pan_east(bbox):
    xmin, ymin, xmax, ymax = bbox
    w = xmax - xmin
    return [xmin + w, ymin, xmax + w, ymax]


DS_CURRENT = None


def main():
    global DS_CURRENT
    with open(os.path.join(HERE, "viewports.json")) as f:
        viewports = json.load(f)

    results = {"bucket": BUCKET, "region": REGION, "runs": N_RUNS,
               "duckdb": subprocess.run(
                   ["duckdb", "--version"], capture_output=True,
                   text=True).stdout.strip(),
               "cold_configs": {k: v for k, v in COLD_CONFIGS.items()},
               "session_configs": SESSION_CONFIGS,
               "datasets": {}}

    for ds in DATASETS:
        DS_CURRENT = ds
        z2l = ZOOM_TO_LEVEL[ds]
        results["datasets"][ds] = {}
        for vp in ("world", "regional", "street"):
            cfg = viewports[ds]["viewports"][vp]
            bbox, zoom = cfg["bbox"], cfg["zoom"]
            level = z2l[zoom]
            entry = {"zoom": zoom, "level": level, "bbox": bbox,
                     "cold": {}, "session": {}}

            # Part 1: cold per-knob
            for name, sets in COLD_CONFIGS.items():
                runs = []
                for _ in range(N_RUNS):
                    (r,) = run_duckdb(sets, [where_clause(level, bbox)])
                    runs.append(r)
                med = median_runs(runs)
                entry["cold"][name] = med
                print(f"{ds:27s} {vp:9s} cold {name:20s} "
                      f"{med['bytes']:>12,.0f}B "
                      f"{med['head'] + med['get']:>3d}req "
                      f"{med['wall_ms']:>8.1f}ms "
                      f"{med['features']:>6d}f", flush=True)

            # Part 2: sessions (cold, repeat, adjacent pan)
            w0 = where_clause(level, bbox)
            w_adj = where_clause(level, pan_east(bbox))
            for name, sets in SESSION_CONFIGS.items():
                per_q = [[], [], []]
                for _ in range(N_RUNS):
                    rs = run_duckdb(sets, [w0, w0, w_adj])
                    for i, r in enumerate(rs):
                        per_q[i].append(r)
                meds = [median_runs(q) for q in per_q]
                entry["session"][name] = {
                    "cold": meds[0], "repeat": meds[1],
                    "adjacent": meds[2],
                }
                for label, m in zip(("cold", "repeat", "adjacent"),
                                    meds):
                    print(f"{ds:27s} {vp:9s} sess {name:7s} "
                          f"{label:8s} {m['bytes']:>12,.0f}B "
                          f"{m['head'] + m['get']:>3d}req "
                          f"{m['wall_ms']:>8.1f}ms "
                          f"{m['features']:>6d}f", flush=True)

            results["datasets"][ds][vp] = entry

    out = os.path.join(HERE, "duckdb_knobs_results.json")
    with open(out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
