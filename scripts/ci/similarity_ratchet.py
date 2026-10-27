#!/usr/bin/env python3
"""Duplicate-code ratchet over similarity-rs (#779).

similarity-rs compares functions *within* one file only: it reports no pair
across two files at any threshold (checked with two identical functions in
two files, similarity-rs 0.5.0). So this runs it twice:

1. In-file pairs over ``crates/{core,cli,python}/src``, as CI did before at
   0.95, but at ``[similarity_pairs] threshold`` in ``ci/ratchets.toml``.
2. Cross-engine pairs: ``overview/convert.rs`` and ``overview/stream.rs``
   concatenated into one temporary file, so the two engines' twins (the
   #507/8576c61 and #407/ab5214d mirrored edits) are compared at all. Only
   pairs with one side in each engine count, at
   ``[similarity_convert_stream] threshold``.

Both runs see production code only: top-level ``#[cfg(test)]`` items are
stripped from a staged copy of the tree first (see ``rust_source.py``).

Each finding is keyed by file and function names, not line numbers, so
unrelated edits do not move it. The keys are pinned in
``ci/similarity_baseline.txt``. A key not in the baseline fails (new
duplication: share the code instead). A baseline key no longer reported
fails too (remove its line and lower the pin), so the list only shrinks.
``--update`` rewrites the baseline from the current findings; use it only
to remove pairs, never to admit new ones.

Usage:
    uv run --no-project python scripts/ci/similarity_ratchet.py
    uv run --no-project python scripts/ci/similarity_ratchet.py --update
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
import tomllib
from collections import Counter
from pathlib import Path

from rust_source import strip_test_items

ROOT = Path(__file__).resolve().parents[2]
RATCHETS = ROOT / "ci" / "ratchets.toml"
BASELINE = ROOT / "ci" / "similarity_baseline.txt"
SRC_DIRS = ["crates/core/src", "crates/cli/src", "crates/python/src"]
ENGINES = ["crates/core/src/overview/convert.rs", "crates/core/src/overview/stream.rs"]
MIN_LINES = 10

PAIR = re.compile(
    r"^\s*(?P<fa>\S+?):(?P<la>\d+)-\d+ (?:function|method) (?P<na>\S+)"
    r" <-> (?P<fb>\S+?):(?P<lb>\d+)-\d+ (?:function|method) (?P<nb>\S+)"
)
SIM = re.compile(r"Similarity: (?P<pct>[\d.]+)%")


def parse_pairs(text: str) -> list[tuple[float, str, int, str, str, int, str]]:
    """Parse similarity-rs output into (similarity, fileA, lineA, nameA,
    fileB, lineB, nameB) tuples."""
    lines = text.splitlines()
    out = []
    for i, line in enumerate(lines):
        m = PAIR.match(line)
        if not m:
            continue
        for follow in lines[i + 1 : i + 3]:
            s = SIM.search(follow)
            if s:
                out.append(
                    (
                        float(s["pct"]) / 100,
                        m["fa"],
                        int(m["la"]),
                        m["na"],
                        m["fb"],
                        int(m["lb"]),
                        m["nb"],
                    )
                )
                break
    return out


def in_file_keys(text: str) -> list[str]:
    """Baseline keys for in-file pairs: ``file :: a <-> b`` (names sorted).
    Paths are as similarity-rs printed them: repo-relative, since it runs
    on a staged copy of the tree with relative arguments."""
    keys = []
    for _, fa, _, na, fb, _, nb in parse_pairs(text):
        a, b = sorted([na, nb])
        keys.append(f"{fa} :: {a} <-> {b}" if fa == fb else f"{fa}::{na} <-> {fb}::{nb}")
    return sorted(keys)


def cross_engine_keys(text: str, split_line: int) -> list[str]:
    """Keys for pairs in the concatenated engines file whose two sides fall
    in different engines (line <= split_line is convert.rs)."""
    keys = []
    for _, _, la, na, _, lb, nb in parse_pairs(text):
        side_a = "convert.rs" if la <= split_line else "stream.rs"
        side_b = "convert.rs" if lb <= split_line else "stream.rs"
        if side_a == side_b:
            continue
        conv, strm = (na, nb) if side_a == "convert.rs" else (nb, na)
        keys.append(f"convert.rs::{conv} <-> stream.rs::{strm}")
    return sorted(keys)


def stage_tree(tmp: Path, files: list[Path]) -> None:
    """Copy ``files`` under ``tmp`` at their repo-relative paths, minus test
    items. similarity-rs's ``--skip-test`` skips only ``#[test]`` functions,
    not the fixture builders around them, and test modules are half of each
    engine file: stripping them is both the scope (production code) and the
    speed (the engines run is quadratic in function count)."""
    for f in files:
        dest = tmp / f.relative_to(ROOT)
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(strip_test_items(f.read_text()))


def run_similarity(threshold: float, paths: list[str], cwd: Path) -> str:
    """Run similarity-rs and return its stdout."""
    cmd = [
        "similarity-rs",
        "--threshold",
        str(threshold),
        "--min-lines",
        str(MIN_LINES),
        "--skip-test",
        *paths,
    ]
    return subprocess.run(cmd, check=True, capture_output=True, text=True, cwd=cwd).stdout


def current_keys(thresholds: tuple[float, float]) -> tuple[list[str], list[str]]:
    """In-file and cross-engine keys for the tree as it is now."""
    with tempfile.TemporaryDirectory() as tmp_s:
        tmp = Path(tmp_s)
        files = sorted(f for d in SRC_DIRS for f in (ROOT / d).rglob("*.rs"))
        stage_tree(tmp, files)
        in_file = in_file_keys(run_similarity(thresholds[0], SRC_DIRS, tmp))
        convert = strip_test_items((ROOT / ENGINES[0]).read_text())
        split_line = convert.count("\n")
        joined = tmp / "engines.rs"
        joined.write_text(convert + strip_test_items((ROOT / ENGINES[1]).read_text()))
        cross = cross_engine_keys(run_similarity(thresholds[1], ["engines.rs"], tmp), split_line)
    return in_file, cross


def read_baseline(path: Path) -> tuple[list[str], list[str]]:
    """The pinned in-file and cross-engine keys."""
    in_file, cross = [], []
    section = None
    for line in path.read_text().splitlines():
        t = line.strip()
        if t == "[in-file]":
            section = in_file
        elif t == "[convert-stream]":
            section = cross
        elif t and not t.startswith("#") and section is not None:
            section.append(t)
    return sorted(in_file), sorted(cross)


def write_baseline(path: Path, in_file: list[str], cross: list[str], thresholds: tuple[float, float]) -> None:
    """Rewrite the baseline file."""
    head = [
        "# similarity-rs findings pinned by scripts/ci/similarity_ratchet.py (#779).",
        f"# Thresholds: in-file {thresholds[0]}, convert-stream {thresholds[1]};",
        f"# --min-lines {MIN_LINES}, --skip-test, test items stripped. Keys are",
        "# file + function names (sorted), not line numbers. This list only",
        "# shrinks: refactor a pair away, then delete its line and lower the pin",
        "# in ci/ratchets.toml. Never add a line to admit new duplication.",
        "",
        "[in-file]",
        *in_file,
        "",
        "# convert.rs and stream.rs concatenated, pairs that span both engines",
        "# (the twin-function pattern behind #507/8576c61 and #407/ab5214d).",
        "[convert-stream]",
        *cross,
        "",
    ]
    path.write_text("\n".join(head))


def diff(name: str, current: list[str], pinned: list[str]) -> list[str]:
    """Failure messages for one key set."""
    cur, pin = Counter(current), Counter(pinned)
    new = sorted((cur - pin).elements())
    gone = sorted((pin - cur).elements())
    msgs = []
    if new:
        msgs.append(
            f"{name}: {len(new)} new near-duplicate pair(s). Share the code "
            "(one owner) instead of copying it:\n  " + "\n  ".join(new)
        )
    if gone:
        msgs.append(
            f"{name}: {len(gone)} pinned pair(s) no longer reported. Delete "
            f"these lines from {BASELINE.relative_to(ROOT)} and lower the pin "
            "in ci/ratchets.toml:\n  " + "\n  ".join(gone)
        )
    return msgs


def check(
    current: tuple[list[str], list[str]],
    pinned: tuple[list[str], list[str]],
    pins: dict[str, int],
) -> list[str]:
    """All failure messages; empty means the ratchet holds."""
    msgs = diff("in-file", current[0], pinned[0])
    msgs += diff("convert<->stream", current[1], pinned[1])
    for key, keys in (("similarity_pairs", pinned[0]), ("similarity_convert_stream", pinned[1])):
        if len(keys) != pins[key]:
            msgs.append(
                f"[{key}] pin is {pins[key]} but {BASELINE.relative_to(ROOT)} "
                f"lists {len(keys)} pair(s); set the pin to {len(keys)}."
            )
    return msgs


def load_config() -> tuple[tuple[float, float], dict[str, int]]:
    """Threshold and pins from ci/ratchets.toml."""
    cfg = tomllib.loads(RATCHETS.read_text())
    thresholds = (
        float(cfg["similarity_pairs"]["threshold"]),
        float(cfg["similarity_convert_stream"]["threshold"]),
    )
    pins = {k: int(cfg[k]["pin"]) for k in ("similarity_pairs", "similarity_convert_stream")}
    return thresholds, pins


def main(argv: list[str] | None = None) -> int:
    """Entry point."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--update", action="store_true", help="rewrite the baseline from current findings")
    args = ap.parse_args(argv)
    thresholds, pins = load_config()
    current = current_keys(thresholds)
    print(
        f"similarity-rs: {len(current[0])} in-file pair(s) >= {thresholds[0]}, "
        f"{len(current[1])} convert<->stream pair(s) >= {thresholds[1]}"
    )
    if args.update:
        write_baseline(BASELINE, current[0], current[1], thresholds)
        print(f"wrote {BASELINE.relative_to(ROOT)}; set the pins to these counts")
        return 0
    msgs = check(current, read_baseline(BASELINE), pins)
    for m in msgs:
        print(f"::error::{m.splitlines()[0]}")
        print(m)
    return 1 if msgs else 0


if __name__ == "__main__":
    sys.exit(main())
