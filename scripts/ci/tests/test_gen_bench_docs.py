"""The generated benchmark regions in the docs match the committed results (#810)."""

import json
import shutil
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import gen_bench_docs as g

REPO = g.ROOT


def test_committed_docs_are_current():
    """Fails if anyone hand-edits a generated region or commits stale results."""
    assert g.run(check=True) == []


def test_every_results_file_has_full_provenance():
    results = g.load_results()  # exits on a missing provenance key
    assert {"e2e-corpus", "e2e-fixtures", "brazil-ftw-2025"} <= set(results)


def test_check_catches_one_hand_edited_number(tmp_path):
    doc = tmp_path / "docs/guides/tippecanoe.md"
    doc.parent.mkdir(parents=True)
    shutil.copy(REPO / "docs/guides/tippecanoe.md", doc)
    assert g.run(root=tmp_path, check=True, files=["docs/guides/tippecanoe.md"]) == []
    doc.write_text(doc.read_text().replace("1,611 / 652 MB", "1,311 / 652 MB"))
    assert g.run(root=tmp_path, check=True, files=["docs/guides/tippecanoe.md"]) == [
        "docs/guides/tippecanoe.md"
    ]
    assert g.run(root=tmp_path, files=["docs/guides/tippecanoe.md"])  # rewrite heals it
    assert g.run(root=tmp_path, check=True, files=["docs/guides/tippecanoe.md"]) == []


def test_readme_and_docs_agree_on_memory_ratio():
    """README said 2.1x while the docs said 2.5x; both now derive from 1,611/652 MB."""
    r = g.load_results()["e2e-corpus"]
    assert g.max_memory_ratio(r) == "2.5×"
    assert "up to 2.5× peak memory" in g.readme_claim(r)


def test_unknown_region_is_an_error():
    with pytest.raises(SystemExit):
        g.rewrite("<!-- bench:nope/headline --><!-- /bench -->", g.load_results())


PROV = {
    "commit": "abc1234", "tylertoo_version": "0.9.0", "host": "m6id.16xlarge",
    "nproc": 64, "kernel": "6.8", "allocator": "mimalloc",
    "tool_versions": {"tippecanoe": "2.79.0"}, "inputs": [], "date": "2026-12-01T00:00:00Z",
}


def test_vida_head_to_head_renders_synthetic_fixture():
    r = {"provenance": PROV, "dataset": "VIDA USA", "features": 153598759,
         "input_bytes": 21.78e9,
         "rows": [
             {"label": "tylertoo", "runs_wall_s": [100, 110, 120], "peak_rss_bytes": 8e9,
              "effective_cores": 30.5, "archive_bytes": 50e9},
             {"label": "tippecanoe", "runs_wall_s": [1000, 1100, 1200],
              "conversion_wall_s": 200, "peak_rss_bytes": 20e9, "archive_bytes": 52e9,
              "note": "fgb in"}]}
    out = g.vida_head_to_head(r)
    assert "| tylertoo | 110 s | 8.0 GB | 30.5 | 50.0 GB |  |" in out
    assert "| tippecanoe | 1,300 s | 20.0 GB | — | 52.0 GB | fgb in |" in out


def test_overture_scaling_renders_synthetic_fixture():
    run = lambda n, wall: {"machines": n, "wall_s": wall, "peak_rss_bytes_per_machine": 100e9,
                           "fetched_bytes": 276.9e9, "local_disk_bytes": 200e9,
                           "merge_s": 30, "cost_usd": 12.5}
    r = {"provenance": PROV, "dataset": "Overture", "features": 2533842612,
         "dataset_bytes": 276.9e9, "runs": [run(1, 6000), run(2, 3000), run(4, 2000)],
         "tippecanoe": {"attempted": False, "needs": ["a full download", "one machine"]}}
    out = g.overture_scaling(r)
    assert "| 2 | 50.0 min | 100.0 GB | 1.00× | 200.0 GB | 30 s | 100% | $12.50 |" in out
    assert "| 4 | 33.3 min" in out and "| 75% |" in out
    assert "not attempted. The same output would need a full download; one machine." in out


def test_showcase_region_round_trips(tmp_path):
    d = tmp_path / "results"
    d.mkdir()
    (d / "vida.json").write_text(json.dumps({
        "provenance": PROV, "dataset": "x", "features": 1, "input_bytes": 1e9,
        "rows": [{"label": "t", "runs_wall_s": [1], "peak_rss_bytes": 1e9, "archive_bytes": 1e9}]}))
    doc = tmp_path / "a.md"
    doc.write_text("<!-- bench:vida/vida-head-to-head -->\nstale\n<!-- /bench -->\n")
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["a.md"]) == ["a.md"]
    g.run(root=tmp_path, results_dir=d, files=["a.md"])
    assert "commit abc1234, tylertoo 0.9.0" in doc.read_text()
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["a.md"]) == []
