#!/usr/bin/env python3
"""Layout benchmark: duplicating vs partitioning vs gpo (partitioning +
geom_overview) — viewer-style range reads against real S3.

Three files, same 5.28M-building dataset (Overture Germany central subset),
each written by its converter's buildings-appropriate settings:

  dup   gpq-tiles overview --mode duplicating  z0-14   (geo:overviews key)
  part  gpq-tiles overview --mode partitioning z0-14   (geo:overviews key)
  gpo   yharby gpo convert, defaults: 3 bands + geom_overview (overviews key)

Read model (identical policy for all three, so the *layout* is what is
measured):

  1. Footer: one 64 KiB tail request; if the footer is larger, one more
     request for the remainder (hyparquet-style).
  2. Level for zoom z:
       ours: coarsest level with level.zoom >= z, else the last.
             dup  -> that level's own row-group slice (self-contained)
             part -> the cumulative row-group prefix 0..row_group_end
       gpo:  coarsest level with max_zoom >= z, else last; cumulative
             prefix; coarse bands read geom_overview, final band geometry.
  3. Prune surviving row groups against the viewport with the bbox
     covering column's footer statistics.
  4. Fetch the rendering geometry column chunk of each surviving row
     group with HTTP Range requests (adjacent ranges merged), 8-way
     parallel, fresh HTTPS session per run (cold), 3 runs, medians.

Planning uses the local copy's footer (byte-identical to the uploaded
object); all fetches hit S3 over HTTPS with presigned URLs.

Run:
  uv run --with pyarrow --with requests python3 bench_layout_reads.py

Env: BENCH_RUNS (default 3), BENCH_RESULTS (default layout_read_results.json)
"""
import concurrent.futures
import json
import os
import statistics
import subprocess
import sys
import time

import pyarrow.parquet as pq
import requests

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
LOCAL = os.path.join(ROOT, "corpus", "data", "layoutbench")

BUCKET = os.environ.get("BENCH_BUCKET", "gpq-tiles-bench")
REGION = os.environ.get("BENCH_REGION", "us-east-2")
PROFILE = os.environ.get("BENCH_AWS_PROFILE", "default")
PREFIX = os.environ.get("BENCH_PREFIX", "layoutbench")
N_RUNS = int(os.environ.get("BENCH_RUNS", "3"))
RESULTS = os.environ.get("BENCH_RESULTS", os.path.join(HERE, "layout_read_results.json"))

FILES = {
    "dup": "buildings-de-central.dup.parquet",
    "part": "buildings-de-central.part.parquet",
    "gpo": "buildings-de-central.gpo.parquet",
    # column-per-zoom prototype: one simplified-geometry column per zoom
    # (geom_z7..geom_z13 + exact `geometry`), rows unique, band-major order.
    # A reader picks ONE column for its zoom; the footer's per-chunk null
    # statistics say which row groups hold that column's data.
    "cpz": "buildings-de-central.cpz.parquet",
}

VIEWPORTS = {
    "world": {"zoom": 8, "bbox": [7.8, 49.5, 10.2, 51.6]},
    "regional": {"zoom": 11, "bbox": [8.7, 50.2875, 9.3, 50.8125]},
    "street": {"zoom": 14, "bbox": [8.76, 50.22, 8.78, 50.24]},
}

TAIL_PROBE = 64 * 1024  # initial footer fetch size


def presign(key):
    out = subprocess.run(
        ["aws", "s3", "presign", f"s3://{BUCKET}/{PREFIX}/{key}",
         "--region", REGION, "--profile", PROFILE, "--expires-in", "43200"],
        capture_output=True, text=True, check=True,
    )
    return out.stdout.strip()


