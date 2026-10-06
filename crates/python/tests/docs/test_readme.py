"""The README quickstart runs as written.

Install and download lines are the reader's setup; the test stands in for
them with the built CLI and the cached fixture, then runs every other line.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from doc_blocks import REPO_ROOT, Block, fenced_blocks, stage_input

README = REPO_ROOT / "README.md"
SETUP_PREFIXES = ("cargo install", "pip install", "curl ")


def _quickstart(lang: str) -> Block:
    text = README.read_text(encoding="utf-8")
    section = text.split("## Quickstart", 1)[1].split("\n## ", 1)[0]
    blocks = [b for b in fenced_blocks(section) if b.lang == lang]
    assert blocks, f"README Quickstart has no {lang} block"
    return blocks[0]


def _run(cmd: list[str], cwd: Path, env: dict[str, str]) -> None:
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"{cmd[0]} exited {proc.returncode}:\n{proc.stdout}"


def test_readme_stays_short() -> None:
    """The README is a landing page; depth lives on the docs site."""
    lines = README.read_text(encoding="utf-8").count("\n")
    assert lines <= 80, f"README.md is {lines} lines; move detail to docs/"


@pytest.mark.parametrize("copy", ["cli", "core", "python"])
def test_crate_readme_matches_root(copy: str) -> None:
    """crates.io and PyPI render the crate copies; they must not drift."""
    crate_readme = REPO_ROOT / "crates" / copy / "README.md"
    assert crate_readme.read_bytes() == README.read_bytes(), (
        f"crates/{copy}/README.md differs from README.md; the pre-commit "
        "hook copies it (git config core.hooksPath .githooks)"
    )


def test_quickstart_cli(
    tmp_path: Path, madagascar: Path, doc_env: dict[str, str]
) -> None:
    block = _quickstart("bash")
    lines = [
        line
        for line in block.body.splitlines()
        if line.strip() and not line.lstrip().startswith(SETUP_PREFIXES)
    ]
    assert lines, "README Quickstart runs nothing besides setup"
    stage_input(madagascar, tmp_path)
    _run(["bash", "-euo", "pipefail", "-c", "\n".join(lines)], tmp_path, doc_env)
    assert (tmp_path / "madagascar.pmtiles").stat().st_size > 0


def test_quickstart_python(
    tmp_path: Path, madagascar: Path, doc_env: dict[str, str]
) -> None:
    block = _quickstart("python")
    stage_input(madagascar, tmp_path)
    _run([sys.executable, "-c", block.body], tmp_path, doc_env)
    assert (tmp_path / "madagascar.pmtiles").stat().st_size > 0
