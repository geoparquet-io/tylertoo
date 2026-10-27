"""Tests for scripts/ci/baseline_ratchet.py (#779).

The must-fail case is real history: 09673f5 re-banded Madagascar's
tippecanoe parity bands in the PR that moved them, widening z3-z6
features from [0.29, 0.55] to [0.86, 1.50] with no evidence trailer.
These tests need the full git history (CI checks out with fetch-depth 0).
"""

from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "ci"))

import baseline_ratchet as br  # noqa: E402

WIDENING_COMMIT = "09673f5"


def git(*args: str, cwd: Path = ROOT) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


def bands_at(rev: str) -> dict:
    return tomllib.loads(git("show", f"{rev}:{br.BANDS_FILE}"))


@pytest.fixture(scope="module")
def history() -> None:
    proc = subprocess.run(["git", "cat-file", "-e", f"{WIDENING_COMMIT}^{{commit}}"], cwd=ROOT, capture_output=True)
    assert proc.returncode == 0, (
        f"{WIDENING_COMMIT} is not in this checkout; the ratchet tests need full history "
        "(actions/checkout with fetch-depth: 0)"
    )


# --- the real widening ------------------------------------------------------


def test_09673f5_widening_is_flagged(history: None) -> None:
    found = br.band_loosenings(bands_at(f"{WIDENING_COMMIT}^"), bands_at(WIDENING_COMMIT))
    joined = "\n".join(found)
    # z3-z6 features: [0.29, 0.55] -> [0.86, 1.50], the widening the gates report cites.
    for z in (3, 4, 5, 6):
        assert f"madagascar-adm4 z{z} features: [0.29, 0.55] -> [0.86, 1.5] widened" in joined
    # Narrowed-only metrics are not reported: open-buildings did not move.
    assert "open-buildings" not in joined


