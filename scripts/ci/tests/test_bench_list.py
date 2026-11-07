"""The one bench list (#810): crates/core/benches/benches.toml and ci/exemptions.toml."""

import shutil
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import bench_list as bl

ROOT = Path(__file__).resolve().parents[3]


def test_repo_is_consistent():
    """Every [[bench]] is listed, every exemption names a listed bench, docs match."""
    assert bl.problems(ROOT) == []


@pytest.fixture
def repo(tmp_path):
    """A copy of the files the bench list reads, safe to break."""
    for rel in (bl.BENCHES, bl.EXEMPTIONS, bl.CARGO, bl.DOC):
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(ROOT / rel, tmp_path / rel)
    return tmp_path


def _edit(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    assert old in text, old
    path.write_text(text.replace(old, new, 1))


def test_bench_missing_from_list_fails(repo):
    _edit(
        repo / bl.CARGO,
        '[[bench]]\nname = "clipping"',
        '[[bench]]\nname = "clipping"\nharness = false\n\n[[bench]]\nname = "new_one"',
    )
    assert any("new_one" in p for p in bl.problems(repo))


def test_listed_bench_without_target_fails(repo):
    _edit(repo / bl.CARGO, '[[bench]]\nname = "algorithmic"\nharness = false\n', "")
    assert any("algorithmic" in p for p in bl.problems(repo))


def test_syscall_bench_on_codspeed_fails(repo):
    _edit(repo / bl.EXEMPTIONS, "[codspeed.pipeline]", "[codspeed_typo.pipeline]")
    assert any("pipeline" in p and "CodSpeed" in p for p in bl.problems(repo))


def test_exemption_for_unknown_bench_fails(repo):
    _edit(repo / bl.EXEMPTIONS, "[codspeed.pipeline]", "[codspeed.pipline]")
    assert any("pipline" in p for p in bl.problems(repo))


def test_hand_edited_doc_table_fails(repo):
    _edit(repo / bl.DOC, "| `clipping` |", "| `clipping` (fast) |")
    assert any(bl.DOC in p for p in bl.problems(repo))


def test_gated_sets():
    benches, ex = bl.load(ROOT)
    weekly = bl.runs(benches, ex, "weekly")
    codspeed = bl.runs(benches, ex, "codspeed")
    # assign_scaling is a wall-clock harness, not criterion: in neither system.
    assert "assign_scaling" not in weekly + codspeed
    # Syscall-dominated benches (CodSpeed's warning on #799/#805) are weekly only.
    for b in ("pipeline", "coverage_fill", "assign"):
        assert b in weekly and b not in codspeed
    assert "algorithmic" in weekly and "algorithmic" in codspeed


@pytest.mark.parametrize(
    ("changed", "expected"),
    [
        ([], []),
        (["README.md"], []),
        (
            ["crates/core/src/sutherland_hodgman.rs"],
            ["clipping", "bbox_containment", "hostile_geometry"],
        ),
        (["crates/core/benches/mvt_encode.rs"], ["mvt_encode"]),
        (["crates/core/src/overview/coalesce.rs"], ["algorithmic"]),
    ],
)
def test_codspeed_selection(changed, expected):
    benches, ex = bl.load(ROOT)
    picked = bl.select(benches, ex, changed)
    assert set(picked) == set(expected) - set(ex.get("codspeed", {}))


def test_global_change_selects_every_codspeed_bench():
    benches, ex = bl.load(ROOT)
    for path in (
        "Cargo.toml",
        "crates/core/benches/benches.toml",
        "ci/exemptions.toml",
    ):
        assert bl.select(benches, ex, [path]) == bl.runs(benches, ex, "codspeed")
