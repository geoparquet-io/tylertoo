#!/usr/bin/env python3
"""Baseline and band ratchet (#779): a PR may not loosen a gate silently.

The output gates compare tylertoo's output with a committed record, and
behaviour PRs used to re-cut that record in the same PR (09673f5 widened the
Madagascar z3-z6 feature band from [0.29, 0.55] to [0.86, 1.50];
`ci_baseline.json` was regenerated three times in three days). A re-cut
record turns the gate into a change detector that the change itself
silences. This check makes every such re-cut carry its evidence.

It fails when the PR (merge-base(BASE, HEAD)..HEAD):

  * loosens any band in benchmarks/e2e/tippecanoe_tolerances.toml: a band
    whose lower edge drops or whose upper edge rises (widened, or moved so
    it now admits a ratio the old band rejected), a band that is removed
    (the ratio becomes ungated), a raised `gate.min_reference` (fewer zooms
    judged) or a changed `gate.mode` (a different run is gated). Narrowing
    a band is always allowed;
  * changes benchmarks/overview/ci_baseline.json (the e2e structural guard);
  * changes tests/fixtures/guard/*.golden.txt (the golden tile digests);
  * changes the golden archives under tests/fixtures/golden/ (README.md
    excepted);

unless a commit in the range carries a trailer

    Baseline-change: #NNN <measured evidence>

where #NNN is the issue the change belongs to and the evidence is the
measurement that justifies it (it must contain at least one digit: a
ratio, a count, a size). One valid trailer covers the whole PR.

Usage:
  baseline_ratchet.py --base origin/main --head HEAD
  baseline_ratchet.py --base HEAD~1 --head HEAD     # one commit (the canary)

Exit status: 0 nothing loosened (or a valid trailer covers it), 1 a
finding with no valid trailer, 2 usage or git error.
"""

from __future__ import annotations

import argparse
import fnmatch
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from typing import Iterable

BANDS_FILE = "benchmarks/e2e/tippecanoe_tolerances.toml"
# Files any change to which is a re-cut baseline.
BASELINE_GLOBS = [
    "benchmarks/overview/ci_baseline.json",
    "tests/fixtures/guard/*.golden.txt",
    "tests/fixtures/golden/*",
]
BASELINE_EXEMPT = ["tests/fixtures/golden/README.md"]

TRAILER_KEY = "Baseline-change"
TRAILER_RE = re.compile(r"^#(\d+)\s+(\S.*)$")

Band = tuple[float, float]


@dataclass(frozen=True)
class Finding:
    path: str
    what: str

    def __str__(self) -> str:
        return f"{self.path}: {self.what}"


# --- band comparison (pure) -------------------------------------------------


def effective_bands(doc: dict) -> dict[tuple[str, str, str], Band]:
    """{(dataset, scope, metric): (lo, hi)} as compare_tippecanoe.py reads it.

    scope is "totals" or "z<N>". A zoom takes the first `zoom_bands` entry
    whose range covers it and that names the metric (compare_tippecanoe.py's
    `zoom_band`), so a re-split of the ranges compares zoom by zoom.
    """
    out: dict[tuple[str, str, str], Band] = {}
    for ds, spec in doc.get("datasets", {}).items():
        for metric, band in spec.get("totals", {}).items():
            out[(ds, "totals", metric)] = (float(band[0]), float(band[1]))
        for entry in spec.get("zoom_bands", []):
            z0, z1 = entry["zooms"]
            for metric, band in entry.items():
                if metric == "zooms":
                    continue
                for z in range(int(z0), int(z1) + 1):
                    out.setdefault((ds, f"z{z}", metric), (float(band[0]), float(band[1])))
    return out


def _fmt(band: Band) -> str:
    return f"[{band[0]:g}, {band[1]:g}]"


