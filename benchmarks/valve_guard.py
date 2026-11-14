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
            in ci/exemptions.toml. On the pinned class (PINNED_CLASS) a
            baseline lacking a field, cell or floor the gate checks fails;
            elsewhere it is NOT APPLICABLE.

Every check ends with FAIL lines, SKIP lines (a check that could not run,
e.g. CPU core-seconds when the output changed), then three lines: `ratio: PASS|FAIL`,
`baseline: PASS|FAIL|NOT APPLICABLE (<why>)` and
`floors: PASS|FAIL|NOT APPLICABLE (<why>)`; --json-out records all.

    uv run --no-project --python 3.12 python -I benchmarks/valve_guard.py check \\
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
# moldova-f0's wall swings 9.4-12.6 s between identical runs at flat CPU
# core-seconds (levels cores 21-29, the valve tail's scheduling), so 1.15x trips
# on noise; its CPU core-seconds check still catches added work.
WALL_REGRESSION_MAX_BY_CELL = {"moldova-f0": 1.40}
RSS_REGRESSION_MAX = 1.20
CORES_DROP_MAX = 0.15  # a phase's median effective cores may fall 15% below the baseline's
# Seeded into a recorded host class that has no `floors` yet; tighten them in
# the baseline file as core use rises (#876); they are data there, not here.
DEFAULT_FLOORS = {"whole_cores_frac": 0.05, "serial_share_max": 0.10}
SPEEDUP_DROP_MAX = 0.15  # 16-to-full-thread speedup may fall 15% below the baseline's
CPU_RISE_MAX = 0.25      # a phase's CPU core-seconds may rise 25% over the baseline's (same output)
LOW_THREADS = 16
# The cells also run at RAYON_NUM_THREADS=LOW_THREADS. 792-def: export levels is
# where parallelism regressed most often, and its effective cores hide spin.
SCALING_CELLS = ("moldova-tiles", "792-def")
CPU_MIN_BASELINE_S = 0.05  # a shorter phase's core-seconds are noise: skipped, said so
CORE_PHASES = ("convert/pass2 (output sink)", "export/levels")
EXEMPTIONS = HERE.parent / "ci" / "exemptions.toml"
# The self-hosted `tylertoo-bench` runner's class (valve-guard.yml). Here a
# baseline lacking a field the gate checks fails instead of reading NOT
# APPLICABLE, so a stale baseline cannot switch the #817 gate off.
PINNED_CLASS = "aarch64/0xd40/64"
RECORD_CMD = ("uv run --no-project --python 3.12 python -I benchmarks/valve_guard.py record "
              "--mim <release tylertoo> --glibc <glibc tylertoo> --data ~/bench-data "
              "(context/PROFILING.md, effective-cores gate)")

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


