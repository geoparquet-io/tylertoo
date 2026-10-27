"""Tests for scripts/ci/mirrored_hunks.py (#779).

Run: uv run --no-project --with pytest pytest scripts/ci/tests -q
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import mirrored_hunks as mh
import pytest

FIXTURES = Path(__file__).parent / "fixtures"

BLOCK = [
    "    // #507: `build_writer_options` may have raised the cap to fit parquet's",
    "    // row-group ceiling. Record it before the options move into the writer.",
    "    let effective_max_row_group_size = (writer_opts.max_row_group_size",
    "        != options.max_row_group_size)",
    "        .then_some(writer_opts.max_row_group_size);",
]


def added(path: str, start: int, lines: list[str]) -> str:
    """A unified=0 diff section adding ``lines`` to ``path`` at ``start``."""
    body = "\n".join(f"+{ln}" for ln in lines)
    return (
        f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
        f"@@ -{start - 1},0 +{start},{len(lines)} @@\n{body}\n"
    )


def no_files(_path: str) -> None:
    return None


def test_8576c61_row_group_block_in_both_engines_fails() -> None:
    """The real #507 commit: the same block pasted into convert.rs and
    stream.rs. Must fail, and must name both files."""
    diff = (FIXTURES / "8576c61.diff").read_text()
    status, text = mh.run(diff, "", no_files)
    assert status == 1
    pairs = [(m.a[0].path, m.b[0].path) for m in mh.find_matches(mh.parse_added(diff, no_files))]
    assert ("crates/core/src/overview/convert.rs", "crates/core/src/overview/stream.rs") in pairs
    assert "may have raised the cap" in text


def test_pure_rename_passes() -> None:
    """A rename adds no lines at all."""
    diff = (
        "diff --git a/crates/core/src/old_name.rs b/crates/core/src/new_name.rs\n"
        "similarity index 100%\n"
        "rename from crates/core/src/old_name.rs\n"
        "rename to crates/core/src/new_name.rs\n"
    )
    assert mh.run(diff, "", no_files)[0] == 0


def test_moved_block_passes() -> None:
    """A block deleted at one site and added at another is a move, not a copy."""
    removed = (
        "diff --git a/crates/core/src/a.rs b/crates/core/src/a.rs\n"
        "--- a/crates/core/src/a.rs\n+++ b/crates/core/src/a.rs\n"
        "@@ -10,5 +9,0 @@\n" + "\n".join(f"-{ln}" for ln in BLOCK) + "\n"
    )
    diff = removed + added("crates/core/src/b.rs", 20, BLOCK)
    assert mh.run(diff, "", no_files)[0] == 0


def test_same_block_twice_in_one_file_fails() -> None:
    """Two separate sites of one file count as two places."""
    diff = added("crates/core/src/a.rs", 10, BLOCK).replace(
        f"+{BLOCK[-1]}\n", f"+{BLOCK[-1]}\n@@ -80,0 +86,5 @@\n" + "\n".join(f"+{ln}" for ln in BLOCK) + "\n"
    )
    status, _ = mh.run(diff, "", no_files)
    assert status == 1


def test_repetition_inside_one_new_block_passes() -> None:
    """One contiguous added block that repeats an argument list is one site."""
    args = [
        "    level_row_counts,",
        "    writer_opts.max_row_group_size,",
        "    writer_opts.policy,",
        "    &level_zooms_for_rows,",
        "    finest_zoom_of_levels,",
    ]
    lines = ["let a = autoscale_cap(", *args, ");", "let b = projected_row_groups(", *args, ");"]
    assert mh.run(added("crates/core/src/a.rs", 1, lines), "", no_files)[0] == 0


def test_near_identical_with_one_renamed_line_fails() -> None:
    """Four of five lines equal is still a copy."""
    other = [*BLOCK[:3], "        != opts.max_row_group_size)", BLOCK[4]]
    diff = added("crates/core/src/overview/convert.rs", 5, BLOCK) + added(
        "crates/core/src/overview/stream.rs", 9, other
    )
    assert mh.run(diff, "", no_files)[0] == 1


def test_whitespace_and_case_are_normalized() -> None:
    other = ["\t" + ln.strip().upper() + "  " for ln in BLOCK]
    diff = added("crates/a.rs", 1, BLOCK) + added("crates/b.rs", 1, other)
    assert mh.run(diff, "", no_files)[0] == 1


def test_four_lines_are_not_enough() -> None:
    diff = added("crates/a.rs", 1, BLOCK[:4]) + added("crates/b.rs", 1, BLOCK[:4])
    assert mh.run(diff, "", no_files)[0] == 0


def test_trivial_lines_do_not_match() -> None:
    """Braces, `Ok(())` and imports carry no signal."""
    trivial = ["}", "    }", ")?;", "Ok(())", "use std::path::Path;", "use std::sync::Arc;", "} else {"]
    diff = added("crates/a.rs", 1, trivial) + added("crates/b.rs", 1, trivial)
    assert mh.run(diff, "", no_files)[0] == 0


def test_test_files_are_out_of_scope() -> None:
    diff = added("crates/core/tests/a.rs", 1, BLOCK) + added("crates/python/tests/test_b.py", 1, BLOCK)
    assert mh.run(diff, "", no_files)[0] == 0


def test_cfg_test_items_are_out_of_scope() -> None:
    """Lines inside a `#[cfg(test)] mod tests` of the head file are skipped."""
    head = "fn prod() {}\n#[cfg(test)]\nmod tests {\n" + "\n".join(f"    {ln}" for ln in BLOCK) + "\n}\n"

    def read(path: str) -> str | None:
        return head if path == "crates/core/src/a.rs" else None

    diff = added("crates/core/src/a.rs", 4, BLOCK) + added("crates/core/src/b.rs", 1, BLOCK)
    assert mh.run(diff, "", read)[0] == 0
    assert mh.run(diff, "", no_files)[0] == 1


@pytest.mark.parametrize(
    ("message", "exempt"),
    [
        ("fix: x\n\nMirrored-ok: #507 both engines until F1 lands\n", True),
        ("fix: x\n\nMirrored-ok: because\n", False),
        ("fix: x\n\nMirrored-ok: #507\n", False),
        ("fix: x\n", False),
    ],
)
def test_trailer_exemption_needs_issue_and_reason(message: str, exempt: bool) -> None:
    diff = (FIXTURES / "8576c61.diff").read_text()
    status, text = mh.run(diff, message, no_files)
    assert status == (0 if exempt else 1)
    if exempt:
        assert "::warning" in text and "::error" not in text


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    _git(tmp_path, "init", "-q", "-b", "main")
    _git(tmp_path, "config", "user.email", "t@example.com")
    _git(tmp_path, "config", "user.name", "t")
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "convert.rs").write_text("fn a() {}\n")
    (tmp_path / "src" / "stream.rs").write_text("fn b() {}\n")
    _git(tmp_path, "add", ".")
    _git(tmp_path, "commit", "-q", "-m", "base")
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _commit_block_in_both(repo: Path, message: str) -> None:
    for name in ("convert.rs", "stream.rs"):
        p = repo / "src" / name
        p.write_text(p.read_text() + "fn f() {\n" + "\n".join(BLOCK) + "\n}\n")
    _git(repo, "commit", "-q", "-am", message)


def test_git_mode_fails_on_a_mirrored_commit(repo: Path) -> None:
    _commit_block_in_both(repo, "fix: mirror")
    assert mh.main(["--base", "main~1", "--head", "HEAD"]) == 1


def test_git_mode_honours_the_trailer(repo: Path) -> None:
    _commit_block_in_both(repo, "fix: mirror\n\nMirrored-ok: #779 test fixture")
    assert mh.main(["--base", "main~1", "--head", "HEAD"]) == 0


def test_git_mode_passes_a_rename(repo: Path) -> None:
    (repo / "src" / "convert.rs").write_text("fn a() {\n" + "\n".join(BLOCK) + "\n}\n")
    _git(repo, "commit", "-q", "-am", "grow")
    _git(repo, "mv", "src/convert.rs", "src/engine.rs")
    _git(repo, "commit", "-q", "-m", "rename")
    assert mh.main(["--base", "HEAD~1", "--head", "HEAD"]) == 0