def band_loosenings(old: dict, new: dict) -> list[str]:
    """Every way `new` lets through a ratio that `old` would have failed."""
    found: list[str] = []
    old_gate, new_gate = old.get("gate", {}), new.get("gate", {})
    if "min_reference" in old_gate:
        o, n = old_gate["min_reference"], new_gate.get("min_reference")
        if n is None or n > o:
            found.append(f"gate.min_reference raised {o} -> {n} (fewer zooms are judged)")
    if old_gate.get("mode") != new_gate.get("mode"):
        found.append(
            f"gate.mode changed {old_gate.get('mode')!r} -> {new_gate.get('mode')!r} (a different run is gated)"
        )

    old_bands, new_bands = effective_bands(old), effective_bands(new)
    for key in sorted(old_bands):
        ds, scope, metric = key
        o = old_bands[key]
        n = new_bands.get(key)
        label = f"{ds} {scope} {metric}"
        if n is None:
            found.append(f"{label}: band {_fmt(o)} removed (the ratio is no longer gated)")
            continue
        edges = []
        if n[0] < o[0]:
            edges.append(f"lower edge {o[0]:g} -> {n[0]:g}")
        if n[1] > o[1]:
            edges.append(f"upper edge {o[1]:g} -> {n[1]:g}")
        if edges:
            kind = "widened" if (n[1] - n[0]) > (o[1] - o[0]) else "moved"
            found.append(f"{label}: {_fmt(o)} -> {_fmt(n)} {kind} ({', '.join(edges)})")
    return found


# --- trailers (pure) --------------------------------------------------------


def valid_trailers(values: Iterable[str]) -> tuple[list[str], list[str]]:
    """Split Baseline-change trailer values into (valid, rejected-with-reason)."""
    good, bad = [], []
    for raw in values:
        value = raw.strip()
        if not value:
            continue
        m = TRAILER_RE.match(value)
        if not m:
            bad.append(f"{value!r}: expected '#NNN <measured evidence>'")
        elif not re.search(r"\d", m.group(2)):
            bad.append(f"{value!r}: the evidence names no measurement (no digit)")
        else:
            good.append(value)
    return good, bad


def is_baseline_path(path: str) -> bool:
    if path in BASELINE_EXEMPT:
        return False
    return any(fnmatch.fnmatchcase(path, g) for g in BASELINE_GLOBS)


# --- git --------------------------------------------------------------------


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def show(rev: str, path: str) -> str | None:
    proc = subprocess.run(["git", "show", f"{rev}:{path}"], capture_output=True, text=True)
    return proc.stdout if proc.returncode == 0 else None


def findings_for(base: str, head: str) -> list[Finding]:
    merge_base = git("merge-base", base, head).strip()
    changed = git("diff", "--name-only", "--no-renames", merge_base, head).split()
    out: list[Finding] = []
    for path in changed:
        if path == BANDS_FILE:
            old_text, new_text = show(merge_base, path), show(head, path)
            old = tomllib.loads(old_text) if old_text is not None else {}
            new = tomllib.loads(new_text) if new_text is not None else {}
            out += [Finding(path, f) for f in band_loosenings(old, new)]
        elif is_baseline_path(path):
            state = "deleted" if show(head, path) is None else "changed"
            out.append(Finding(path, f"baseline {state}"))
    return out


def trailer_values(base: str, head: str) -> list[str]:
    merge_base = git("merge-base", base, head).strip()
    log = git(
        "log",
        f"--format=%(trailers:key={TRAILER_KEY},valueonly,unfold,separator=%x00)%x00",
        f"{merge_base}..{head}",
    )
    return [v for v in log.split("\0") if v.strip()]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", required=True, help="the branch the PR merges into (e.g. origin/main)")
    ap.add_argument("--head", default="HEAD", help="the PR's head (default HEAD)")
    args = ap.parse_args(argv)

    try:
        findings = findings_for(args.base, args.head)
        values = trailer_values(args.base, args.head) if findings else []
    except subprocess.CalledProcessError as e:
        print(f"error: git {' '.join(e.cmd[1:])} failed: {e.stderr.strip()}", file=sys.stderr)
        return 2

    if not findings:
        print("baseline ratchet: no band loosened, no baseline changed.")
        return 0

    print("baseline ratchet: this change loosens a gate or re-cuts a baseline:")
    for f in findings:
        print(f"  - {f}")
    good, bad = valid_trailers(values)
    for b in bad:
        print(f"  rejected trailer {b}")
    if good:
        print(f"OK: covered by {TRAILER_KEY}: " + "; ".join(good))
        return 0
    print(
        f"\nFAIL: no commit in the range carries a valid trailer. Add one to a commit message:\n\n"
        f"    {TRAILER_KEY}: #NNN <measured evidence, e.g. 'z3 features 0.910 on CI run 123'>\n\n"
        "A re-cut baseline is the gate agreeing with the change that moved it; the trailer\n"
        "records which issue owns the move and what was measured. See ci/README.md."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
