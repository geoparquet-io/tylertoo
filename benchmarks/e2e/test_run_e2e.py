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


@pytest.mark.parametrize("fixture,expected", [
    ("madagascar-adm4", True), ("open-buildings", True),
    ("road-detections", False), ("ftw-moldova-z10", True),
    ("fieldmaps-boundaries", False),
    ("custom", False),
])
def test_decay_expectations_require_an_eligible_polygon_cohort(tmp_path, monkeypatch, fixture, expected):
    from types import SimpleNamespace

    calls = []
    def run(argv, **kwargs):
        calls.append(argv)
        return SimpleNamespace(returncode=0, stderr="")

    monkeypatch.setattr(run_e2e.subprocess, "run", run)
    monkeypatch.setattr(run_e2e.shutil, "which", lambda _: "uv")
    monkeypatch.delenv("PMTILES_BIN", raising=False)
    args = SimpleNamespace(
        tylertoo=__file__, settings=["pv0"], tier="full", work=str(tmp_path),
        min_zoom=0, max_zoom=14, exemptions=run_e2e.EXEMPTIONS,
        keep=True, markdown=None, invariants_out=None,
    )
    assert run_e2e.run_invariants(args, {fixture: {"path": __file__, "layer": "test"}}) == 0
    verifier = next(argv for argv in calls if "--strict" in argv)
    assert ("--expect-decay" in verifier) is expected


def test_dataset_help_limits_decay_expectations_to_built_in_fixtures(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["run_e2e.py", "--help"])
    monkeypatch.setenv("COLUMNS", "200")  # argparse wraps at hyphens; keep the phrase on one line
    with pytest.raises(SystemExit) as exc:
        run_e2e.main()
    assert exc.value.code == 0
    help_text = " ".join(capsys.readouterr().out.split())
    assert "skips --expect-decay (only established for built-in fixtures)" in help_text


def test_after_warmup_hook_and_needs(tmp_path):
    src, stable = tmp_path / "a.fgb", tmp_path / "tip.fgb"
    legs = {"convert": {"argv": ["sh", "-c", f"echo x > {src}"], "unlink_first": src,
                        "after_warmup": lambda _m: stable.write_text(src.read_text())},
            "tippecanoe": {"argv": ["cat", str(stable)], "needs": stable}}
    summ, _ = run_e2e.run_rounds(legs, 1, tmp_path)
    assert set(summ) == {"convert", "tippecanoe"}
    with pytest.raises(run_e2e.MissingInput):
        run_e2e.run_rounds({"tippecanoe": {"argv": ["true"], "needs": tmp_path / "nope"}}, 1,
                           tmp_path)


def test_tippecanoe_reads_the_converter_whose_time_is_reported(tmp_path):
    """The converter is picked once, at warm-up: its output is tippecanoe's
    input, and its time is the one the end-to-end sum reports."""
    slow, fast = tmp_path / "slow.fgb", tmp_path / "fast.fgb"
    converters = {
        "slow": (["sh", "-c", f"sleep 0.4; echo slow > {slow}"], slow),
        "fast": (["sh", "-c", f"echo fast > {fast}"], fast),
    }
    tip_in = tmp_path / "tip.fgb"
    order = tmp_path / "read"
    legs, chosen = run_e2e.converter_legs(converters, tip_in)
    legs["tippecanoe_fgb"] = {"needs": tip_in,
                              "argv": ["sh", "-c", f"cat {tip_in} >> {order}"]}
    summ, _ = run_e2e.run_rounds(legs, 2, tmp_path)
    assert chosen["tool"] == "fast"
    assert set(order.read_text().split()) == {"fast"}
    assert "convert:fast" in summ
