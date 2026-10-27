#!/usr/bin/env python3
"""Fail a PR that pastes the same block into two places (#779).

The pattern this catches: a fix lands as one block copied into each twin of
a concept instead of into one owner. 8576c61 (#507) added the same five-line
row-group block, comment included, to ``overview/convert.rs`` and to
``overview/stream.rs``; ab5214d (#407) did the same to the carrier twins,
which have since drifted apart; #743 is the same shape. 15 of 23 fix PRs to
convert.rs also edited stream.rs.

How it works:

* Take the PR's added lines in non-test ``.rs`` / ``.py`` files
  (``git diff --unified=0 base...head``). Test files and directories
  (``tests/``, ``benches/``, ``fuzz/``, ``test_*.py``, ``conftest.py``) are
  skipped, and so are added lines inside top-level ``#[cfg(test)]`` items of
  a Rust file (tests copy setup by design).
* Normalize each line lightly: whitespace collapsed, lowercased, ``self.``
  / ``&`` / ``mut`` / visibility dropped, number literals as ``0``. Lines
  with fewer than 8 word characters (``}``, ``)?;``, ``Ok(())``) and
  ``use`` / ``import`` lines carry no signal and are skipped.
* Fail when a run of 5 significant added lines matches, in at least 4 of its
  5 aligned lines, a run added somewhere else: another file, or a
  non-overlapping place in the same file.

Exemption: a commit in the PR whose message carries the trailer
``Mirrored-ok: #NNN <reason>`` (an issue number and a reason). The findings
are still printed.

A pure rename or a block moved from one place to another adds each line
once and passes.

Usage (CI):
    uv run --no-project python scripts/ci/mirrored_hunks.py --base BASE_SHA --head HEAD_SHA
Offline (tests):
    uv run --no-project python scripts/ci/mirrored_hunks.py --diff FILE [--messages FILE]
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections import defaultdict
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import PurePosixPath

from rust_source import cfg_test_lines

WINDOW = 5
MIN_EQUAL = 4
MIN_WORD_CHARS = 8
TRAILER = re.compile(r"^Mirrored-ok:\s*#\d+\s+\S", re.MULTILINE)
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


@dataclass(frozen=True)
class Line:
    """One significant added line."""

    path: str
    lineno: int
    run: int  # contiguous-added-lines run id, unique per file
    text: str  # as added (for the report)
    norm: str  # normalized (for matching)


def is_test_path(path: str) -> bool:
    """Whether ``path`` is test code by location or name."""
    p = PurePosixPath(path)
    if any(part in ("tests", "benches", "fuzz") for part in p.parts[:-1]):
        return True
    name = p.name
    return name.startswith("test_") or name.endswith("_test.py") or name == "conftest.py"


def in_scope(path: str) -> bool:
    """A non-test Rust or Python file."""
    return path.endswith((".rs", ".py")) and not is_test_path(path)


_NUM = re.compile(r"\b\d[\d_]*(?:\.\d+)?\b")
_VIS = re.compile(r"\bpub(?:\([^)]*\))?\s+")


def normalize(line: str) -> str:
    """Light normalization: whitespace, case, borrows, visibility, numbers."""
    s = line.strip().lower()
    s = _VIS.sub("", s)
    s = s.replace("self.", "").replace("&mut ", "").replace("&", "")
    s = re.sub(r"\bmut\s+", "", s)
    s = _NUM.sub("0", s)
    s = re.sub(r"\s+", " ", s)
    return s.rstrip(",;")


def significant(norm: str) -> bool:
    """Whether a normalized line carries enough signal to match on."""
    if norm.startswith(("use ", "import ", "from ", "mod ")):
        return False
    return len(re.findall(r"\w", norm)) >= MIN_WORD_CHARS


def parse_added(diff: str, read_head: Callable[[str], str | None]) -> list[Line]:
    """Significant added lines of in-scope files, in diff order."""
    out: list[Line] = []
    path: str | None = None
    skip: set[int] = set()
    lineno = 0
    run = 0
    last_added = -10
    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            target = raw[4:].strip()
            path = target[2:] if target.startswith("b/") else None
            if path is not None and not in_scope(path):
                path = None
            skip = set()
            if path is not None and path.endswith(".rs"):
                src = read_head(path)
                skip = cfg_test_lines(src) if src is not None else set()
            continue
        if raw.startswith("--- ") or raw.startswith("diff --git"):
            continue
        m = HUNK.match(raw)
        if m:
            lineno = int(m.group(1))
            continue
        if path is None:
            continue
        if raw.startswith("+"):
            if lineno != last_added + 1:
                run += 1
            last_added = lineno
            if lineno not in skip:
                norm = normalize(raw[1:])
                if significant(norm):
                    out.append(Line(path, lineno, run, raw[1:].rstrip(), norm))
            lineno += 1
        elif raw.startswith(" "):
            lineno += 1
    return out


@dataclass(frozen=True)
class Match:
    """Two places that received the same block."""

    a: tuple[Line, ...]
    b: tuple[Line, ...]


def windows(lines: list[Line]) -> list[tuple[Line, ...]]:
    """Every run of WINDOW consecutive significant lines within one added run."""
    by_run: dict[tuple[str, int], list[Line]] = defaultdict(list)
    for ln in lines:
        by_run[(ln.path, ln.run)].append(ln)
    out = []
    for run_lines in by_run.values():
        for i in range(len(run_lines) - WINDOW + 1):
            out.append(tuple(run_lines[i : i + WINDOW]))
    return out


def same_place(a: tuple[Line, ...], b: tuple[Line, ...]) -> bool:
    """Whether two windows are one edit site: the same contiguous run of
    added lines. Repetition inside one new block (two calls taking the same
    argument list) is a local style question; the mirrored-fix pattern is
    the same block landing at two separate sites."""
    return a[0].path == b[0].path and a[0].run == b[0].run


def find_matches(lines: list[Line]) -> list[Match]:
    """Pairs of windows that agree in at least MIN_EQUAL aligned lines,
    merged so each pair of runs is reported once."""
    wins = windows(lines)
    index: dict[tuple, list[int]] = defaultdict(list)
    for wi, w in enumerate(wins):
        norms = [ln.norm for ln in w]
        # One key per wildcarded position: two windows that share any key
        # agree in at least MIN_EQUAL (= WINDOW - 1) aligned lines.
        for hole in range(WINDOW):
            key = (hole, *(n for i, n in enumerate(norms) if i != hole))
            index[key].append(wi)
    seen: dict[tuple, Match] = {}
    for group in index.values():
        for x in range(len(group)):
            for y in range(x + 1, len(group)):
                a, b = wins[group[x]], wins[group[y]]
                if same_place(a, b):
                    continue
                if (b[0].path, b[0].lineno) < (a[0].path, a[0].lineno):
                    a, b = b, a
                pair = (a[0].path, a[0].run, b[0].path, b[0].run)
                prev = seen.get(pair)
                if prev is None:
                    seen[pair] = Match(a, b)
                else:
                    seen[pair] = Match(_span(prev.a, a), _span(prev.b, b))
    return list(seen.values())


def _span(x: tuple[Line, ...], y: tuple[Line, ...]) -> tuple[Line, ...]:
    """The union of two windows of one run, in line order."""
    return tuple(sorted(set(x) | set(y), key=lambda ln: ln.lineno))


def exempt(messages: str) -> bool:
    """Whether any commit message carries a valid ``Mirrored-ok`` trailer."""
    return TRAILER.search(messages) is not None


def report(matches: list[Match]) -> str:
    """Human-readable findings, with GitHub annotations."""
    out = []
    for m in matches:
        a, b = m.a, m.b
        out.append(
            f"::error file={b[0].path},line={b[0].lineno}::block added at "
            f"{a[0].path}:{a[0].lineno}-{a[-1].lineno} is pasted again at "
            f"{b[0].path}:{b[0].lineno}-{b[-1].lineno}"
        )
        out.extend(f"    {ln.text.strip()}" for ln in a)
    return "\n".join(out)


def run(diff: str, messages: str, read_head: Callable[[str], str | None]) -> tuple[int, str]:
    """Exit status and report for one PR."""
    matches = find_matches(parse_added(diff, read_head))
    if not matches:
        return 0, "mirrored-hunks: no block is added in two places"
    text = report(matches)
    hint = (
        f"\nmirrored-hunks: {len(matches)} block(s) of {WINDOW}+ near-identical added "
        "lines in two places. Put the logic in one owner both places call (#779). "
        "If the copy is deliberate, add the commit trailer "
        "'Mirrored-ok: #NNN <reason>'."
    )
    if exempt(messages):
        return 0, text.replace("::error", "::warning") + "\nexempted by a Mirrored-ok trailer"
    return 1, text + hint


def git(*args: str) -> str:
    """Run git and return stdout."""
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def main(argv: list[str] | None = None) -> int:
    """Entry point."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--base", help="base commit (the PR's target)")
    ap.add_argument("--head", default="HEAD", help="head commit (default HEAD)")
    ap.add_argument("--diff", help="read a unified=0 diff from this file instead of git")
    ap.add_argument("--messages", help="commit messages file (with --diff)")
    args = ap.parse_args(argv)
    if args.diff:
        with open(args.diff) as f:
            diff = f.read()
        messages = open(args.messages).read() if args.messages else ""
        status, text = run(diff, messages, lambda _p: None)
    elif args.base:
        diff = git("diff", "--unified=0", "--no-color", "-M", f"{args.base}...{args.head}", "--", "*.rs", "*.py")
        messages = git("log", "--format=%B", f"{args.base}..{args.head}")

        def read_head(path: str) -> str | None:
            try:
                return git("show", f"{args.head}:{path}")
            except subprocess.CalledProcessError:
                return None

        status, text = run(diff, messages, read_head)
    else:
        ap.error("need --base or --diff")
    print(text)
    return status


if __name__ == "__main__":
    sys.exit(main())
