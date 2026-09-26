#!/usr/bin/env python3
"""Criterion baseline regression gate (#448).

Compares the current `target/criterion/**/new/estimates.json` snapshots
(written by every `cargo bench` invocation, with or without `--baseline`)
against a named baseline directory (default `main`) seeded from a prior run
on `main`, and fails when a bench's mean point estimate regressed past a
threshold.

Deliberately NOT wired into the required-checks "bench" job that runs on
every push/PR: timing on shared CI runners is noisy (see `ci_guard.py`'s own
comment on why *that* gate stays structural, not timing-based). This script
is the numeric half of a workflow that instead runs on a schedule or a PR
label — see `.github/workflows/bench.yml`'s `criterion-regression` job and
DEVELOPMENT.md's "Criterion regression gate" section.

Usage:
  criterion_gate.py --check                       # compare 'main' vs 'new', exit 1 on regression
  criterion_gate.py --check --baseline main --warn 10 --fail 25
  criterion_gate.py --check --criterion-dir target/criterion

Directory layout this reads (criterion's own, unmodified):
  target/criterion/<group>/<function>/<baseline>/estimates.json
  target/criterion/<group>/<function>/new/estimates.json
"""
import argparse
import json
import os
import sys
from typing import Optional


def load_point_estimate(path: str) -> Optional[float]:
    """The `mean.point_estimate` (nanoseconds) from a criterion estimates.json."""
    try:
        with open(path) as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return None
    try:
        return float(data["mean"]["point_estimate"])
    except (KeyError, TypeError, ValueError):
        return None


def find_benches(criterion_dir: str, baseline: str):
    """Yield (group, function, baseline_path, new_path) for every bench that
    has BOTH a `<baseline>/estimates.json` and a `new/estimates.json`.

    Criterion's own directory model (see `BenchmarkId::new` in
    `criterion::report`): a leaf bench directory is
    `<group>/<function-or-value>[/<value>]/<baseline-name>/estimates.json` —
    two path segments under the criterion root for `bench_function` and
    `bench_with_input(BenchmarkId::from_parameter(..))` (everything this
    repo's benches use), three for `BenchmarkId::new(function, value)`. This
    walks depth-first and yields the first leaf it finds under each subtree,
    so both shapes work without hardcoding a depth.
    """
    if not os.path.isdir(criterion_dir):
        return

    def walk(path: str, parts: list):
        base_est = os.path.join(path, baseline, "estimates.json")
        new_est = os.path.join(path, "new", "estimates.json")
        if os.path.exists(base_est) and os.path.exists(new_est):
            yield ("/".join(parts), base_est, new_est)
            return
        for entry in sorted(os.listdir(path)):
            if entry in ("report", baseline, "new", "base"):
                continue
            entry_path = os.path.join(path, entry)
            if os.path.isdir(entry_path):
                yield from walk(entry_path, parts + [entry])

    for group in sorted(os.listdir(criterion_dir)):
        group_path = os.path.join(criterion_dir, group)
        if group == "report" or not os.path.isdir(group_path):
            continue
        for name, base_est, new_est in walk(group_path, [group]):
            yield (name, base_est, new_est)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="run the gate (required)")
    ap.add_argument("--criterion-dir", default="target/criterion")
    ap.add_argument("--baseline", default="main", help="named baseline directory to compare against")
    ap.add_argument("--warn", type=float, default=10.0, help="warn threshold, percent regression")
    ap.add_argument("--fail", type=float, default=25.0, help="fail threshold, percent regression")
    args = ap.parse_args()

    if not args.check:
        ap.print_help()
        return 2

    rows = list(find_benches(args.criterion_dir, args.baseline))
    if not rows:
        print(
            f"criterion_gate: no benches found with both '{args.baseline}/' and 'new/' "
            f"estimates under {args.criterion_dir} — nothing to compare (first run, or "
            "baseline wasn't seeded). Not a failure.",
        )
        return 0

    warnings = []
    failures = []
    print(f"{'bench':<55} {'baseline (ns)':>16} {'new (ns)':>16} {'change':>9}")
    print("-" * 100)
    for name, base_path, new_path in rows:
        base_ns = load_point_estimate(base_path)
        new_ns = load_point_estimate(new_path)
        if base_ns is None or new_ns is None or base_ns <= 0:
            print(f"{name:<55} {'?':>16} {'?':>16} {'skip':>9}")
            continue
        pct = (new_ns - base_ns) / base_ns * 100.0
        flag = ""
        if pct >= args.fail:
            flag = "FAIL"
            failures.append((name, pct))
        elif pct >= args.warn:
            flag = "warn"
            warnings.append((name, pct))
        print(f"{name:<55} {base_ns:>16.0f} {new_ns:>16.0f} {pct:>8.1f}% {flag}")

    print()
    if warnings:
        print(f"WARN: {len(warnings)} bench(es) regressed >= {args.warn}%:")
        for name, pct in warnings:
            print(f"  - {name}: +{pct:.1f}%")
    if failures:
        print(f"FAIL: {len(failures)} bench(es) regressed >= {args.fail}%:")
        for name, pct in failures:
            print(f"  - {name}: +{pct:.1f}%")
        return 1

    print(f"OK: no bench regressed >= {args.fail}% against baseline '{args.baseline}'.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