class Plan:
    """A file's parsed layout: levels, per-row-group bbox stats and the
    byte range of each candidate rendering column chunk."""

    def __init__(self, tag, path):
        self.tag = tag
        pf = pq.ParquetFile(path)
        md = pf.metadata
        self.file_bytes = os.path.getsize(path)
        with open(path, "rb") as f:
            f.seek(-8, 2)
            self.footer_bytes = int.from_bytes(f.read(4), "little") + 8
        kv = md.metadata
        if b"geo:overviews" in kv:
            ov = json.loads(kv[b"geo:overviews"])
            self.kind = ov["mode"]  # duplicating | partitioning | column-per-zoom
            if self.kind == "column-per-zoom":
                self.levels = [
                    {"zoom": l["zoom"], "column": l["column"]}
                    for l in ov["levels"]
                ]
            else:
                self.levels = [
                    {"zoom": l["zoom"], "rg_end": l["row_group_end"]}
                    for l in ov["levels"]
                ]
            self.overview_column = None
        else:
            ov = json.loads(kv[b"overviews"])
            self.kind = "gpo"
            self.levels = [
                {"zoom": l["max_zoom"], "rg_end": l["row_group_end"]}
                for l in ov["levels"]
            ]
            self.overview_column = ov.get("overview_column")

        # column indices by dotted path, from row group 0
        paths = {}
        rg0 = md.row_group(0)
        for i in range(rg0.num_columns):
            paths[rg0.column(i).path_in_schema] = i
        self.col_idx = paths

        self.rg = []
        for i in range(md.num_row_groups):
            rg = md.row_group(i)

            def stat(path, attr):
                s = rg.column(paths[path]).statistics
                return getattr(s, attr) if s and s.has_min_max else None

            def chunk_range(path):
                if path not in paths:
                    return None
                c = rg.column(paths[path])
                start = c.data_page_offset
                if c.dictionary_page_offset is not None:
                    start = min(start, c.dictionary_page_offset)
                return (start, start + c.total_compressed_size)

            entry = {
                "xmin": stat("bbox.xmin", "min"),
                "ymin": stat("bbox.ymin", "min"),
                "xmax": stat("bbox.xmax", "max"),
                "ymax": stat("bbox.ymax", "max"),
                "rows": rg.num_rows,
                "geometry": chunk_range("geometry"),
                "overview": chunk_range(self.overview_column)
                if self.overview_column else None,
            }
            if self.kind == "column-per-zoom":
                # per zoom column: byte range + whether this row group holds
                # any values at all (all-null chunks are skipped from the
                # footer alone — the null statistics ARE the level index)
                cols = {}
                for l in self.levels:
                    col = l["column"]
                    s = rg.column(paths[col]).statistics
                    all_null = (s is not None and s.null_count is not None
                                and s.null_count == rg.num_rows)
                    cols[col] = {"range": chunk_range(col), "all_null": all_null}
                entry["cols"] = cols
            self.rg.append(entry)

    def level_for_zoom(self, z):
        for i, l in enumerate(self.levels):
            if z <= l["zoom"]:
                return i
        return len(self.levels) - 1

    def read_plan(self, zoom, bbox):
        """Row-group indices + (column, byte-range) fetches for one view."""
        li = self.level_for_zoom(zoom)
        lv = self.levels[li]
        if self.kind == "column-per-zoom":
            # one column carries the whole level; candidate row groups are
            # those whose chunk holds any values, then bbox-pruned
            col = lv["column"]
            vx0, vy0, vx1, vy1 = bbox
            picked, ranges, rows = [], [], 0
            for i, g in enumerate(self.rg):
                info = g["cols"][col]
                if info["all_null"] or info["range"] is None:
                    continue
                if g["xmin"] is None or (
                    g["xmin"] <= vx1 and g["xmax"] >= vx0
                    and g["ymin"] <= vy1 and g["ymax"] >= vy0
                ):
                    picked.append(i)
                    ranges.append(info["range"])
                    rows += g["rows"]
            return {"level": li, "column": col, "row_groups": picked,
                    "candidate_rgs": sum(1 for g in self.rg
                                         if not g["cols"][col]["all_null"]),
                    "rows_spanned": rows, "ranges": merge_ranges(ranges)}
        if self.kind == "duplicating":
            start = self.levels[li - 1]["rg_end"] + 1 if li > 0 else 0
            candidates = range(start, lv["rg_end"] + 1)
        else:  # partitioning and gpo read the cumulative prefix
            candidates = range(0, lv["rg_end"] + 1)

        is_final = li == len(self.levels) - 1
        col = ("overview"
               if self.kind == "gpo" and self.overview_column and not is_final
               else "geometry")

        vx0, vy0, vx1, vy1 = bbox
        picked, ranges, rows = [], [], 0
        for i in candidates:
            g = self.rg[i]
            if g["xmin"] is None or (
                g["xmin"] <= vx1 and g["xmax"] >= vx0
                and g["ymin"] <= vy1 and g["ymax"] >= vy0
            ):
                r = g[col]
                if r is not None:
                    picked.append(i)
                    ranges.append(r)
                    rows += g["rows"]
        return {"level": li, "column": col, "row_groups": picked,
                "candidate_rgs": len(list(candidates)), "rows_spanned": rows,
                "ranges": merge_ranges(ranges)}


