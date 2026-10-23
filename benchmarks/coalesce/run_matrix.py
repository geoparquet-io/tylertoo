#!/usr/bin/env python3
"""
run_matrix.py — the #745 benchmark: convert variants × size valves × input
orders, small to big, with a time and RSS cap on every run.

Each stage is cheap next to the one after it, and its job is to rule out
broken or impractical options before the expensive runs (#745 plan, phase C):

  G0  Madagascar adm4 (17k), hilbert       correctness and determinism
  G1  FTW Moldova (632k), hilbert          first cost signal, all converts
  G2  FTW Moldova, four orders             what row order costs and changes
  G3  NZ Parcels / FTW Ukraine (4M+)       finalists only (pass --converts,
                                           --valves)

What it records, per run, so a slowdown can be traced to its source:

  * convert and export each on their own: wall, CPU, peak RSS (GNU time),
    and the cap outcome (ok, TIMEOUT, RSS_CAP, FAILED). A capped run is a
    result, and the matrix moves on.
  * export phase timings from TYLERTOO_PROFILE_JSON (the last line).
  * the export report's valve tallies per zoom (merged, dropped, passes)
    and tile counts.
  * totals with and without the input's preparation (prep.json from
    prepare_data.py), so a sorted input does not look cheaper because its
    sort ran outside the timer.

Convert runs once per (dataset, order, convert variant); every valve exports
that overview. Runs wait for the 1-minute load average to drop below a tenth
of the core count, and at least 3 (other
sessions share this box) and log it after.

    python3 benchmarks/coalesce/run_matrix.py --stage G0 \\
        --bin target/release/tylertoo --data ~/bench-data/745 \\
        --out ~/bench-data/745/results

Results append to <out>/results.jsonl, one JSON object per run.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

# Spelled for the #745 defaults (density budget on points only, placeholder
# squares, merge valve). The G0-G2 results in RESULTS.md were taken with the
# pre-default spellings of the same settings (`--density-drop-kinds points
# --collapse-square` for C1); the overview files are the same.
CONVERTS = {
    # The defaults before #745: the density budget thins every kind, polygon
    # thinning is on, and polygons too small for a level are dropped.
    "C0": ["--drop-polygons", "--drop-lines", "--polygon-thinning", "1", "--no-collapse"],
    # The #745 default as first measured: the budget thins points only;
    # polygons too small for a level, or thinned out, become placeholder
    # squares. Polygon thinning was still on then.
    "C1": ["--polygon-thinning", "1"],
    # C1, plus no polygon visibility gate and no polygon thinning: every
    # polygon reaches every level, simplified for it.
    "C2": ["--polygon-visibility", "0", "--polygon-thinning", "0"],
    # C1t with a tighter simplification tolerance: closes the slivers between
    # neighbouring polygons that each simplify on their own.
    "C1s": ["--simplify-factor", "0.25"],
    # C1t with no simplification: only the tile grid's quantization reduces
    # vertices, and it snaps shared borders identically.
    "C1z": ["--simplify-factor", "0"],
    # The shipped default: C1 without polygon thinning. The size gate still
    # applies, but no polygon loses its cell to a neighbour whose bbox
    # centre lands there.
    "C1t": [],
}

VALVES = ["drop", "merge"]

STAGES = {
    "G0": {"datasets": ["madagascar"], "orders": ["hilbert"], "converts": ["C0", "C2"],
           "valves": VALVES, "wall_s": 120, "rss_gib": 8, "max_zoom": 12,
           "determinism": True},
    "G1": {"datasets": ["moldova"], "orders": ["hilbert"], "converts": ["C0", "C1", "C2"],
           "valves": VALVES, "wall_s": 600, "rss_gib": 24, "max_zoom": 12},
    # Sort order: today against the new default, per order.
    "G2": {"datasets": ["moldova"], "orders": ["hilbert", "str", "unsorted", "as-delivered"],
           "cells": [("C0", "drop"), ("C1", "merge")], "wall_s": 600, "rss_gib": 24,
           "max_zoom": 12},
    # Scaling ladder: Hilbert prefixes of Mato Grosso (prepare_data.py
    # --prefixes), today against the new default.
    "L": {"datasets": ["mato-grosso"],
          "orders": ["hilbert-250000", "hilbert-500000", "hilbert-1000000",
                     "hilbert-2000000", "hilbert"],
          "cells": [("C0", "drop"), ("C1", "merge")], "wall_s": 1800, "rss_gib": 96,
          "max_zoom": 12},
    "G3": {"datasets": ["nz-parcels"], "orders": ["hilbert"],
           "cells": [("C0", "drop"), ("C1", "merge")], "wall_s": 2700, "rss_gib": 96,
           "max_zoom": 14},
    # NZ re-timing on a quiet host, with the tighter-simplification recipe.
    "G3t": {"datasets": ["nz-parcels"], "orders": ["hilbert"],
            "cells": [("C0", "drop"), ("C1", "merge"), ("C1s", "merge"), ("C1z", "merge")],
            "wall_s": 2700,
            "rss_gib": 96, "max_zoom": 14},
}

def default_load_max(cores: int | None) -> float:
    """A tenth of the cores, at least 3: one compile on a big host does not
    hold every run, while a competing benchmark still does."""
    return max(3.0, 0.1 * (cores or 0))


LOAD_MAX = default_load_max(os.cpu_count())
LOAD_WAIT_S = 1800


def loadavg() -> float:
    return float(Path("/proc/loadavg").read_text().split()[0])


def wait_for_quiet() -> tuple[float, float]:
    t0 = time.monotonic()
    while loadavg() >= LOAD_MAX and time.monotonic() - t0 < LOAD_WAIT_S:
        time.sleep(10)
    return loadavg(), round(time.monotonic() - t0, 1)


def tree_rss_kib(pid: int) -> int:
    """Resident KiB of pid and every descendant, walking /proc/<pid>/task/*/
    children so the cost is the tree's size, not the whole process table."""
    total, stack = 0, [pid]
    while stack:
        p = stack.pop()
        try:
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    total += int(line.split()[1])
                    break
            for task in Path(f"/proc/{p}/task").iterdir():
                stack.extend(int(c) for c in (task / "children").read_text().split())
        except (OSError, ValueError):
            pass
    return total


def gnu_elapsed_s(stderr: str) -> float | None:
    """GNU time's wall clock, `h:mm:ss` or `m:ss.ss`."""
    m = re.search(r"Elapsed \(wall clock\) time \(h:mm:ss or m:ss\):\s*([\d:.]+)", stderr)
    if not m:
        return None
    secs = 0.0
    for part in m.group(1).split(":"):
        secs = secs * 60 + float(part)
    return secs


def run_capped(argv: list[str], wall_s: float, rss_gib: float, env: dict | None = None) -> dict:
    """Run argv under GNU time with a wall and an RSS cap. A cap kills the
    whole process group and is reported, not raised."""
    cap_kib = int(rss_gib * 1024 * 1024)
    t0 = time.monotonic()
    proc = subprocess.Popen(
        ["/usr/bin/time", "-v", *argv], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env, start_new_session=True,
    )
    outcome = {"status": None, "peak_poll_kib": 0}

    def watch():
        while proc.poll() is None:
            rss = tree_rss_kib(proc.pid)
            outcome["peak_poll_kib"] = max(outcome["peak_poll_kib"], rss)
            elapsed = time.monotonic() - t0
            reason = "RSS_CAP" if rss > cap_kib else "TIMEOUT" if elapsed > wall_s else None
            if reason:
                outcome["status"] = reason
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                return
            time.sleep(0.1)

    watcher = threading.Thread(target=watch, daemon=True)
    watcher.start()
    stdout, stderr = proc.communicate()
    wall_poll = time.monotonic() - t0
    watcher.join()
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", stderr)
    user = re.search(r"User time \(seconds\):\s*([\d.]+)", stderr)
    sys_t = re.search(r"System time \(seconds\):\s*([\d.]+)", stderr)
    status = outcome["status"] or ("ok" if proc.returncode == 0 else "FAILED")
    wall = gnu_elapsed_s(stderr) if status == "ok" else None
    return {
        "argv": argv,
        "status": status,
        "wall_s": round(wall if wall is not None else wall_poll, 3),
        "cpu_s": round(float(user.group(1)) + float(sys_t.group(1)), 2) if user and sys_t else None,
        "peak_rss_kib": int(m.group(1)) if m and status == "ok" else outcome["peak_poll_kib"],
        "stderr_tail": stderr[-3000:] if status != "ok" else None,
    }


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def last_json_line(path: Path) -> dict | None:
    try:
        lines = [ln for ln in path.read_text().splitlines() if ln.strip()]
        return json.loads(lines[-1]) if lines else None
    except (OSError, json.JSONDecodeError):
        return None


def zoom_summary(report: dict) -> list[dict]:
    keep = ("zoom", "tile_count", "tile_feature_count", "oversized_tiles", "merged_features",
            "valve_dropped_features", "max_valve_passes", "valve_passes")
    return [{k: z.get(k) for k in keep} for z in report.get("zooms", [])]


def convert_argv(binary: str, src: Path, ov: Path, max_zoom: int, report: Path,
                 flags: list[str]) -> list[str]:
    return [binary, "overview", str(src), str(ov), "--min-zoom", "0",
            "--max-zoom", str(max_zoom), "--force", "--report", str(report), *flags]


def export_argv(binary: str, ov: Path, pm: Path, max_tile_size: str,
                max_compressed_tile_size: str, valve: str, report: Path | None) -> list[str]:
    argv = [binary, "export-pmtiles", str(ov), str(pm), "--force",
            "--max-tile-size", str(max_tile_size),
            "--max-compressed-tile-size", str(max_compressed_tile_size),
            "--size-valve", valve]
    return [*argv, "--report", str(report)] if report else argv


def run_cell(binary: str, src: Path, out: Path, name: str, convert_flags: list[str],
             valves: list[str], *, max_zoom: int, max_tile_size: str,
             max_compressed_tile_size: str, wall_s: float, rss_gib: float,
             repeats: int = 1, convert_repeats: int = 1, keep: bool = False,
             determinism_check: bool = False, prep_s: float = 0.0,
             record=lambda rec: None) -> list[dict]:
    """Convert src once per convert repeat (the last overview is kept), then
    export it `repeats` times per valve. Every run is passed to `record` and
    returned: one `convert` record, then one `export` record per repeat. Files
    are `<out>/<name>.*`; `prep_s` is the input's preparation time, added to
    `total_with_prep_s`."""
    ov = out / f"{name}.overview.parquet"
    convert_runs = []
    for _ in range(convert_repeats):
        # Every repeat starts from the same state: overwriting the previous
        # repeat's overview with --force costs up to 3x the wall time at the
        # same CPU time (NZ: 8 s against 24 s).
        ov.unlink(missing_ok=True)
        subprocess.run(["sync"])
        load_before, waited = wait_for_quiet()
        conv = run_capped(
            convert_argv(binary, src, ov, max_zoom, out / f"{name}.convert.json", convert_flags),
            wall_s, rss_gib,
        )
        conv.update(load_before=load_before, load_wait_s=waited, load_after=loadavg(),
                    overview_bytes=ov.stat().st_size if ov.exists() else None)
        print(f"[convert] {name}: {conv['status']} {conv['wall_s']}s "
              f"{(conv['peak_rss_kib'] or 0) / 1048576:.2f} GiB", flush=True)
        convert_runs.append(conv)
        if conv["status"] != "ok":
            break
    conv = convert_runs[-1]
    records = [{"kind": "convert", "convert_run": conv, "convert_runs": convert_runs}]
    record(records[-1])
    if conv["status"] != "ok":
        if not keep:
            ov.unlink(missing_ok=True)
        return records
    for valve in valves:
        pm = out / f"{name}.{valve}.pmtiles"
        for rep in range(repeats):
            report = out / f"{name}.{valve}.report.json"
            profile = out / f"{name}.{valve}.{rep}.profile.jsonl"
            profile.unlink(missing_ok=True)
            subprocess.run(["sync"])
            load_before, waited = wait_for_quiet()
            env = dict(os.environ, TYLERTOO_PROFILE_JSON=str(profile))
            exp = run_capped(
                export_argv(binary, ov, pm, max_tile_size, max_compressed_tile_size, valve,
                            report),
                wall_s, rss_gib, env=env,
            )
            exp.update(load_before=load_before, load_wait_s=waited, load_after=loadavg())
            rec = {"kind": "export", "valve": valve, "repeat": rep, "convert_run": conv,
                   "export_run": exp}
            if exp["status"] == "ok":
                rep_json = json.loads(report.read_text())
                rec["pmtiles_bytes"] = pm.stat().st_size
                rec["pmtiles_sha256"] = sha256(pm)
                rec["zooms"] = zoom_summary(rep_json)
                rec["merged_features"] = rep_json.get("merged_features")
                rec["valve_dropped_features"] = rep_json.get("valve_dropped_features")
                rec["oversized_tiles"] = rep_json.get("oversized_tiles")
                rec["profile"] = last_json_line(profile)
                rec["total_s"] = round(conv["wall_s"] + exp["wall_s"], 3)
                rec["total_with_prep_s"] = round(rec["total_s"] + prep_s, 3)
                if determinism_check and rep == 0:
                    rec["determinism"] = determinism(
                        binary, ov, pm, valve, max_tile_size, max_compressed_tile_size,
                        wall_s, rss_gib, rec["pmtiles_sha256"])
            print(f"[export] {name}/{valve}#{rep}: {exp['status']} "
                  f"{exp['wall_s']}s {(exp['peak_rss_kib'] or 0) / 1048576:.2f} GiB "
                  f"merged={rec.get('merged_features')} "
                  f"dropped={rec.get('valve_dropped_features')}", flush=True)
            records.append(rec)
            record(rec)
    # The PMTiles stay for tile_metrics.py; the overview, which can be many GB
    # at scale, does not.
    if not keep:
        ov.unlink(missing_ok=True)
    return records


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--stage", required=True, choices=sorted(STAGES))
    ap.add_argument("--bin", type=Path, required=True)
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--datasets", help="override the stage's datasets (comma list)")
    ap.add_argument("--orders", help="override the stage's orders")
    ap.add_argument("--converts", help="override the stage's convert variants")
    ap.add_argument("--valves", help="override the stage's valves")
    ap.add_argument("--max-zoom", type=int)
    ap.add_argument("--max-tile-size", default="500K", help="raw MVT cap (export-pmtiles)")
    ap.add_argument("--max-compressed-tile-size", default="0", help="gzip cap; 0 = none")
    ap.add_argument("--repeats", type=int, default=1, help="timed export repeats per cell")
    ap.add_argument("--keep", action="store_true", help="keep overview and PMTiles outputs")
    args = ap.parse_args()

    stage = dict(STAGES[args.stage])
    for key in ("datasets", "orders", "converts", "valves"):
        if getattr(args, key):
            stage[key] = getattr(args, key).split(",")
    # A stage either crosses converts with valves, or names its cells.
    if "cells" in stage and not (args.converts or args.valves):
        cells = stage["cells"]
    else:
        cells = [(c, v) for c in stage.get("converts", []) for v in stage.get("valves", VALVES)]
    stage["converts"] = list(dict.fromkeys(c for c, _ in cells))
    stage["valves_for"] = {c: [v for cc, v in cells if cc == c] for c in stage["converts"]}
    max_zoom = args.max_zoom or stage["max_zoom"]
    data = args.data.expanduser()
    out = args.out.expanduser() / args.stage
    out.mkdir(parents=True, exist_ok=True)
    prep = json.loads((data / "prep.json").read_text()) if (data / "prep.json").exists() else {}
    results_path = args.out.expanduser() / "results.jsonl"
    binary = str(args.bin.resolve())
    git = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True, cwd=Path(__file__).parent).stdout.strip()

    def record(rec: dict) -> None:
        rec.update(stage=args.stage, git=git, max_zoom=max_zoom,
                   max_tile_size=args.max_tile_size,
                   max_compressed_tile_size=args.max_compressed_tile_size)
        with results_path.open("a") as f:
            f.write(json.dumps(rec) + "\n")

    for ds in stage["datasets"]:
        for order in stage["orders"]:
            src = data / ds / f"{order}.parquet"
            if not src.exists():
                print(f"[skip] {src} missing (run prepare_data.py)", flush=True)
                continue
            prep_rec = prep.get(f"{ds}/{order}", {})
            for cv in stage["converts"]:
                base = {"cell": f"{ds}/{order}/{cv}", "dataset": ds, "order": order,
                        "convert": cv, "prep": prep_rec}
                run_cell(binary, src, out, f"{ds}.{order}.{cv}", CONVERTS[cv],
                         stage["valves_for"][cv], max_zoom=max_zoom,
                         max_tile_size=args.max_tile_size,
                         max_compressed_tile_size=args.max_compressed_tile_size,
                         wall_s=stage["wall_s"], rss_gib=stage["rss_gib"],
                         repeats=args.repeats, keep=args.keep,
                         determinism_check=stage.get("determinism", False),
                         prep_s=prep_rec.get("wall_s") or 0.0,
                         record=lambda rec, base=base: record({**base, **rec}))
    return 0


def determinism(binary: str, ov: Path, pm: Path, valve: str, max_tile_size: str,
                max_compressed_tile_size: str, wall_s: float, rss_gib: float,
                first: str) -> dict:
    """Re-export twice more (same threads, then one thread) and compare."""
    res = {}
    for label, threads in (("again", None), ("one_thread", "1")):
        other = pm.with_suffix(f".{label}.pmtiles")
        env = dict(os.environ)
        if threads:
            env["RAYON_NUM_THREADS"] = threads
        run = run_capped(
            export_argv(binary, ov, other, max_tile_size, max_compressed_tile_size, valve, None),
            wall_s * 4, rss_gib, env=env,
        )
        res[label] = run["status"] == "ok" and sha256(other) == first
        other.unlink(missing_ok=True)
    return res

if __name__ == "__main__":
    sys.exit(main())
