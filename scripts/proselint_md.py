# /// script
# requires-python = ">=3.10"
# dependencies = ["proselint==0.16.0"]
# ///
"""Run proselint on Markdown prose, skipping code.

proselint reads a Markdown file as plain text, so it flags words inside
fenced code blocks, inline code spans, URLs, and HTML comments. This
wrapper writes a copy of each file with those regions blanked to spaces
(line and column positions are unchanged), runs proselint on the copies,
and reports each finding against the original path.

Usage (from the repo root; scripts/lint-prose.sh calls it this way):

    uv run --script scripts/proselint_md.py FILE...

The check selection lives in proselint.json at the repo root.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

CONFIG = Path(__file__).resolve().parent.parent / "proselint.json"

FENCE = re.compile(r"^\s*(?:>\s*)*(`{3,}|~{3,})")
INLINE_CODE = re.compile(r"(`+)(?!`).+?(?<!`)\1")
COMMENT = re.compile(r"<!--.*?-->", re.S)
LINK_TARGET = re.compile(r"\]\([^)\s]*(?:\s+\"[^\"]*\")?\)")
AUTOLINK = re.compile(r"<https?://[^>]*>|https?://[^\s)>\]]+")
SNIPPET = re.compile(r"^\s*-{2}8<-{2}.*$", re.M)


def _blank(match: re.Match[str]) -> str:
    return re.sub(r"[^\n]", " ", match.group(0))


def mask(text: str) -> str:
    """Return `text` with code, comments, and URLs replaced by spaces."""
    lines = text.split("\n")
    fence: str | None = None
    for i, line in enumerate(lines):
        opened = FENCE.match(line)
        if fence is None:
            if opened:
                fence = opened.group(1)
                lines[i] = " " * len(line)
            continue
        lines[i] = " " * len(line)
        if (
            opened
            and opened.group(1)[0] == fence[0]
            and len(opened.group(1)) >= len(fence)
            and not line[opened.end() :].strip()
        ):
            fence = None
    text = "\n".join(lines)
    for pattern in (COMMENT, SNIPPET, INLINE_CODE, LINK_TARGET, AUTOLINK):
        text = pattern.sub(_blank, text)
    return text


def main(paths: list[str]) -> int:
    if not paths:
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        copies: list[str] = []
        originals: dict[str, str] = {}
        for index, path in enumerate(paths):
            copy = Path(tmp).resolve() / f"{index}.md"
            text = Path(path).read_text(encoding="utf-8")
            copy.write_text(mask(text), encoding="utf-8")
            copies.append(str(copy))
            originals[copy.as_uri()] = path
        completed = subprocess.run(
            [
                sys.executable,
                "-m",
                "proselint",
                "check",
                "--config",
                str(CONFIG),
                "--output-format",
                "json",
                *copies,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    if completed.returncode not in (0, 1):
        sys.stderr.write(completed.stderr)
        return completed.returncode
    results = json.loads(completed.stdout)["result"]
    found = 0
    for uri, path in originals.items():
        for diagnostic in results.get(uri, {}).get("diagnostics", []):
            line, column = diagnostic["pos"]
            print(
                f"{path}:{line}:{column}: "
                f"{diagnostic['check_path']}: {diagnostic['message']}"
            )
            found += 1
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
