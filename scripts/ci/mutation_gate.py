#!/usr/bin/env python3
"""Mutation Diff helpers (#779): shard plan across packages, and the fix-PR gate.

mutation-diff.yml mutates the lines a PR changes in tylertoo-core and in the
CLI (`tylertoo`, crates/cli: #374, #600 and #709 lived there, unmutated).
Each package has its own cargo-mutants config (.cargo/mutants.toml,
.cargo/mutants-cli.toml), so each shard runs one package.

  plan --list PKG=CONFIG=LISTFILE ... --per-shard N --max-shards M
      Print {"planned": total, "shard_count": n, "matrix": {"include": [...]}}
      for the workflow. Shards go to packages in proportion to their mutant
      count, at least one per package with mutants, M in total at most.

  check MISSED_TXT [--root DIR]
      List the missed mutants that carry no `// mutants: equivalent #NNN`
      annotation (trailing the mutated line, or on a comment line directly
      above it), and
      exit 1 when there is one. The workflow runs it on every PR and makes
      it blocking for PRs titled `fix...`: a fix whose changed lines can be
      mutated without a test noticing has not pinned what it fixed.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from pathlib import Path

MISSED_RE = re.compile(r"^(?P<file>[^:\s][^:]*):(?P<line>\d+):(?P<col>\d+):\s*(?P<desc>.*)$")
EQUIVALENT_RE = re.compile(r"//\s*mutants:\s*equivalent\s+#\d+")


def plan(packages: list[tuple[str, str, int]], per_shard: int, max_shards: int) -> dict:
    """packages: [(package, config, planned mutants)]."""
    live = [(p, c, n) for p, c, n in packages if n > 0]
    total = sum(n for _, _, n in live)
    if not live:
        return {"planned": 0, "shard_count": 0, "matrix": {"include": []}}
    want = min(max(math.ceil(total / per_shard), len(live)), max(max_shards, len(live)))
    # Proportional split, at least one each, largest remainder for the rest.
    shares = {p: max(1, math.floor(want * n / total)) for p, _, n in live}
    while sum(shares.values()) > want:
        biggest = max(shares, key=lambda p: shares[p])
        shares[biggest] -= 1
    order = sorted(live, key=lambda x: -(want * x[2] / total - math.floor(want * x[2] / total)))
    i = 0
    while sum(shares.values()) < want:
        shares[order[i % len(order)][0]] += 1
        i += 1
    include = []
    for p, c, _ in live:
        for k in range(shares[p]):
            include.append({"package": p, "config": c, "shard": k, "shard_count": shares[p], "id": f"{p}-{k}"})
    return {"planned": total, "shard_count": len(include), "matrix": {"include": include}}


def unannotated(missed_lines: list[str], root: Path) -> list[str]:
    out = []
    cache: dict[str, list[str]] = {}
    for raw in missed_lines:
        line = raw.strip()
        if not line:
            continue
        m = MISSED_RE.match(line)
        if not m:
            out.append(line)  # unparseable: report it rather than wave it through
            continue
        path = m.group("file")
        if path not in cache:
            try:
                cache[path] = (root / path).read_text().splitlines()
            except OSError:
                cache[path] = []
        src = cache[path]
        n = int(m.group("line"))
        here = src[n - 1] if 0 < n <= len(src) else ""
        above = src[n - 2] if 1 < n <= len(src) + 1 else ""
        # A trailing annotation covers its own line; a comment-only line
        # covers the line below it.
        covered = EQUIVALENT_RE.search(here) or (above.lstrip().startswith("//") and EQUIVALENT_RE.search(above))
        if not covered:
            out.append(line)
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--list", action="append", required=True, metavar="PKG=CONFIG=LISTFILE")
    p.add_argument("--per-shard", type=int, required=True)
    p.add_argument("--max-shards", type=int, required=True)
    c = sub.add_parser("check")
    c.add_argument("missed", type=Path)
    c.add_argument("--root", type=Path, default=Path.cwd())
    args = ap.parse_args(argv)

    if args.cmd == "plan":
        packages = []
        for spec in args.list:
            pkg, config, listfile = spec.split("=", 2)
            lines = [ln for ln in Path(listfile).read_text().splitlines() if ln.strip()]
            packages.append((pkg, config, len(lines)))
        print(json.dumps(plan(packages, args.per_shard, args.max_shards)))
        return 0

    missed = args.missed.read_text().splitlines() if args.missed.exists() else []
    bad = unannotated(missed, args.root)
    for line in bad:
        print(line)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
