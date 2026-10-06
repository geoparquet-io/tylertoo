"""Markdown helpers for the documentation tests.

Parses fenced code blocks out of the site's Markdown and compares the
expected-output blocks a tutorial shows against what a command printed.
"""

from __future__ import annotations

import re
import shutil
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]
EXAMPLES_DIR = REPO_ROOT / "examples"
MADAGASCAR = "fieldmaps-madagascar-adm4.parquet"

# Pages written from source by the reference generators; they hold usage
# strings, not runnable examples.
GENERATED_PAGES = frozenset(
    {
        REPO_ROOT / "docs" / "reference" / "cli.md",
        REPO_ROOT / "docs" / "reference" / "python.md",
    }
)

NOTEST = "# notest"

_FENCE = re.compile(
    r"^(?P<indent>[ \t]*)(?P<fence>`{3,})(?P<info>[^`\n]*)\n"
    r"(?P<body>.*?)^(?P=indent)(?P=fence)[ \t]*$",
    re.MULTILINE | re.DOTALL,
)
_ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Wall-clock durations ("in 0.46s", "12ms") vary run to run; everything
# else tylertoo prints for a fixed input is deterministic.
_DURATION = re.compile(r"\b\d+(?:\.\d+)?(?:ms|s)\b")


@dataclass(frozen=True)
class Block:
    """One fenced code block."""

    lang: str
    body: str


def fenced_blocks(markdown: str) -> list[Block]:
    """Return every fenced code block in document order."""
    blocks = []
    for m in _FENCE.finditer(markdown):
        indent = m.group("indent")
        lines = m.group("body").splitlines()
        body = "\n".join(line.removeprefix(indent) for line in lines)
        lang = m.group("info").strip().split(" ")[0]
        blocks.append(Block(lang=lang, body=body))
    return blocks


def site_pages() -> list[Path]:
    """Hand-written Markdown that the docs site renders or includes."""
    pages = [REPO_ROOT / "README.md"]
    pages += sorted((REPO_ROOT / "docs").rglob("*.md"))
    pages += sorted(EXAMPLES_DIR.glob("*/README.md"))
    return [p for p in pages if p.resolve() not in GENERATED_PAGES]


def example_steps(example: Path) -> list[Path]:
    """The numbered step scripts of one example, in run order."""
    return sorted(p for p in example.glob("0*") if p.suffix in {".sh", ".py"})


def fence_for(source: Path) -> str:
    """The exact fence a tutorial must contain to show `source` verbatim."""
    lang = {".sh": "bash", ".py": "python"}[source.suffix]
    text = source.read_text(encoding="utf-8").rstrip("\n")
    return f"```{lang}\n{text}\n```"


def expected_output(tutorial: str, source: Path) -> str | None:
    """The `text` block shown right after `source`'s fence, if any."""
    blocks = fenced_blocks(tutorial)
    text = source.read_text(encoding="utf-8").rstrip("\n")
    for i, block in enumerate(blocks):
        if block.body == text and i + 1 < len(blocks):
            nxt = blocks[i + 1]
            return nxt.body if nxt.lang == "text" else None
    return None


def normalize(text: str) -> str:
    """Strip colour codes and durations, and trailing spaces per line."""
    text = _DURATION.sub("<t>", _ANSI.sub("", text))
    return "\n".join(line.rstrip() for line in text.splitlines())


def missing_lines(expected: str, actual: str) -> list[str]:
    """Lines of `expected` that `actual` does not contain.

    A tutorial may show a subset of the output; `...` marks the elisions.
    """
    haystack = normalize(actual)
    missing = []
    for line in normalize(expected).splitlines():
        needle = line.strip()
        if needle and needle != "..." and needle not in haystack:
            missing.append(line)
    return missing


def stage_input(madagascar: Path, workdir: Path) -> None:
    """Put the input where a reader's `curl -LO` would have left it."""
    shutil.copyfile(madagascar, workdir / MADAGASCAR)