def test_09673f5_fails_the_check_without_a_trailer(
    history: None, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.chdir(ROOT)
    assert br.main(["--base", f"{WIDENING_COMMIT}^", "--head", WIDENING_COMMIT]) == 1
    out = capsys.readouterr().out
    assert "FAIL" in out and "madagascar-adm4 z3 features" in out


def test_the_reverse_of_09673f5_is_not_a_loosening_everywhere(history: None) -> None:
    # Going back narrows z3-z6 features' upper edge; it lowers their lower
    # edge though, which admits ratios the new band rejected.
    found = br.band_loosenings(bands_at(WIDENING_COMMIT), bands_at(f"{WIDENING_COMMIT}^"))
    assert any("z3 features" in f and "lower edge 0.86 -> 0.29" in f for f in found)
    assert not any("upper edge" in f and "z3 features" in f for f in found)


# --- band semantics ---------------------------------------------------------


def doc(**bands: list[float]) -> dict:
    return {
        "gate": {"mode": "matched", "min_reference": 20},
        "datasets": {"d": {"totals": {"tiles": [0.99, 1.01]}, "zoom_bands": [{"zooms": [1, 2], **bands}]}},
    }


def test_narrowing_passes() -> None:
    assert br.band_loosenings(doc(features=[0.5, 0.9]), doc(features=[0.6, 0.8])) == []


def test_shift_is_a_loosening() -> None:
    found = br.band_loosenings(doc(features=[0.5, 0.9]), doc(features=[0.6, 1.0]))
    assert found == [
        "d z1 features: [0.5, 0.9] -> [0.6, 1] moved (upper edge 0.9 -> 1)",
        "d z2 features: [0.5, 0.9] -> [0.6, 1] moved (upper edge 0.9 -> 1)",
    ]


def test_removed_band_is_a_loosening() -> None:
    found = br.band_loosenings(doc(features=[0.5, 0.9], bytes=[0.1, 0.2]), doc(features=[0.5, 0.9]))
    assert any("bytes" in f and "removed" in f for f in found)


def test_resplit_ranges_compare_zoom_by_zoom() -> None:
    old = doc(features=[0.5, 0.9])
    new = doc(features=[0.5, 0.9])
    new["datasets"]["d"]["zoom_bands"] = [
        {"zooms": [1, 1], "features": [0.5, 0.9]},
        {"zooms": [2, 2], "features": [0.4, 0.9]},
    ]
    assert br.band_loosenings(old, new) == ["d z2 features: [0.5, 0.9] -> [0.4, 0.9] widened (lower edge 0.5 -> 0.4)"]


def test_first_covering_entry_wins_like_compare_tippecanoe() -> None:
    d = doc(features=[0.5, 0.9])
    d["datasets"]["d"]["zoom_bands"].append({"zooms": [1, 3], "features": [0.0, 9.0]})
    bands = br.effective_bands(d)
    assert bands[("d", "z1", "features")] == (0.5, 0.9)
    assert bands[("d", "z3", "features")] == (0.0, 9.0)


def test_gate_settings() -> None:
    old = doc()
    new = doc()
    new["gate"]["min_reference"] = 50
    assert br.band_loosenings(old, new) == ["gate.min_reference raised 20 -> 50 (fewer zooms are judged)"]
    new = doc()
    new["gate"]["mode"] = "defaults"
    assert "gate.mode changed" in br.band_loosenings(old, new)[0]


# --- trailers and paths -----------------------------------------------------


def test_trailer_validation() -> None:
    good, bad = br.valid_trailers(["#761 z3 features 0.910 on CI run 12345", "#761 looks fine", "761 z3 0.9", "  "])
    assert good == ["#761 z3 features 0.910 on CI run 12345"]
    assert len(bad) == 2


@pytest.mark.parametrize(
    ("path", "watched"),
    [
        ("benchmarks/overview/ci_baseline.json", True),
        ("tests/fixtures/guard/br-clip-divergence.golden.txt", True),
        ("tests/fixtures/golden/open-buildings.pmtiles", True),
        ("tests/fixtures/golden/decoded/open-buildings-z5-x16-y11.geojson", True),
        ("tests/fixtures/golden/README.md", False),
        ("benchmarks/overview/ci_guard.py", False),
    ],
)
def test_baseline_paths(path: str, watched: bool) -> None:
    assert br.is_baseline_path(path) is watched


# --- end to end on a scratch repository -------------------------------------


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    git("init", "-q", "-b", "main", cwd=tmp_path)
    git("config", "user.email", "t@example.com", cwd=tmp_path)
    git("config", "user.name", "t", cwd=tmp_path)
    (tmp_path / "benchmarks/e2e").mkdir(parents=True)
    (tmp_path / "benchmarks/overview").mkdir(parents=True)
    (tmp_path / br.BANDS_FILE).write_text(
        '[gate]\nmode = "matched"\nmin_reference = 20\n'
        "[[datasets.d.zoom_bands]]\nzooms = [1, 2]\nfeatures = [0.5, 0.9]\n"
    )
    (tmp_path / "benchmarks/overview/ci_baseline.json").write_text('{"a": 1}\n')
    git("add", "-A", cwd=tmp_path)
    git("commit", "-q", "-m", "base", cwd=tmp_path)
    return tmp_path


def run_check(repo: Path, monkeypatch: pytest.MonkeyPatch) -> int:
    monkeypatch.chdir(repo)
    return br.main(["--base", "main~1", "--head", "HEAD"])


def commit(repo: Path, message: str) -> None:
    git("commit", "-q", "-am", message, cwd=repo)


def test_widening_without_trailer_fails(repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = repo / br.BANDS_FILE
    p.write_text(p.read_text().replace("[0.5, 0.9]", "[0.4, 0.9]"))
    commit(repo, "test: widen\n\nCo-Authored-By: x <x@example.com>")
    assert run_check(repo, monkeypatch) == 1


def test_widening_with_trailer_passes(repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = repo / br.BANDS_FILE
    p.write_text(p.read_text().replace("[0.5, 0.9]", "[0.4, 0.9]"))
    commit(repo, "test: widen\n\nBaseline-change: #761 z1 features 0.41 on CI\nCo-Authored-By: x <x@example.com>")
    assert run_check(repo, monkeypatch) == 0


def test_trailer_without_evidence_fails(repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = repo / "benchmarks/overview/ci_baseline.json"
    p.write_text('{"a": 2}\n')
    commit(repo, "test: rebase\n\nBaseline-change: #761 regenerated")
    assert run_check(repo, monkeypatch) == 1


def test_trailer_on_an_earlier_commit_covers_the_pr(repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    (repo / "benchmarks/overview/ci_baseline.json").write_text('{"a": 2}\n')
    commit(repo, "test: regen\n\nBaseline-change: #761 polygons z5 1,204 -> 1,198 features")
    (repo / "x.txt").write_text("x")
    git("add", "x.txt", cwd=repo)
    commit(repo, "chore: unrelated")
    monkeypatch.chdir(repo)
    assert br.main(["--base", "main~2", "--head", "HEAD"]) == 0


def test_narrowing_and_unrelated_changes_pass(repo: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = repo / br.BANDS_FILE
    p.write_text(p.read_text().replace("[0.5, 0.9]", "[0.6, 0.8]"))
    commit(repo, "test: tighten")
    assert run_check(repo, monkeypatch) == 0
