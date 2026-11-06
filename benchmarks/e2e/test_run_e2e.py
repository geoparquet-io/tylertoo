"""Tests for run_e2e.py's timed rounds (#810): every leg is timed by
run_manifest, and the tools alternate within a round.

    uv run --no-project --python 3.12 --with pytest pytest benchmarks/e2e/test_run_e2e.py
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run_e2e  # noqa: E402

pytestmark = pytest.mark.skipif(not Path("/usr/bin/time").exists(), reason="needs GNU time")


def _leg(order: Path, name: str, fail: bool = False, **kw) -> dict:
    return {"argv": ["sh", "-c", f"echo {name} >> {order}" + ("; exit 3" if fail else "")], **kw}


def test_legs_alternate_within_each_round(tmp_path):
    order = tmp_path / "order"
    legs = {n: _leg(order, n) for n in ("tylertoo", "convert", "tippecanoe")}
    summ, failed = run_e2e.run_rounds(legs, 2, tmp_path)
    assert failed == {}
    seen = order.read_text().split()
    assert seen[:3] == ["tylertoo", "convert", "tippecanoe"]  # warm-up, dependency order
    assert seen[3:6] == ["tylertoo", "convert", "tippecanoe"]
    assert seen[6:] == ["tippecanoe", "convert", "tylertoo"]
    s = summ["tylertoo"]
    assert s["runs"] == 2 and len(s["wall_all_s"]) == 2  # noqa: PLR2004
    for key in ("wall_median_s", "wall_min_s", "wall_max_s", "peak_rss_bytes", "cpu_pct",
                "cpu_s", "effective_cores", "argv"):
        assert key in s, key
    assert s["peak_rss_bytes"] > 0


def test_a_failing_optional_leg_drops_its_group_and_is_recorded(tmp_path):
    order = tmp_path / "order"
    legs = {"tylertoo": _leg(order, "tylertoo"),
            "convert_geojsonseq": _leg(order, "gj", fail=True, optional="geojson"),
            "tippecanoe_geojsonseq": _leg(order, "tgj", optional="geojson")}
    summ, failed = run_e2e.run_rounds(legs, 1, tmp_path)
    assert set(summ) == {"tylertoo"}
    assert set(failed) == {"convert_geojsonseq"}
    assert "tgj" not in order.read_text().split()


def test_a_failing_required_leg_raises(tmp_path):
    legs = {"tylertoo": _leg(tmp_path / "order", "tylertoo", fail=True)}
    with pytest.raises(run_e2e.CommandFailed):
        run_e2e.run_rounds(legs, 1, tmp_path)


def test_after_warmup_hook_and_needs(tmp_path):
    src, stable = tmp_path / "a.fgb", tmp_path / "tip.fgb"
    legs = {"convert": {"argv": ["sh", "-c", f"echo x > {src}"], "unlink_first": src,
                        "after_warmup": lambda: stable.write_text(src.read_text())},
            "tippecanoe": {"argv": ["cat", str(stable)], "needs": stable}}
    summ, _ = run_e2e.run_rounds(legs, 1, tmp_path)
    assert set(summ) == {"convert", "tippecanoe"}
    with pytest.raises(run_e2e.MissingInput):
        run_e2e.run_rounds({"tippecanoe": {"argv": ["true"], "needs": tmp_path / "nope"}}, 1,
                           tmp_path)
