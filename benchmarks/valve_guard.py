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
            above RATIO_WALL_MAX (1.8x) wall or RATIO_RSS_MAX (1.5x) peak RSS.
            Needs no baseline. The limits were set from measurements at 64
            cores and at 4 cores (`taskset -c 0-3`, like ubuntu-latest), not
            assumed machine independent: the ratio moves with core count.
            Wall, merge:drop (mimalloc / glibc): 6c3cec4 2.04x / 2.02x at 64
            cores and 2.13x / 2.10x at 4; this head 1.64x / 1.55x at 64 and
            1.47x / 1.45x at 4. glibc peak RSS: 6c3cec4 2.85x (64) and 2.58x
            (4); head 1.26x and 0.99x. The wall limit sits between the two.
            On mimalloc the RSS ratio is about 1.0-1.3x for both sides, so
            only the glibc pair gates on RSS.
  baseline  a cell more than 15% slower or 20% heavier than its entry in
            benchmarks/baseline/valve_guard.json, which holds one baseline
            per host class (architecture, CPU model, CPU count). Wall and RSS
            from another class mean nothing, so on a host with no baseline
            of its class (ubuntu-latest, say) this half reports
            "baseline: NOT APPLICABLE", never a pass.

  cores     (#817) on a matched host class, the median effective cores of the
            convert pass 2 and export levels phases (moldova-tiles measures
            both) more than CORES_DROP_MAX (15%) below the baseline's fail
            (part of `baseline`). The pinned class's baseline entry may also
            hold `floors` {whole_cores_frac, serial_share_max}: whole-run
            cores under frac x CPUs, or export serial_share over the max,
            fail (`floors`). A cell is exempt only via `[cores_floor.<cell>]`
            in ci/exemptions.toml.

Every check ends with three lines: `ratio: PASS|FAIL`,
`baseline: PASS|FAIL|NOT APPLICABLE (<why>)` and
`floors: PASS|FAIL|NOT APPLICABLE (<why>)`; --json-out records all.

    uv run python -I benchmarks/valve_guard.py check \\
        --mim target/release/tylertoo --glibc target/glibc/release/tylertoo \\
        --data ~/bench-data
    ... record   (same flags) rewrites this host class's baseline, keeping
                 the other classes'

Inputs under --data: 792/buildings.parquet (benchmarks/prepare_792.py makes
it from the public download) and coalesce/inputs/moldova-hilbert.parquet.
Standard library only.
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tomllib
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run_manifest as rm  # noqa: E402
BASELINE = HERE / "baseline" / "valve_guard.json"

RATIO_WALL_MAX = 1.8
RATIO_RSS_MAX = 1.5
WALL_REGRESSION_MAX = 1.15
RSS_REGRESSION_MAX = 1.20
CORES_DROP_MAX = 0.15  # a phase's median effective cores may fall 15% below the baseline's
# Seeded into a recorded host class that has no `floors` yet; tighten them in
# the baseline file as #813 lands (they are data there, not constants here).
DEFAULT_FLOORS = {"whole_cores_frac": 0.05, "serial_share_max": 0.10}
CORE_PHASES = ("convert/pass2 (output sink)", "export/levels")
EXEMPTIONS = HERE.parent / "ci" / "exemptions.toml"

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


def host_class_key(host: dict) -> str:
    """The baseline file's key for a host class (run_manifest.host_class)."""
    return f"{host['machine']}/{host['cpu_model']}/{host['cpus']}"


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
    # Moldova is pinned in manifest.toml; the #792 file is made locally
    # (prepare_792.py), so its sha256 is recorded with the results instead.
    if not rm.verified(moldova, rm.load_manifest()["inputs"]["moldova"]["sha256"]):
        raise SystemExit(f"{moldova} does not match manifest.toml's moldova sha256")
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


def measure(cells: list[str], bins: dict, data: Path, rounds: int, gate) -> dict:
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
                if gate.wait().get("gate") == "TIMED_OUT":
                    raise SystemExit(f"{cell} round {rnd}: host stayed busy past the gate; "
                                     "nothing recorded")
                argv = ["/usr/bin/time", "-v", *command(cell, bins, work, prepared)]
                prof = work / "profile.jsonl"
                prof.unlink(missing_ok=True)
                run = rm.run_capped(argv, caps, {**os.environ, "TYLERTOO_PROFILE_JSON": str(prof)},
                                    work / "log.txt", work)
                run["profile"] = rm.read_profile(prof)
                gate.last_own_end = time.monotonic()  # our own load average is not a busy host
                (work / "out.pmtiles").unlink(missing_ok=True)
                subprocess.run(["sync"], check=False)
                if run["status"] != "ok":
                    raise SystemExit(f"{cell} round {rnd}: {run['status']}\n{run['stderr_tail']}")
                results[cell].append(run)
                print(f"  {cell:16} r{rnd} {run['wall_s']:7.2f} s {run['peak_rss_kib'] / 1048576:6.2f} GiB",
                      flush=True)
    return {c: cell_summary(rs) for c, rs in results.items()}


def cell_summary(rs: list[dict]) -> dict:
    """One cell's baseline record from its runs: medians of wall, RSS, whole-run
    cores, the pass-2 and levels phase cores, and the export serial share."""
    shares = [v for r in rs if (v := (r.get("profile", {}).get("export") or {}).get("serial_share")) is not None]
    return {"wall_s": round(statistics.median(r["wall_s"] for r in rs), 3),
            "peak_rss_kib": int(statistics.median(r["peak_rss_kib"] for r in rs)),
            "peak_rss_min_kib": min(r["peak_rss_kib"] for r in rs),
            "effective_cores": statistics.median(r["effective_cores"] or 0 for r in rs),
            "phase_cores": {k: v["median"] for k, v in rm.phase_cores(rs).items() if k in CORE_PHASES},
            "serial_share": statistics.median(shares) if shares else None,
            "rounds": len(rs)}


def ratio_failures(cells: dict) -> list[str]:  # noqa: D103
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
    """Cells over their limits against one host class's baseline record."""
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
        for phase, was in (base.get("phase_cores") or {}).items():
            got = (now.get("phase_cores") or {}).get(phase)
            if got is not None and was and got < was * (1 - CORES_DROP_MAX):
                bad.append(f"{cell} {phase} effective cores {got} is more than "
                           f"{CORES_DROP_MAX:.0%} below its baseline {was}")
    return bad


def load_exemptions(path: Path = EXEMPTIONS) -> set[str]:
    """Cells exempt from the floors: `[cores_floor.<cell>]` in ci/exemptions.toml."""
    return set(tomllib.loads(path.read_text()).get("cores_floor", {}))


def floor_failures(cells: dict, floors: dict, cpus: int, exempt) -> list[str]:
    """Cells under the pinned class's floors: whole-run cores as a fraction of
    the CPUs, and the export serial share."""
    bad = []
    need = floors["whole_cores_frac"] * cpus
    for cell, now in cells.items():
        if cell in exempt:
            continue
        whole, share = now.get("effective_cores"), now.get("serial_share")
        if whole is not None and whole < need:
            bad.append(f"{cell} effective cores {whole} < floor {need:g} "
                       f"({floors['whole_cores_frac']} x {cpus} CPUs)")
        if share is not None and share > floors["serial_share_max"]:
            bad.append(f"{cell} serial share {share} > {floors['serial_share_max']}")
    return bad


def verdict(cells: dict, baseline_doc: dict | None, host: dict, exempt=frozenset()) -> dict:
    """Three halves: ratio always; baseline (wall, RSS, phase cores) and floors
    only against a baseline recorded on this host class (floors also need its
    `floors` section), else `not applicable` and why."""
    ratio = ratio_failures(cells)
    out = {"host_class": host_class_key(host),
           "ratio": {"status": "fail" if ratio else "pass", "failures": ratio}}
    have = sorted((baseline_doc or {}).get("hosts", {}))
    base = (baseline_doc or {}).get("hosts", {}).get(host_class_key(host))
    if base is None:
        reason = (f"no baseline for host class {host_class_key(host)}; recorded classes: "
                  f"{', '.join(have) or 'none'}")
        out["baseline"] = {"status": "not applicable", "reason": reason, "failures": []}
        out["floors"] = {"status": "not applicable", "reason": reason, "failures": []}
    else:
        bad = baseline_failures(cells, base)
        out["baseline"] = {"status": "fail" if bad else "pass", "failures": bad,
                           "measured": base.get("measured")}
        if base.get("floors"):
            bad = floor_failures(cells, base["floors"], host["cpus"], exempt)
            out["floors"] = {"status": "fail" if bad else "pass", "failures": bad}
        else:
            out["floors"] = {"status": "not applicable", "failures": [],
                             "reason": f"host class {host_class_key(host)} has no floors section"}
    return out


def summary_lines(v: dict) -> list[str]:
    """`ratio: ...` and `baseline: ...`, the two lines a check ends with."""
    def line(name: str, h: dict) -> str:
        why = f" ({h['reason']})" if h["status"] == "not applicable" else ""
        return f"{name}: {h['status'].upper()}{why}"
    return [f"ratio: {v['ratio']['status'].upper()}", line("baseline", v["baseline"]),
            line("floors", v["floors"])]


def exit_status(v: dict) -> int:
    """1 when any half failed; `not applicable` is not a failure."""
    return 1 if v["ratio"]["failures"] or v["baseline"]["failures"] or v["floors"]["failures"] else 0


def with_record(baseline_doc: dict | None, record: dict) -> dict:
    """The baseline file with this host class's entry replaced; the class's
    hand-tuned `floors` survive a re-record."""
    hosts = dict((baseline_doc or {}).get("hosts", {}))
    old = hosts.get(host_class_key(record["host"]), {}).get("floors")
    if old and "floors" not in record:
        record = {**record, "floors": old}
    hosts[host_class_key(record["host"])] = record
    return {"hosts": hosts}


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

    bins = {"mim": args.mim.resolve(), "glibc": args.glibc.resolve()}
    for b in bins.values():
        if not b.exists():
            raise SystemExit(f"missing binary {b}")
    gate = rm.Gate(args.load_max, 3600, Path(tempfile.gettempdir()) / "valve-guard-gate.log")
    cells = measure(args.cells, bins, args.data, args.rounds, gate)
    record = {"host": rm.host_class(),
              "inputs": {"792": rm.sha256_file(args.data / "792" / "buildings.parquet"),
                         "moldova": rm.load_manifest()["inputs"]["moldova"]["sha256"]},
              "hostname": platform.node(), "measured": time.strftime("%F"), "cells": cells,
              "limits": {"ratio_wall": RATIO_WALL_MAX, "ratio_rss": RATIO_RSS_MAX,
                         "wall": WALL_REGRESSION_MAX, "rss": RSS_REGRESSION_MAX}}
    record["limits"]["cores_drop"] = CORES_DROP_MAX
    doc = json.loads(args.baseline.read_text()) if args.baseline.exists() else None
    if args.mode == "record" and not (doc or {}).get("hosts", {}).get(host_class_key(record["host"]), {}).get("floors"):
        record["floors"] = DEFAULT_FLOORS
    if args.mode == "record":
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(json.dumps(with_record(doc, record), indent=2) + "\n")
        print(f"wrote {args.baseline} [{host_class_key(record['host'])}]")
        doc = with_record(doc, record)
    v = verdict(cells, doc, record["host"], load_exemptions())
    if args.json_out:
        args.json_out.write_text(json.dumps({**record, "verdict": v}, indent=2) + "\n")
    for f in v["ratio"]["failures"] + v["baseline"]["failures"] + v["floors"]["failures"]:
        print(f"FAIL {f}")
    for line in summary_lines(v):
        print(line)
    return exit_status(v)


if __name__ == "__main__":
    sys.exit(main())