def prepare(bins: dict, data: Path, work: Path, cells: list[str]) -> dict:
    """The untimed overviews the chosen cells need: #792 at its benchmark settings,
    Moldova at factor 0."""
    kinds = {CELLS[c][1] for c in cells}
    b792 = data / "792" / "buildings.parquet"
    moldova = data / "coalesce" / "inputs" / "moldova-hilbert.parquet"
    for p in ([b792] if "ov792" in kinds else []) + [moldova]:
        if not p.exists():
            raise SystemExit(f"missing input {p}")
    # Moldova is pinned in manifest.toml; the #792 file is made locally
    # (prepare_792.py), so its sha256 is recorded with the results instead.
    if not rm.verified(moldova, rm.load_manifest()["inputs"]["moldova"]["sha256"]):
        raise SystemExit(f"{moldova} does not match manifest.toml's moldova sha256")
    ov792, movf0 = work / "ov792.parquet", work / "moldova-f0.parquet"
    mim = str(bins["mim"])
    if "ov792" in kinds and not ov792.exists():
        subprocess.run([mim, "overview", str(b792), str(ov792), *OVERVIEW_792], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if "movf0" in kinds and not movf0.exists():
        subprocess.run([mim, "overview", str(moldova), str(movf0), "--max-zoom", "12",
                        "--simplify-factor", "0"], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return {"ov792": str(ov792), "movf0": str(movf0), "moldova": str(moldova)}


def run_env(parent: dict, threads: int | None, prof: str) -> dict:
    """The child env: RAYON_NUM_THREADS set explicitly per arm (`threads`) or
    removed, so a parent's limit never reaches a run unannounced."""
    env = {k: v for k, v in parent.items() if k != "RAYON_NUM_THREADS"}
    env["TYLERTOO_PROFILE_JSON"] = prof
    if threads:
        env["RAYON_NUM_THREADS"] = str(threads)
    return env


def refuse_thread_env(mode: str, inject: int | None, parent: dict) -> None:
    """record never runs with a thread limit; check takes one only as --inject-threads."""
    if mode == "record" and inject:
        raise SystemExit("--inject-threads is check-only: a recorded baseline must be unrestricted")
    if "RAYON_NUM_THREADS" in parent:
        raise SystemExit("RAYON_NUM_THREADS is set in the environment; unset it (the guard sets it "
                         "per arm). To demonstrate a failure use `check --inject-threads N`")


def measure(cells: list[str], bins: dict, data: Path, rounds: int, gate,
            inject_threads: int | None = None) -> dict:
    caps = {"wall_s": 1800, "rss_gib": 32, "scratch_gib": 30}
    results: dict[str, list[dict]] = {c: [] for c in cells}
    low: dict[str, list[dict]] = {c: [] for c in cells if c in SCALING_CELLS}
    with tempfile.TemporaryDirectory(prefix="valve-guard-", dir=os.environ.get("VALVE_GUARD_TMP")) as tmp:
        work = Path(tmp)
        prepared = prepare(bins, data, work, cells)
        gate.last_own_end = time.monotonic()
        for rnd in range(rounds):
            order = cells if rnd % 2 == 0 else cells[::-1]  # drift cancels
            jobs = [(c, False) for c in order] + [(c, True) for c in order if c in low]
            for cell, is_low in jobs:
                threads = LOW_THREADS if is_low else inject_threads
                rm.require_disk(work, 10)
                if gate.wait().get("gate") == "TIMED_OUT":
                    raise SystemExit(f"{cell} round {rnd}: host stayed busy past the gate; "
                                     "nothing recorded")
                argv = ["/usr/bin/time", "-v", *command(cell, bins, work, prepared)]
                prof = work / "profile.jsonl"
                prof.unlink(missing_ok=True)
                run = rm.run_capped(argv, caps, run_env(os.environ, threads, str(prof)),
                                    work / "log.txt", work)
                if (work / "out.pmtiles").is_file():
                    run["output_sha256"] = rm.sha256_file(work / "out.pmtiles")
                run["profile"] = rm.read_profile(prof)
                gate.last_own_end = time.monotonic()  # our own load average is not a busy host
                (work / "out.pmtiles").unlink(missing_ok=True)
                subprocess.run(["sync"], check=False)
                if run["status"] != "ok":
                    raise SystemExit(f"{cell} round {rnd}: {run['status']}\n{run['stderr_tail']}")
                (low if is_low else results)[cell].append(run)
                print(f"  {cell:16} r{rnd} t{threads or 'all'} {run['wall_s']:7.2f} s {run['peak_rss_kib'] / 1048576:6.2f} GiB",
                      flush=True)
    return {c: cell_summary(rs, low.get(c)) for c, rs in results.items()}


def _phase(runs: list[dict], fn) -> tuple[dict, dict]:
    """({phase: median of fn(sampler phase)}, {phase: rounds that had it}) over
    the runs, CORE_PHASES only. A round counts only if fn gave a value."""
    vals: dict[str, list[float]] = {}
    for r in runs:
        for line in ("convert", "export"):
            for name, p in ((r.get("profile") or {}).get(line) or {}).get("phases", {}).items():
                if f"{line}/{name}" in CORE_PHASES and (v := fn(p)) is not None:
                    vals.setdefault(f"{line}/{name}", []).append(v)
    # six decimals: a short phase must not round to a zero baseline
    return ({k: round(statistics.median(v), 6) for k, v in vals.items()}, {k: len(v) for k, v in vals.items()})


def _cpu(p: dict) -> float | None:
    if p.get("cpu_user_secs") is None or p.get("cpu_sys_secs") is None:
        return None
    return p["cpu_user_secs"] + p["cpu_sys_secs"]


def _whole(p: dict) -> float | None:
    """A phase's wall, only when its cores and CPU were reported too."""
    return p.get("wall_secs") if p.get("effective_cores") is not None and _cpu(p) is not None else None


def cell_summary(rs: list[dict], low: list[dict] | None = None) -> dict:
    """One cell's baseline record from its runs: medians of wall, RSS, whole-run
    cores, the pass-2 and levels phase cores, wall and CPU core-seconds, and
    the export serial share, with how many rounds reported each (a gate must
    not pass on a sample thinner than the run). `low` are the same cell's runs
    at LOW_THREADS: they add `speedup_16_to_full` (low wall over full wall,
    whole run and per phase)."""
    shares = [v for r in rs if (v := (r.get("profile", {}).get("export") or {}).get("serial_share")) is not None]
    walls, counts = _phase(rs, _whole)
    out = {"wall_s": round(statistics.median(r["wall_s"] for r in rs), 3),
           "peak_rss_kib": int(statistics.median(r["peak_rss_kib"] for r in rs)),
           "peak_rss_min_kib": min(r["peak_rss_kib"] for r in rs),
           "effective_cores": statistics.median(r["effective_cores"] or 0 for r in rs),
           "phase_cores": {k: v["median"] for k, v in rm.phase_cores(rs).items() if k in CORE_PHASES},
           "phase_wall_s": walls, "phase_cpu_s": _phase(rs, lambda p: _cpu(p) if _whole(p) is not None else None)[0],
           "phase_rounds": counts,
           "serial_share": statistics.median(shares) if shares else None,
           "serial_share_rounds": len(shares),
           "output_sha256": rs[0].get("output_sha256"),
           "rounds": len(rs)}
    if low:
        lo_walls, lo_counts = _phase(low, _whole)
        lo = {"whole": statistics.median(r["wall_s"] for r in low), **lo_walls}
        hi = {"whole": statistics.median(r["wall_s"] for r in rs), **walls}
        out["speedup_16_to_full"] = {k: round(lo[k] / hi[k], 3) for k in hi if k in lo and hi[k]}
        out["low_threads"] = LOW_THREADS
        out["low_rounds"] = len(low)
        out["low_phase_rounds"] = lo_counts
    return out


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


def cell_phases(cell: str) -> tuple[str, ...]:
    """The phases `cell` measures: pass 2 only on `tiles`."""
    return CORE_PHASES if CELLS[cell][1] == "moldova" else ("export/levels",)


def missing_telemetry(cell: str, s: dict) -> list[str]:
    """What a complete record of `cell` lacks: each phase the cell measures
    in every round, and at LOW_THREADS too for a
    scaling cell. A gate must not pass, nor a baseline be kept, on less."""
    phases = cell_phases(cell)
    arms = [("", s.get("rounds", 0), s.get("phase_rounds"))]
    if cell in SCALING_CELLS:
        arms.append((f" {LOW_THREADS}-thread", s.get("low_rounds", 0), s.get("low_phase_rounds")))
    out = []
    for label, rounds, got in arms:
        for phase in phases:
            n = (got or {}).get(phase, 0)
            if n < rounds or n == 0:
                out.append(f"{cell} {phase}{label} telemetry missing in {rounds - n} of {rounds} rounds")
    return out


def baseline_gaps(cell: str, base: dict) -> list[str]:
    """The fields the gate compares that one baseline cell lacks."""
    phases = cell_phases(cell)
    gaps = [f"{k}[{p}]" for k in ("phase_cores", "phase_cpu_s") for p in phases
            if (base.get(k) or {}).get(p) is None]
    if cell in SCALING_CELLS:
        gaps += [f"speedup_16_to_full[{p}]" for p in ("whole", *phases)
                 if (base.get("speedup_16_to_full") or {}).get(p) is None]
    if not base.get("output_sha256"):
        gaps.append("output_sha256")
    return gaps or missing_telemetry(cell, base)


def record_problems(cells: dict) -> list[str]:
    """Why these measurements must not become a baseline (empty when complete)."""
    out = [m for cell, s in cells.items() for m in missing_telemetry(cell, s)]
    return out + [f"{cell} serial share telemetry missing in {s['rounds'] - s.get('serial_share_rounds', 0)} "
                  f"of {s['rounds']} rounds" for cell, s in cells.items()
                  if s.get("serial_share_rounds", 0) < s["rounds"]]


def baseline_failures(cells: dict, baseline: dict) -> tuple[list[str], list[str], list[str]]:
    """(failures, stale cells, skipped checks) of the cells against one host
    class's baseline record. The current run must be complete
    (`missing_telemetry`); an incomplete baseline cell (recorded before #817's
    fields) is `stale`."""
    bad, stale, skipped = [], [], []
    for cell, now in cells.items():
        base = baseline["cells"].get(cell)
        if not base:
            print(f"  {cell}: no baseline entry")
            stale.append(f"{cell}: no baseline entry")
            continue
        wall = now["wall_s"] / base["wall_s"]
        # Peak RSS only ever noises upward (Moldova factor 0 swings 4.6 to
        # 5.9 GiB between identical runs), so the smallest of the rounds is
        # compared with the baseline's smallest.
        rss = now["peak_rss_min_kib"] / base["peak_rss_min_kib"]
        print(f"  {cell:16} wall {wall:.2f}x  rss {rss:.2f}x of baseline")
        if wall > WALL_REGRESSION_MAX_BY_CELL.get(cell, WALL_REGRESSION_MAX):
            bad.append(f"{cell} wall {now['wall_s']} s is {wall:.2f}x its baseline {base['wall_s']} s")
        if rss > RSS_REGRESSION_MAX:
            bad.append(f"{cell} peak RSS is {rss:.2f}x its baseline")
        if gaps := baseline_gaps(cell, base):  # recorded before #817's fields, or incompletely
            stale.append(f"{cell}: {', '.join(gaps)}")
        bad += missing_telemetry(cell, now)
        for phase, was in (base.get("phase_cores") or {}).items():
            got = (now.get("phase_cores") or {}).get(phase)
            if got is not None and was is not None and got < was * (1 - CORES_DROP_MAX):
                bad.append(f"{cell} {phase} effective cores {got} is more than "
                           f"{CORES_DROP_MAX:.0%} below its baseline {was}")
        for phase, was in (base.get("speedup_16_to_full") or {}).items():
            got = (now.get("speedup_16_to_full") or {}).get(phase)
            if got is None:
                bad.append(f"{cell} {phase} {LOW_THREADS}-to-full speedup missing from this run")
            elif got < was * (1 - SPEEDUP_DROP_MAX):
                bad.append(f"{cell} {phase} {LOW_THREADS}-to-full speedup {got} is more than "
                           f"{SPEEDUP_DROP_MAX:.0%} below its baseline {was}")
        if not base.get("output_sha256") or base.get("output_sha256") != now.get("output_sha256"):
            skipped.append(f"{cell} CPU core-seconds: output differs from the baseline's (or no sha)")
        else:
            for phase, was in (base.get("phase_cpu_s") or {}).items():
                got = (now.get("phase_cpu_s") or {}).get(phase)
                if got is None:
                    bad.append(f"{cell} {phase} CPU core-seconds missing from this run")
                elif was < CPU_MIN_BASELINE_S:
                    skipped.append(f"{cell} {phase} CPU core-seconds: baseline {was} s is under "
                                   f"{CPU_MIN_BASELINE_S} s")
                elif got > was * (1 + CPU_RISE_MAX):
                    bad.append(f"{cell} {phase} CPU core-seconds {got} is more than "
                               f"{CPU_RISE_MAX:.0%} above its baseline {was}")
    return bad, stale, skipped


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
        if share is None:
            bad.append(f"{cell} serial share missing from this run (export profile not read)")
        elif now.get("serial_share_rounds", now.get("rounds", 0)) < now.get("rounds", 0):
            bad.append(f"{cell} serial share telemetry missing in "
                       f"{now['rounds'] - now['serial_share_rounds']} of {now['rounds']} rounds")
        elif share > floors["serial_share_max"]:
            bad.append(f"{cell} serial share {share} > {floors['serial_share_max']}")
    return bad


def verdict(cells: dict, baseline_doc: dict | None, host: dict, exempt=frozenset(),
            injected_threads: int | None = None) -> dict:
    """Three halves: ratio always; baseline (wall, RSS, phase cores) and floors
    only against a baseline recorded on this host class (floors also need its
    `floors` section), else `not applicable` and why."""
    ratio = ratio_failures(cells)
    out = {"host_class": host_class_key(host),
           "ratio": {"status": "fail" if ratio else "pass", "failures": ratio}}
    if injected_threads:
        out["injected_threads"] = injected_threads
    have = sorted((baseline_doc or {}).get("hosts", {}))
    base = (baseline_doc or {}).get("hosts", {}).get(host_class_key(host))
    pinned = host_class_key(host) == PINNED_CLASS
    if base is None:
        reason = (f"no baseline for host class {host_class_key(host)}; recorded classes: "
                  f"{', '.join(have) or 'none'}")
        if pinned:
            bad = [f"{reason}; the pinned class must have one. Record it: {RECORD_CMD}"]
            out["baseline"] = {"status": "fail", "failures": bad}
            out["floors"] = {"status": "fail", "failures": bad}
        else:
            out["baseline"] = {"status": "not applicable", "reason": reason, "failures": []}
            out["floors"] = {"status": "not applicable", "reason": reason, "failures": []}
        return out
    bad, stale, skipped = baseline_failures(cells, base)
    if stale and pinned:
        bad.append(f"the pinned class's baseline lacks fields the gate checks ({'; '.join(stale)}). "
                   f"Record them: {RECORD_CMD}")
    out["baseline"] = {"status": "fail" if bad else "pass", "failures": bad,
                       "measured": base.get("measured"), "skipped": skipped}
    if stale and not bad:
        out["baseline"].update(status="not applicable", reason=(
            f"baseline predates #817 fields ({'; '.join(stale)}); run record"))
    no_floors = [f"floors.{k}" for k in DEFAULT_FLOORS if k not in (base.get("floors") or {})]
    if not no_floors:
        bad = floor_failures(cells, base["floors"], host["cpus"], exempt)
        out["floors"] = {"status": "fail" if bad else "pass", "failures": bad}
    elif pinned:
        out["floors"] = {"status": "fail", "failures": [
            f"the pinned class's baseline lacks {', '.join(no_floors)}. Record them: {RECORD_CMD}"]}
    else:
        out["floors"] = {"status": "not applicable", "failures": [],
                         "reason": f"host class {host_class_key(host)} has no floors section"}
    return out


def summary_lines(v: dict) -> list[str]:
    """`ratio: ...` and `baseline: ...`, the two lines a check ends with."""
    def line(name: str, h: dict) -> str:
        why = f" ({h['reason']})" if h["status"] == "not applicable" else ""
        return f"{name}: {h['status'].upper()}{why}"
    inj = f" (INJECTED: full arm pinned to {v['injected_threads']} threads)" if v.get("injected_threads") else ""
    return [f"ratio: {v['ratio']['status'].upper()}{inj}", line("baseline", v["baseline"]),
            line("floors", v["floors"])]


def report_lines(v: dict) -> list[str]:
    """Everything a check prints at the end: FAIL and SKIP lines, then the summary."""
    fails = v["ratio"]["failures"] + v["baseline"]["failures"] + v["floors"]["failures"]
    return ([f"FAIL {f}" for f in fails] + [f"SKIP {s}" for s in v["baseline"].get("skipped", [])]
            + summary_lines(v))


def exit_status(v: dict) -> int:
    """1 when any half failed; `not applicable` is not a failure."""
    return 1 if v["ratio"]["failures"] or v["baseline"]["failures"] or v["floors"]["failures"] else 0


def with_record(baseline_doc: dict | None, record: dict) -> dict:
    """The baseline file with this host class's entry replaced; the class's
    hand-tuned `floors` survive a re-record, and a class without them gets
    DEFAULT_FLOORS."""
    hosts = dict((baseline_doc or {}).get("hosts", {}))
    old = hosts.get(host_class_key(record["host"]), {}).get("floors")
    record = {**record, "floors": old or DEFAULT_FLOORS}
    prev = hosts.get(host_class_key(record["host"]), {}).get("cells", {})
    hosts[host_class_key(record["host"])] = {**record, "cells": {**prev, **record["cells"]}}
    return {"hosts": hosts}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["check", "record"])
    ap.add_argument("--mim", type=Path, required=True, help="tylertoo built with the default (mimalloc)")
    ap.add_argument("--glibc", type=Path, help="tylertoo built with --no-default-features")
    ap.add_argument("--data", type=Path, default=Path.home() / "bench-data")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--cells", nargs="*", default=list(CELLS), choices=list(CELLS))
    ap.add_argument("--baseline", type=Path, default=BASELINE)
    ap.add_argument("--inject-threads", type=int, help="check only: pin the full-thread arm to N "
                    "threads (a failure demo); the verdict is labelled INJECTED")
    ap.add_argument("--load-max", type=float, default=3.0)
    ap.add_argument("--json-out", type=Path, help="also write the measured cells here")
    args = ap.parse_args(argv)

    bins = {"mim": args.mim.resolve(), **({"glibc": args.glibc.resolve()} if args.glibc else {})}
    if any(CELLS[c][0] == "glibc" for c in args.cells) and not args.glibc:
        raise SystemExit("--glibc is required for the -glibc cells (or pick --cells without them)")
    for b in bins.values():
        if not b.exists():
            raise SystemExit(f"missing binary {b}")
    gate = rm.Gate(args.load_max, 3600, Path(tempfile.gettempdir()) / "valve-guard-gate.log")
    refuse_thread_env(args.mode, args.inject_threads, os.environ)
    cells = measure(args.cells, bins, args.data, args.rounds, gate, args.inject_threads)
    record = {"host": rm.host_class(),
              "inputs": {"792": (rm.sha256_file(p) if (p := args.data / "792" / "buildings.parquet").exists() else None),
                         "moldova": rm.load_manifest()["inputs"]["moldova"]["sha256"]},
              "hostname": platform.node(), "measured": time.strftime("%F"), "cells": cells,
              "limits": {"ratio_wall": RATIO_WALL_MAX, "ratio_rss": RATIO_RSS_MAX,
                         "wall": WALL_REGRESSION_MAX, "rss": RSS_REGRESSION_MAX}}
    record["limits"].update(cores_drop=CORES_DROP_MAX, speedup_drop=SPEEDUP_DROP_MAX, cpu_rise=CPU_RISE_MAX)
    if args.inject_threads:
        record["injected_threads"] = args.inject_threads
    doc = json.loads(args.baseline.read_text()) if args.baseline.exists() else None
    if args.mode == "record":
        if problems := record_problems(cells):
            raise SystemExit("not recording an incomplete baseline:\n  " + "\n  ".join(problems))
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(json.dumps(with_record(doc, record), indent=2) + "\n")
        print(f"wrote {args.baseline} [{host_class_key(record['host'])}]")
        doc = with_record(doc, record)
    v = verdict(cells, doc, record["host"], load_exemptions(), args.inject_threads)
    if args.json_out:
        args.json_out.write_text(json.dumps({**record, "verdict": v}, indent=2) + "\n")
    for line in report_lines(v):
        print(line)
    return exit_status(v)


if __name__ == "__main__":
    sys.exit(main())
