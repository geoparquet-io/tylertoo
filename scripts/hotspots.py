# /// script
# requires-python = ">=3.10"
# dependencies = ["lizard==1.24.1"]
# ///
"""Rank functions by cyclomatic complexity times file churn (#614).

A complex function that nobody touches is stable; a busy file of simple
functions is cheap to change. The functions worth refactoring first are the
ones that are both, so this prints the top N by

    score = CCN(function) x commits(file it lives in, over --since)

CCN comes from lizard, per function. Churn is the number of commits that
touched the file in `git log --since=<since>` (renames are not followed, so
a file renamed inside the window only counts commits since the rename).
Churn is per file: every function in a busy file shares that file's count.

Usage (from anywhere in the repo):

    uv run --script scripts/hotspots.py                # top 25, 18 months
    uv run --script scripts/hotspots.py --top 50 --since 6.months
    uv run --script scripts/hotspots.py --min-ccn 10 crates/core/src

A local triage tool, not a CI gate. See DEVELOPMENT.md, "Hotspots".
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

import lizard

DEFAULT_PATHS = ["crates/core/src", "crates/cli/src", "crates/python/src"]


@dataclass
class Hotspot:
    score: int
    ccn: int
    commits: int
    nloc: int
    path: str
    line: int
    name: str


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        check=True,
        capture_output=True,
        text=True,
    )
    return Path(out.stdout.strip())


def churn(root: Path, since: str, paths: list[str]) -> Counter[str]:
    """Commits per file (repo-relative path) since `since`."""
    out = subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "log",
            "--no-renames",
            "--numstat",
            f"--since={since}",
            "--format=%H",
            "--",
            *paths,
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    # Numstat prints one `added<TAB>deleted<TAB>path` line per file per
    # commit, so counting those lines counts commits; the hash lines are
    # the only other non-blank output.
    counts: Counter[str] = Counter()
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) == 3:
            counts[parts[2]] += 1
    return counts


def rust_files(root: Path, paths: list[str]) -> list[Path]:
    files: list[Path] = []
    for p in paths:
        base = root / p
        if base.is_file() and base.suffix == ".rs":
            files.append(base)
        elif base.is_dir():
            files.extend(sorted(base.rglob("*.rs")))
        else:
            print(f"hotspots: skipping {p}: not a .rs file or directory", file=sys.stderr)
    return files


def hotspots(root: Path, paths: list[str], since: str, min_ccn: int) -> list[Hotspot]:
    commits = churn(root, since, paths)
    found: list[Hotspot] = []
    for file in rust_files(root, paths):
        rel = file.relative_to(root).as_posix()
        n = commits.get(rel, 0)
        if n == 0:
            continue
        for fn in lizard.analyze_file(str(file)).function_list:
            ccn = fn.cyclomatic_complexity
            if ccn < min_ccn:
                continue
            found.append(Hotspot(ccn * n, ccn, n, fn.nloc, rel, fn.start_line, fn.name))
    found.sort(key=lambda h: (-h.score, -h.ccn, h.path, h.line))
    return found


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument(
        "paths",
        nargs="*",
        default=DEFAULT_PATHS,
        help="files or directories, repo-relative (default: %(default)s)",
    )
    ap.add_argument("--top", type=int, default=25, help="rows to print (default: %(default)s)")
    ap.add_argument(
        "--since",
        default="18.months",
        help="churn window, any `git log --since` value (default: %(default)s)",
    )
    ap.add_argument(
        "--min-ccn",
        type=int,
        default=2,
        help="ignore functions below this CCN (default: %(default)s)",
    )
    args = ap.parse_args()

    root = repo_root()
    rows = hotspots(root, args.paths, args.since, args.min_ccn)[: args.top]
    if not rows:
        print("hotspots: no functions found (check the paths and --since)", file=sys.stderr)
        return 1

    print(f"Top {len(rows)} by CCN x commits (since {args.since})\n")
    header = f"{'score':>6} {'ccn':>4} {'commits':>7} {'nloc':>5}  location  function"
    print(header)
    print("-" * len(header))
    for h in rows:
        print(f"{h.score:>6} {h.ccn:>4} {h.commits:>7} {h.nloc:>5}  {h.path}:{h.line}  {h.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
