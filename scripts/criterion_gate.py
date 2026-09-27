#!/usr/bin/env python3
"""Criterion baseline regression gate (#448).

Reads a criterion output directory produced by `scripts/criterion_compare.sh`
(base revision run with `--save-baseline <baseline>`, then the head run with
`--baseline-lenient <baseline>` after every stale `new/` and `change/` was
deleted) and fails on a gross per-bench regression.

Verdict per bench, from criterion's own `change/estimates.json` (the relative
change of the mean, with a bootstrap confidence interval):
  FAIL  the CI *lower bound* of the change is >= --fail  (we are confident the
        regression is at least that large, not just a noisy point estimate)
  warn  the point estimate is >= --warn
  ok    otherwise
A bench with a `new/` result but no `<baseline>/` result is reported as
"new (no baseline)" and does not fail. A bench with a baseline but no `new/`
result was not run at head (removed or renamed) and is reported, not failed.

Exits non-zero when no bench is comparable at all: a requested comparison that
compared nothing is a broken run, not a pass.

This is not a per-PR gate: shared-runner timing is noisy (see ci_guard.py for
why *that* guard stays structural). It runs from
.github/workflows/bench-regression.yml on dispatch or on a PR carrying the
`benchmark` label; see DEVELOPMENT.md's "Criterion regression gate".

Usage:
  criterion_gate.py --check --criterion-dir target/criterion-compare --baseline base
  criterion_gate.py --check ... --warn 10 --fail 25

Directory layout this reads (criterion's own):
  <criterion-dir>/<group>/<function>[/<value>]/{<baseline>,new,change}/estimates.json
"""
import argparse
import json
import os
import sys
from typing import Iterator, Optional, Tuple

RESULT_DIRS = ("new", "change", "base")


def load_json(path: str) -> Optional[dict]:
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return None


def mean_point(path: str) -> Optional[float]:
    """`mean.point_estimate` (ns) from a baseline/new estimates.json."""
    data = load_json(path)
    try:
        return float(data["mean"]["point_estimate"])  # type: ignore[index]
    except (KeyError, TypeError, ValueError):
        return None


def mean_change(path: str) -> Optional[Tuple[float, float, float]]:
    """(point, ci_lower, ci_upper) of the relative mean change, in percent."""
    data = load_json(path)
    try:
        m = data["mean"]  # type: ignore[index]
        ci = m["confidence_interval"]
        return (
            float(m["point_estimate"]) * 100.0,
            float(ci["lower_bound"]) * 100.0,
            float(ci["upper_bound"]) * 100.0,
        )
    except (KeyError, TypeError, ValueError):
        return None


def find_benches(criterion_dir: str, baseline: str) -> Iterator[Tuple[str, str]]:
    """Yield (name, leaf_dir) for every bench leaf directory: one holding a
    `<baseline>/` or `new/` result. Leaves sit two or three levels under the
    root depending on `BenchmarkId` shape, so walk rather than hardcode."""
    skip = set(RESULT_DIRS) | {baseline, "report"}

    def walk(path: str, parts: list) -> Iterator[Tuple[str, str]]:
        if os.path.exists(os.path.join(path, baseline, "estimates.json")) or os.path.exists(
            os.path.join(path, "new", "estimates.json")
        ):
            yield ("/".join(parts), path)
            return
        for entry in sorted(os.listdir(path)):
            sub = os.path.join(path, entry)
            if entry not in skip and os.path.isdir(sub):
                yield from walk(sub, parts + [entry])

    if not os.path.isdir(criterion_dir):
        return
    for group in sorted(os.listdir(criterion_dir)):
        gpath = os.path.join(criterion_dir, group)
        if group != "report" and os.path.isdir(gpath):
            yield from walk(gpath, [group])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="run the gate (required)")
    ap.add_argument("--criterion-dir", default="target/criterion-compare")
    ap.add_argument("--baseline", default="base", help="named baseline directory to compare against")
    ap.add_argument("--warn", type=float, default=10.0, help="warn when the point estimate regresses >= this %%")
    ap.add_argument("--fail", type=float, default=25.0, help="fail when the CI lower bound regresses >= this %%")
    args = ap.parse_args()

    if not args.check:
        ap.print_help()
        return 2

    compared = 0
    new_benches, not_run, unreadable, warnings, failures = [], [], [], [], []
    print(f"{'bench':<52} {'base (ns)':>13} {'new (ns)':>13} {'change':>8}  {'95% CI':>17}")
    print("-" * 110)
    for name, leaf in find_benches(args.criterion_dir, args.baseline):
        base_est = os.path.join(leaf, args.baseline, "estimates.json")
        new_est = os.path.join(leaf, "new", "estimates.json")
        change_est = os.path.join(leaf, "change", "estimates.json")
        has_base, has_new = os.path.exists(base_est), os.path.exists(new_est)
        if has_new and not has_base:
            new_benches.append(name)
            print(f"{name:<52} {'-':>13} {'':>13} {'':>8}  new (no baseline)")
            continue
        if has_base and not has_new:
            not_run.append(name)
            print(f"{name:<52} {'':>13} {'-':>13} {'':>8}  not run at head")
            continue
        base_ns, new_ns, change = mean_point(base_est), mean_point(new_est), mean_change(change_est)
        if base_ns is None or new_ns is None or change is None:
            unreadable.append(name)
            print(f"{name:<52} {'?':>13} {'?':>13} {'?':>8}  unreadable estimates")
            continue
        compared += 1
        pct, lo, hi = change
        flag = ""
        if lo >= args.fail:
            flag = "FAIL"
            failures.append((name, pct, lo))
        elif pct >= args.warn:
            flag = "warn"
            warnings.append((name, pct, lo))
        ci = f"[{lo:+.1f}, {hi:+.1f}]%"
        print(f"{name:<52} {base_ns:>13.0f} {new_ns:>13.0f} {pct:>+7.1f}%  {ci:>17} {flag}")

    print()
    if new_benches:
        print(f"note: {len(new_benches)} bench(es) have no '{args.baseline}' result (new at head); not gated.")
    if not_run:
        print(f"note: {len(not_run)} bench(es) have a '{args.baseline}' result but none at head (removed/renamed).")
    if warnings:
        print(f"WARN: {len(warnings)} bench(es) with point estimate >= +{args.warn}%:")
        for name, pct, lo in warnings:
            print(f"  - {name}: {pct:+.1f}% (CI lower bound {lo:+.1f}%)")

    status = 0
    if unreadable:
        print(f"FAIL: {len(unreadable)} bench(es) had base+new results but unreadable estimates/change:")
        for name in unreadable:
            print(f"  - {name}")
        status = 1
    if failures:
        print(f"FAIL: {len(failures)} bench(es) regressed with CI lower bound >= +{args.fail}%:")
        for name, pct, lo in failures:
            print(f"  - {name}: {pct:+.1f}% (CI lower bound {lo:+.1f}%)")
        status = 1
    if compared == 0:
        print(
            f"FAIL: no bench under {args.criterion_dir} has both a '{args.baseline}/' and a fresh "
            "'new/' result, so nothing was compared. The base run or the head run did not "
            "produce results (missing fixtures, wrong --criterion-dir, or wrong --baseline)."
        )
        return 1
    if status == 0:
        print(f"OK: {compared} bench(es) compared; none regressed past +{args.fail}% (CI lower bound).")
    return status


if __name__ == "__main__":
    sys.exit(main())
