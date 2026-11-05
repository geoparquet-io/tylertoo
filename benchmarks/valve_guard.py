#!/usr/bin/env python3
"""valve_guard.py: wall and peak-RSS guard for the size valve (#792, #772).

Extends run_manifest.py (it imports that file's runner, quiet-host gate and
GNU-time parsing) with the cells the size valve regressed on, which no other
gate measures:

  792-def, 792-nocap, 792-drop, 792-dropnocap   export of the #792 overview
                                               (1.36M Bavarian buildings),
                                               mimalloc CLI
  792-def-glibc, 792-drop-glibc                the same on a glibc build
                                               (`--no-default-features`)
  moldova-f0                                   export of Moldova at
                                               --simplify-factor 0 (#772)
  moldova-tiles                                `tiles` z0-12

Two kinds of failure, both exit status 1:

  ratio     merge:drop on #792 (792-def over 792-drop, and the glibc pair)
            above RATIO_WALL_MAX (1.7x) wall or RATIO_RSS_MAX (1.5x) peak RSS.
            Needs no baseline and does not depend on the machine. On 6c3cec4
            it read 2.0x wall and 3.1x glibc RSS; with the valve fixes about
            1.6x and under 1.2x.
  baseline  a cell more than 15% slower or 20% heavier than its entry in
            benchmarks/baseline/valve_guard.json. Only compared when the
            baseline's host matches this one (CPU model and count): wall and
            RSS from another machine mean nothing, so a different host gets
            the ratio check only, and says so.

    uv run python -I benchmarks/valve_guard.py check \\
        --mim target/release/tylertoo --glibc target/glibc/release/tylertoo \\
        --data ~/bench-data
    ... record   (same flags) rewrites the baseline for this host

Inputs under --data: 792/buildings.parquet (benchmarks/prepare_792.py makes
it from the public download) and coalesce/inputs/moldova-hilbert.parquet.
Standard library only.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASELINE = HERE / "baseline" / "valve_guard.json"

RATIO_WALL_MAX = 1.7
RATIO_RSS_MAX = 1.5
WALL_REGRESSION_MAX = 1.15
RSS_REGRESSION_MAX = 1.20

SIZE_ARGS = ["--max-tile-size", "2000000", "--feature-order", "_size:desc"]
OVERVIEW_792 = ["--max-zoom", "14", "--no-density-drop", "--polygon-visibility", "0",
                "--polygon-thinning", "0", "--collapse-square", "--sort-key", "_size"]

# cell -> (binary kind, input kind, extra export args)
CELLS = {
    "792-def": ("mim", "ov792", []),
    "792-nocap": ("mim", "ov792", ["--max-compressed-tile-size", "0"]),
    "792-drop": ("mim", "ov792", ["--size-valve", "drop"]),
    "792-dropnocap": ("mim", "ov792", ["--max-compressed-tile-size", "0", "--size-valve", "drop"]),
    "792-def-glibc": ("glibc", "ov792", []),
    "792-drop-glibc": ("glibc", "ov792", ["--size-valve", "drop"]),
    "moldova-f0": ("mim", "movf0", []),
    "moldova-tiles": ("mim", "moldova", []),
}
RATIO_PAIRS = [("792-def", "792-drop"), ("792-def-glibc", "792-drop-glibc")]


def load_run_manifest():
    spec = importlib.util.spec_from_file_location("run_manifest", HERE / "run_manifest.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["run_manifest"] = mod
    spec.loader.exec_module(mod)
    return mod


def host_key() -> dict:
    model = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith(("model name", "Model name", "CPU part")):
                model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    return {"machine": platform.machine(), "cpu_model": model, "cpus": os.cpu_count()}


def command(cell: str, bins: dict, work: Path, prepared: dict) -> list[str]:
    kind, inp, extra = CELLS[cell]
    exe = str(bins[kind])
    out = str(work / "out.pmtiles")
    if inp == "moldova":
        return [exe, "tiles", prepared["moldova"], out, "--max-zoom", "12"]
    size_args = SIZE_ARGS if inp == "ov792" else []
    return [exe, "export-pmtiles", prepared[inp], out, *size_args, *extra]


def prepare(bins: dict, data: Path, work: Path) -> dict:
    """The untimed overviews: #792 at its benchmark settings, Moldova at factor 0."""
    b792 = data / "792" / "buildings.parquet"
    moldova = data / "coalesce" / "inputs" / "moldova-hilbert.parquet"
    for p in (b792, moldova):
        if not p.exists():
            raise SystemExit(f"missing input {p}")
    ov792, movf0 = work / "ov792.parquet", work / "moldova-f0.parquet"
    mim = str(bins["mim"])
    if not ov792.exists():
        subprocess.run([mim, "overview", str(b792), str(ov792), *OVERVIEW_792], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not movf0.exists():
        subprocess.run([mim, "overview", str(moldova), str(movf0), "--max-zoom", "12",
                        "--simplify-factor", "0"], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return {"ov792": str(ov792), "movf0": str(movf0), "moldova": str(moldova)}


def measure(rm, cells: list[str], bins: dict, data: Path, rounds: int, gate) -> dict:
    caps = {"wall_s": 1800, "rss_gib": 32, "scratch_gib": 30}
    results: dict[str, list[dict]] = {c: [] for c in cells}
    with tempfile.TemporaryDirectory(prefix="valve-guard-", dir=os.environ.get("VALVE_GUARD_TMP")) as tmp:
        work = Path(tmp)
        prepared = prepare(bins, data, work)
        gate.last_own_end = time.monotonic()
        for rnd in range(rounds):
            order = cells if rnd % 2 == 0 else cells[::-1]  # drift cancels
            for cell in order:
                rm.require_disk(work, 10)
                gate.wait()
                argv = ["/usr/bin/time", "-v", *command(cell, bins, work, prepared)]
                run = rm.run_capped(argv, caps, dict(os.environ), work / "log.txt", work)
                gate.last_own_end = time.monotonic()  # our own load average is not a busy host
                (work / "out.pmtiles").unlink(missing_ok=True)
                subprocess.run(["sync"], check=False)
                if run["status"] != "ok":
                    raise SystemExit(f"{cell} round {rnd}: {run['status']}\n{run['stderr_tail']}")
                results[cell].append(run)
                print(f"  {cell:16} r{rnd} {run['wall_s']:7.2f} s {run['peak_rss_kib'] / 1048576:6.2f} GiB",
                      flush=True)
    return {c: {"wall_s": round(statistics.median(r["wall_s"] for r in rs), 3),
                "peak_rss_kib": int(statistics.median(r["peak_rss_kib"] for r in rs)),
                "peak_rss_min_kib": min(r["peak_rss_kib"] for r in rs),
                "rounds": len(rs)} for c, rs in results.items()}


def ratio_failures(cells: dict) -> list[str]:
    bad = []
    for merge, drop in RATIO_PAIRS:
        if merge not in cells or drop not in cells:
            continue
        wall = cells[merge]["wall_s"] / cells[drop]["wall_s"]
        rss = cells[merge]["peak_rss_kib"] / cells[drop]["peak_rss_kib"]
        print(f"  ratio {merge}:{drop}  wall {wall:.2f}x (max {RATIO_WALL_MAX})  "
              f"rss {rss:.2f}x (max {RATIO_RSS_MAX})")
        if wall > RATIO_WALL_MAX:
            bad.append(f"{merge}:{drop} wall {wall:.2f}x > {RATIO_WALL_MAX}x")
        if rss > RATIO_RSS_MAX:
            bad.append(f"{merge}:{drop} peak RSS {rss:.2f}x > {RATIO_RSS_MAX}x")
    return bad


def baseline_failures(cells: dict, baseline: dict) -> list[str]:
    if baseline.get("host") != host_key():
        print(f"  baseline recorded on {baseline.get('host')}, this host is {host_key()}: "
              "cell-by-cell comparison skipped, ratios only")
        return []
    bad = []
    for cell, now in cells.items():
        base = baseline["cells"].get(cell)
        if not base:
            print(f"  {cell}: no baseline entry")
            continue
        wall = now["wall_s"] / base["wall_s"]
        # Peak RSS only ever noises upward (Moldova factor 0 swings 4.6 to
        # 5.9 GiB between identical runs), so the smallest of the rounds is
        # compared with the baseline's smallest.
        rss = now["peak_rss_min_kib"] / base["peak_rss_min_kib"]
        print(f"  {cell:16} wall {wall:.2f}x  rss {rss:.2f}x of baseline")
        if wall > WALL_REGRESSION_MAX:
            bad.append(f"{cell} wall {now['wall_s']} s is {wall:.2f}x its baseline {base['wall_s']} s")
        if rss > RSS_REGRESSION_MAX:
            bad.append(f"{cell} peak RSS is {rss:.2f}x its baseline")
    return bad


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["check", "record"])
    ap.add_argument("--mim", type=Path, required=True, help="tylertoo built with the default (mimalloc)")
    ap.add_argument("--glibc", type=Path, required=True, help="tylertoo built with --no-default-features")
    ap.add_argument("--data", type=Path, default=Path.home() / "bench-data")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--cells", nargs="*", default=list(CELLS), choices=list(CELLS))
    ap.add_argument("--baseline", type=Path, default=BASELINE)
    ap.add_argument("--load-max", type=float, default=3.0)
    ap.add_argument("--json-out", type=Path, help="also write the measured cells here")
    args = ap.parse_args(argv)

    rm = load_run_manifest()
    bins = {"mim": args.mim.resolve(), "glibc": args.glibc.resolve()}
    for b in bins.values():
        if not b.exists():
            raise SystemExit(f"missing binary {b}")
    gate = rm.Gate(args.load_max, 3600, Path(tempfile.gettempdir()) / "valve-guard-gate.log")
    cells = measure(rm, args.cells, bins, args.data, args.rounds, gate)
    record = {"host": host_key(), "hostname": platform.node(), "measured": time.strftime("%F"), "cells": cells,
              "limits": {"ratio_wall": RATIO_WALL_MAX, "ratio_rss": RATIO_RSS_MAX,
                         "wall": WALL_REGRESSION_MAX, "rss": RSS_REGRESSION_MAX}}
    if args.json_out:
        args.json_out.write_text(json.dumps(record, indent=2) + "\n")
    if args.mode == "record":
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(json.dumps(record, indent=2) + "\n")
        print(f"wrote {args.baseline}")
        return 1 if ratio_failures(cells) else 0
    failures = ratio_failures(cells)
    if args.baseline.exists():
        failures += baseline_failures(cells, json.loads(args.baseline.read_text()))
    else:
        print(f"  no baseline at {args.baseline}: ratios only")
    for f in failures:
        print(f"FAIL {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
