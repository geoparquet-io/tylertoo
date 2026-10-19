#!/usr/bin/env python3
"""Summarize benchmarks/staging/sweep_out/sweep.csv as Markdown tables.

For each host, a concurrency x window grid of the median staging time
(seconds; requests in parentheses), then the HTTP/1.1-vs-HTTP/2 cells and
the whole-object baselines. Medians are over the N repetitions.

Usage:
  python3 summarize.py [sweep.csv]
"""
import csv
import os
import statistics
import sys
from collections import defaultdict

MIB = 1024 * 1024


def median(xs):
    xs = [float(x) for x in xs if x not in ("", None)]
    return statistics.median(xs) if xs else None


def fmt(x, digits=2):
    return "–" if x is None else f"{x:.{digits}f}"


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "sweep_out", "sweep.csv")
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))

    cells = defaultdict(list)
    for r in rows:
        key = (r["host"], r["grid"], int(r["concurrency"]), int(r["window"]), r["http"])
        cells[key].append(r)

    hosts = sorted({k[0] for k in cells if k[1] == "main"})
    for host in hosts:
        concs = sorted({k[2] for k in cells if k[0] == host and k[1] == "main"})
        windows = sorted({k[3] for k in cells if k[0] == host and k[1] == "main"})
        print(f"\n#### {host}: median staging seconds (requests)\n")
        head = ["concurrency"] + ["off" if w == 0 else f"{w // MIB} MiB" for w in windows]
        print("| " + " | ".join(head) + " |")
        print("|" + "---|" * len(head))
        for c in concs:
            line = [str(c)]
            for w in windows:
                rs = cells.get((host, "main", c, w, "1"), [])
                stage = median(r["stage_s"] for r in rs)
                reqs = median(r["requests"] for r in rs)
                live = {r["live_conc"] for r in rs if r["live_conc"]}
                cap = f" ×{min(map(int, live))}" if live and min(map(int, live)) < c else ""
                line.append(f"{fmt(stage, 1)} ({fmt(reqs, 0)}){cap}" if rs else "–")
            print("| " + " | ".join(line) + " |")

    http = [k for k in cells if k[1] == "http"]
    if http:
        print("\n#### HTTP/1.1 vs HTTP/2: median staging seconds\n")
        print("| host | concurrency | window | HTTP/1.1 | HTTP/2 |")
        print("|---|---|---|---|---|")
        for host, _, c, w, _ in sorted({(k[0], k[1], k[2], k[3], "") for k in http}):
            h1 = median(r["stage_s"] for r in cells.get((host, "http", c, w, "1"), []))
            h2 = median(r["stage_s"] for r in cells.get((host, "http", c, w, "2"), []))
            wl = "off" if w == 0 else f"{w // MIB} MiB"
            print(f"| {host} | {c} | {wl} | {fmt(h1, 1)} | {fmt(h2, 1)} |")

    base = [k for k in cells if k[1].startswith("baseline")]
    if base:
        print("\n#### Whole-object baselines: median wall seconds\n")
        print("| host | tool | seconds |")
        print("|---|---|---|")
        for k in sorted(base):
            print(f"| {k[0]} | {k[1].removeprefix('baseline-')} | {fmt(median(r['wall_s'] for r in cells[k]))} |")


if __name__ == "__main__":
    main()
