"""Shared helpers for the structural-gate scripts (#779): which lines of a
rustfmt-formatted Rust file are test code.

The rule matches ci/ratchets.toml's production-code scope, simplified to
rustfmt's layout: a top-level ``#[cfg(test)]`` / ``#[cfg(all(test, ..))]``
attribute starts at column 0, and the item it gates ends either on its own
line (``mod testutil;``, ``use ..;``) or at the next line that starts with
``}`` at column 0. crates/core/tests/architecture.rs implements the exact
version with a lexer; these scripts only need the line set.
"""

from __future__ import annotations


def _is_test_attr(line: str) -> bool:
    s = line.rstrip()
    return s == "#[cfg(test)]" or s.startswith("#[cfg(all(test")


def cfg_test_lines(src: str) -> set[int]:
    """1-based line numbers inside top-level ``#[cfg(test)]`` items."""
    lines = src.splitlines()
    out: set[int] = set()
    i = 0
    while i < len(lines):
        if not _is_test_attr(lines[i]):
            i += 1
            continue
        start = i
        i += 1
        while i < len(lines) and lines[i].startswith("#["):
            i += 1
        if i < len(lines) and lines[i].rstrip().endswith(";"):
            end = i
        else:
            while i < len(lines) and not lines[i].startswith("}"):
                i += 1
            end = i
        out.update(range(start + 1, min(end, len(lines) - 1) + 2))
        i = end + 1
    return out


def strip_test_items(src: str) -> str:
    """``src`` without its top-level ``#[cfg(test)]`` items."""
    drop = cfg_test_lines(src)
    return "".join(line for n, line in enumerate(src.splitlines(keepends=True), start=1) if n not in drop)