def merge_ranges(ranges, gap=0):
    if not ranges:
        return []
    out = []
    for s, e in sorted(ranges):
        if out and s <= out[-1][1] + gap:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return [(s, e) for s, e in out]


def fetch_view(url, plan_result, footer_bytes, file_bytes):
    """One cold run: fresh session, footer + data ranges, 8-way parallel."""
    session = requests.Session()
    t0 = time.monotonic()
    n_req = 0
    total = 0

    # footer: tail probe, then remainder if the footer overflows the probe
    tail_start = max(0, file_bytes - TAIL_PROBE)
    r = session.get(url, headers={"Range": f"bytes={tail_start}-{file_bytes - 1}"})
    r.raise_for_status()
    n_req += 1
    total += len(r.content)
    if footer_bytes > TAIL_PROBE:
        rest = footer_bytes - TAIL_PROBE
        r = session.get(url, headers={
            "Range": f"bytes={file_bytes - footer_bytes}-{tail_start - 1}"})
        r.raise_for_status()
        n_req += 1
        total += len(r.content)
        assert len(r.content) == rest

    def get_range(rng):
        s, e = rng
        rr = session.get(url, headers={"Range": f"bytes={s}-{e - 1}"})
        rr.raise_for_status()
        return len(rr.content)

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as ex:
        for n in ex.map(get_range, plan_result["ranges"]):
            total += n
            n_req += 1
    wall = time.monotonic() - t0
    return {"requests": n_req, "bytes": total, "wall_s": wall}


def main():
    results = {"viewports": VIEWPORTS, "runs": N_RUNS, "files": {}}
    for tag, fname in FILES.items():
        local = os.path.join(LOCAL, fname)
        plan = Plan(tag, local)
        url = presign(fname)
        entry = {
            "file_bytes": plan.file_bytes,
            "footer_bytes": plan.footer_bytes,
            "kind": plan.kind,
            "levels": plan.levels,
            "views": {},
        }
        for vname, v in VIEWPORTS.items():
            pr = plan.read_plan(v["zoom"], v["bbox"])
            runs = [fetch_view(url, pr, plan.footer_bytes, plan.file_bytes)
                    for _ in range(N_RUNS)]
            med = {k: statistics.median(r[k] for r in runs)
                   for k in ("requests", "bytes", "wall_s")}
            entry["views"][vname] = {
                "zoom": v["zoom"],
                "level": pr["level"],
                "column": pr["column"],
                "row_groups_read": len(pr["row_groups"]),
                "row_groups_candidate": pr["candidate_rgs"],
                "rows_spanned": pr["rows_spanned"],
                **med,
            }
            print(f"{tag:5s} {vname:9s} z{v['zoom']:<3d} col={pr['column']:9s} "
                  f"rg={len(pr['row_groups']):4d}/{pr['candidate_rgs']:4d} "
                  f"req={med['requests']:5.0f} "
                  f"bytes={med['bytes'] / 1e6:8.2f}MB wall={med['wall_s']:6.2f}s",
                  flush=True)
        results["files"][tag] = entry
    with open(RESULTS, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {RESULTS}")


if __name__ == "__main__":
    main()
