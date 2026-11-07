#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""The one bench list (#810): crates/core/benches/benches.toml + ci/exemptions.toml.

    bench_list.py weekly              benches the weekly wall-clock gate runs
    bench_list.py codspeed --all      every bench CodSpeed runs
    git diff --name-only B H | bench_list.py codspeed
                                      the CodSpeed benches those paths can move
    bench_list.py noisy               --noisy globs for criterion_gate.py, one per line
    bench_list.py docs                rewrite the DEVELOPMENT.md bench table
    bench_list.py check               exit 1 unless the list, crates/core/Cargo.toml,
                                      the exemptions and the table agree

Bench lists print space-separated, in benches.toml order.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BENCHES = "crates/core/benches/benches.toml"
EXEMPTIONS = "ci/exemptions.toml"
CARGO = "crates/core/Cargo.toml"
DOC = "DEVELOPMENT.md"
REGION = re.compile(r"(<!-- generated:bench-list -->\n).*?(<!-- /generated -->)", re.S)


def load(root: Path = ROOT) -> tuple[dict, dict]:
    return tomllib.loads((root / BENCHES).read_text()), tomllib.loads(
        (root / EXEMPTIONS).read_text()
    )


def runs(benches: dict, ex: dict, system: str) -> list[str]:
    """The benches `system` ("weekly" or "codspeed") runs: all but its exemptions."""
    return [b for b in benches["bench"] if b not in ex.get(system, {})]


def select(benches: dict, ex: dict, changed: list[str]) -> list[str]:
    """The CodSpeed benches a change to `changed` can move."""

    def hit(patterns: list[str]) -> bool:
        return any(re.fullmatch(p, c) for p in patterns for c in changed)

    every = runs(benches, ex, "codspeed")
    if hit(benches["global_paths"]):
        return every
    return [
        b
        for b in every
        if hit(
            [
                *benches["bench"][b].get("paths", ["crates/core/.*"]),
                rf"crates/core/benches/{b}\.rs",
            ]
        )
    ]


def table(benches: dict, ex: dict) -> str:
    rows = [
        "| Bench | Kind | Weekly gate | CodSpeed | Covers |",
        "|---|---|---|---|---|",
    ]
    for name, b in benches["bench"].items():
        on = [
            "yes" if name not in ex.get(s, {}) else "no" for s in ("weekly", "codspeed")
        ]
        rows.append(f"| `{name}` | `{b['kind']}` | {on[0]} | {on[1]} | {b['covers']} |")
    return "\n".join(rows) + "\n"


def render_doc(root: Path, benches: dict, ex: dict) -> tuple[str, str]:
    text = (root / DOC).read_text()
    return text, REGION.sub(lambda m: m[1] + table(benches, ex) + m[2], text)


def problems(root: Path = ROOT) -> list[str]:
    benches, ex = load(root)
    listed = set(benches["bench"])
    targets = {
        b["name"] for b in tomllib.loads((root / CARGO).read_text()).get("bench", [])
    }
    errs = [
        f"[[bench]] {n} in {CARGO} is missing from {BENCHES}"
        for n in sorted(targets - listed)
    ]
    errs += [
        f"{n} in {BENCHES} has no [[bench]] in {CARGO}"
        for n in sorted(listed - targets)
    ]
    for system in ("weekly", "codspeed"):
        errs += [
            f"[{system}.{n}] in {EXEMPTIONS} names no bench in {BENCHES}"
            for n in ex.get(system, {})
            if n not in listed
        ]
    for name, b in benches["bench"].items():
        if b.get("kind") not in ("cpu", "syscall"):
            errs.append(f"{name}: kind must be cpu or syscall")
        elif b["kind"] == "syscall" and name not in ex.get("codspeed", {}):
            errs.append(
                f"{name} is syscall-dominated but runs on CodSpeed: add [codspeed.{name}] to {EXEMPTIONS}"
            )
        if not b.get("covers"):
            errs.append(f"{name}: no `covers` text for {DOC}")
    text, want = render_doc(root, benches, ex)
    if not REGION.search(text):
        errs.append(f"{DOC} has no <!-- generated:bench-list --> region")
    elif text != want:
        errs.append(f"{DOC} bench table is stale: run scripts/bench_list.py docs")
    return errs


def main(argv: list[str]) -> int:
    cmd = argv[0] if argv else ""
    benches, ex = load()
    if cmd == "weekly":
        print(" ".join(runs(benches, ex, "weekly")))
    elif cmd == "codspeed" and argv[1:] == ["--all"]:
        print(" ".join(runs(benches, ex, "codspeed")))
    elif cmd == "codspeed":
        print(" ".join(select(benches, ex, sys.stdin.read().split())))
    elif cmd == "noisy":
        print("\n".join(ex.get("noisy", {})))
    elif cmd == "docs":
        _, want = render_doc(ROOT, benches, ex)
        (ROOT / DOC).write_text(want)
    elif cmd == "check":
        errs = problems()
        for e in errs:
            print(f"error: {e}", file=sys.stderr)
        return 1 if errs else 0
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
