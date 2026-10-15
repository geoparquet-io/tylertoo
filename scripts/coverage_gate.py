#!/usr/bin/env python3
"""Line-coverage floor for the Rust workspace (#678).

Reads the lcov file the CI Coverage job writes (`cargo llvm-cov ... --lcov`)
and fails when total line coverage is below --min. The total is read here,
in CI, so the gate does not depend on Codecov: Codecov's `codecov/project`
status never posted on this repo, and its upload step does not fail the job.
Codecov still gets the report, for the per-file view and the PR comment.

Counting, per source file (lcov `SF:` records):
  - a line is one `DA:<line>,<hits>` record; it is covered when hits > 0.
    A file listed twice (one section per test binary) is merged, and a line
    counts as covered if any section hit it.
  - files matching an --ignore glob are left out. The defaults are the
    `ignore:` list in codecov.yml, so both report the same file set; keep
    them in step.
Codecov does the same line count on the same file set; the two totals
should agree to within rounding.

Usage:
  coverage_gate.py --check lcov.info --min 94
  coverage_gate.py --check lcov.info --min 94 --show 15   # least-covered files
"""
import argparse
import fnmatch
import os
import sys
from typing import Dict, List

# Keep in step with `ignore:` in codecov.yml.
DEFAULT_IGNORE = ["crates/cli/**", "crates/python/**", "tests/**", "benches/**"]


def rel_path(path: str, root: str) -> str:
    """Repo-relative path with forward slashes; lcov records absolute paths."""
    if os.path.isabs(path):
        path = os.path.relpath(path, root)
    return path.replace(os.sep, "/")


def ignored(path: str, globs: List[str]) -> bool:
    # fnmatch's `*` already crosses `/`, so `dir/**` matches everything under dir.
    return any(fnmatch.fnmatchcase(path, g) for g in globs)


def read_lcov(lcov: str, root: str) -> Dict[str, Dict[int, bool]]:
    """{repo-relative file: {line: covered}} from an lcov tracefile."""
    files: Dict[str, Dict[int, bool]] = {}
    current = None
    with open(lcov) as f:
        for raw in f:
            line = raw.strip()
            if line.startswith("SF:"):
                current = files.setdefault(rel_path(line[3:], root), {})
            elif line.startswith("DA:") and current is not None:
                fields = line[3:].split(",")
                lineno, hits = int(fields[0]), int(fields[1])
                current[lineno] = current.get(lineno, False) or hits > 0
            elif line == "end_of_record":
                current = None
    return files


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", metavar="LCOV", required=True, help="lcov tracefile to read")
    ap.add_argument("--min", type=float, required=True, help="fail when total line coverage %% is below this")
    ap.add_argument(
        "--ignore",
        action="append",
        metavar="GLOB",
        help="repo-relative glob to leave out (repeatable; replaces the codecov.yml defaults)",
    )
    ap.add_argument("--root", default=os.getcwd(), help="repo root that lcov paths are made relative to")
    ap.add_argument("--show", type=int, default=10, help="list this many least-covered files")
    args = ap.parse_args()

    globs = args.ignore if args.ignore is not None else DEFAULT_IGNORE
    files = read_lcov(args.check, args.root)
    kept = {path: lines for path, lines in files.items() if not ignored(path, globs)}
    total = sum(len(lines) for lines in kept.values())
    hit = sum(sum(lines.values()) for lines in kept.values())

    if total == 0:
        print(
            f"FAIL: {args.check} has no coverable lines outside the ignored paths "
            f"({len(files)} file(s) read, ignore={globs}). The coverage run produced "
            "nothing to gate, or every path was ignored."
        )
        return 1

    def pct(h: int, n: int) -> float:
        return 100.0 * h / n if n else 100.0

    worst = sorted(kept.items(), key=lambda kv: (pct(sum(kv[1].values()), len(kv[1])), kv[0]))
    if args.show > 0:
        print(f"{'least-covered files':<60} {'lines':>7} {'missed':>7} {'cover':>7}")
        print("-" * 84)
        for path, lines in worst[: args.show]:
            h = sum(lines.values())
            print(f"{path:<60} {len(lines):>7} {len(lines) - h:>7} {pct(h, len(lines)):>6.2f}%")
        print()

    coverage = pct(hit, total)
    print(
        f"{len(kept)} file(s), {total} lines, {hit} hit, {total - hit} missed "
        f"({len(files) - len(kept)} file(s) ignored): {coverage:.2f}% line coverage"
    )
    if coverage < args.min:
        print(f"FAIL: line coverage {coverage:.2f}% is below the {args.min}% floor.")
        return 1
    print(f"OK: line coverage {coverage:.2f}% meets the {args.min}% floor.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
