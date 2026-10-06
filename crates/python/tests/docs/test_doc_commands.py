"""Every command a page shows still exists, with every flag it uses.

The guides show commands that are too heavy to run in CI (remote inputs,
sharded fleets, multi-GB files). This test parses each ``tylertoo`` call
out of their bash blocks and checks the subcommand and each flag against
the CLI's own ``--help``, so a renamed or removed flag fails here instead
of in a reader's terminal.
"""

from __future__ import annotations

import re
import shlex
import subprocess
from collections.abc import Sequence
from functools import cache
from pathlib import Path

import pytest
from doc_blocks import (
    NOTEST,
    REPO_ROOT,
    example_steps,
    fence_for,
    fenced_blocks,
    site_pages,
)

SHELL_LANGS = {"bash", "sh", "shell", "console"}
# The bare form `tylertoo IN OUT` is the `tiles` subcommand.
DEFAULT_SUBCOMMAND = "tiles"
_NUMBER = re.compile(r"^-\d")
_OPERATORS = set("|&;()<>")


@cache
def _help(binary: Path, subcommand: str | None) -> str:
    cmd = [str(binary)] + ([subcommand] if subcommand else []) + ["--help"]
    return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout


def _subcommands(binary: Path) -> set[str]:
    text = _help(binary, None).split("Commands:", 1)[1].split("\n\n", 1)[0]
    return {line.split()[0] for line in text.splitlines() if line.strip()}


def _tokens(line: str) -> list[str]:
    lexer = shlex.shlex(line, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        return list(lexer)
    except ValueError:  # unbalanced quote inside a prose placeholder
        return []


def _calls_in_line(tokens: list[str]) -> list[list[str]]:
    """Split one line at shell operators; keep commands that run tylertoo."""
    commands: list[list[str]] = [[]]
    for tok in tokens:
        if set(tok) <= _OPERATORS:
            commands.append([])
        else:
            commands[-1].append(tok)
    # Command position only: `-p tylertoo` in a cargo call is a package.
    return [cmd[1:] for cmd in commands if cmd[:1] == ["tylertoo"]]


def _invocations(script: str) -> list[list[str]]:
    """Argument lists for each `tylertoo ...` call in a shell snippet."""
    joined = re.sub(r"\\\n", " ", script)
    return [
        call for line in joined.splitlines() for call in _calls_in_line(_tokens(line))
    ]


def _shell_blocks() -> Sequence[object]:
    found = []
    for page in site_pages():
        rel = page.relative_to(REPO_ROOT)
        for i, block in enumerate(fenced_blocks(page.read_text(encoding="utf-8"))):
            if block.lang in SHELL_LANGS and "tylertoo" in block.body:
                where = f"{rel}#{i}"
                found.append(pytest.param(where, block.body, id=where))
    return found


def _unknown_flags(binary: Path, args: list[str], subcommands: set[str]) -> list[str]:
    if not args or args[0] in {"--help", "-h", "--version", "-V"}:
        return []
    sub = args[0] if args[0] in subcommands else DEFAULT_SUBCOMMAND
    help_text = _help(binary, sub)
    flags = [
        arg.split("=", 1)[0]
        for arg in args
        if arg.startswith("-") and not _NUMBER.match(arg)
    ]
    return [
        f"`tylertoo {sub}` has no flag {flag}"
        for flag in flags
        if not re.search(rf"(?<![\w-]){re.escape(flag)}(?![\w-])", help_text)
    ]


@pytest.mark.parametrize(("where", "body"), _shell_blocks())
def test_commands_match_cli(where: str, body: str, tylertoo_bin: Path) -> None:
    subcommands = _subcommands(tylertoo_bin)
    problems = [
        problem
        for args in _invocations(body)
        for problem in _unknown_flags(tylertoo_bin, args, subcommands)
    ]
    assert not problems, f"{where}:\n" + "\n".join(problems)


def test_untested_python_blocks_are_marked() -> None:
    """A Python block runs in a test, or says it does not."""
    tested = {
        fence_for(step)
        for example in (REPO_ROOT / "examples").glob("*/")
        for step in example_steps(example)
        if step.suffix == ".py"
    }
    unmarked = []
    for page in site_pages():
        text = page.read_text(encoding="utf-8")
        if page == REPO_ROOT / "README.md":
            # test_readme.py runs the Quickstart section's block.
            head, _, rest = text.partition("## Quickstart")
            text = head + rest.partition("\n## ")[2]
        for block in fenced_blocks(text):
            if block.lang != "python" or NOTEST in block.body:
                continue
            if f"```python\n{block.body}\n```" not in tested:
                unmarked.append(f"{page.relative_to(REPO_ROOT)}: {block.body[:60]!r}")
    assert not unmarked, (
        f"Python blocks no test runs; add a `{NOTEST}` comment or a test:\n"
        + "\n".join(unmarked)
    )
