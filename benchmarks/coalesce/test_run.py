"""
Tests for run.py and the run_matrix.py helpers it reuses (#755).

    uv run --no-project --with pytest pytest benchmarks/coalesce/test_run.py
"""

from __future__ import annotations

import copy
import hashlib
import json
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run  # noqa: E402
import run_matrix  # noqa: E402


@pytest.fixture(scope="module")
def cfg() -> dict:
    return run.load_config(HERE / "bench.toml")


# --- config -----------------------------------------------------------------


def test_small_profile_has_no_nz_and_no_tippecanoe(cfg):
    plan = run.profile_plan(cfg, "small")
    assert plan["datasets"] == ["madagascar", "moldova"]
    assert "tippecanoe" not in plan["settings"]
    assert {w["dataset"] for w in plan["windows"]} <= {"madagascar", "moldova"}
    assert plan["tippecanoe"] == []


def test_full_profile_adds_nz_and_tippecanoe(cfg):
    plan = run.profile_plan(cfg, "full")
    assert "nz" in plan["datasets"]
    assert plan["tippecanoe"] == ["nz", "moldova"]
    assert {w["name"] for w in plan["windows"]} >= {"auckland-city", "upper-north-island"}


def test_every_dataset_pins_a_sha256(cfg):
    for name, ds in cfg["datasets"].items():
        assert len(ds["sha256"]) == 64, name


def test_unknown_dataset_in_profile_is_an_error(cfg):
    bad = copy.deepcopy(cfg)
    bad["profiles"]["small"]["datasets"].append("atlantis")
    with pytest.raises(run.ConfigError, match="atlantis"):
        run.validate_config(bad)


def test_unknown_setting_in_profile_is_an_error(cfg):
    bad = copy.deepcopy(cfg)
    bad["profiles"]["small"]["settings"].append("sf9")
    with pytest.raises(run.ConfigError, match="sf9"):
        run.validate_config(bad)


def test_tippecanoe_args_are_per_dataset(cfg):
    nz = cfg["tippecanoe"]["datasets"]["nz"]["args"]
    md = cfg["tippecanoe"]["datasets"]["moldova"]["args"]
    assert "-z14" in nz and "-z12" in md
    assert run.tippecanoe_key("s", "i", nz, "g") != run.tippecanoe_key("s", "i", md, "g")


def test_tippecanoe_on_unknown_dataset_is_an_error(cfg):
    bad = copy.deepcopy(cfg)
    bad["tippecanoe"]["datasets"]["atlantis"] = {"args": []}
    with pytest.raises(run.ConfigError, match="atlantis"):
        run.validate_config(bad)


def test_window_on_unknown_dataset_is_an_error(cfg):
    bad = copy.deepcopy(cfg)
    bad["windows"][0]["dataset"] = "atlantis"
    with pytest.raises(run.ConfigError, match="atlantis"):
        run.validate_config(bad)


# --- cache keys -------------------------------------------------------------


def test_tippecanoe_key_is_stable_and_tracks_its_inputs():
    g = "geoparquet-io==1.7.0"
    a = run.tippecanoe_key("68ab8dc", "f" * 64, ["-z14", "-b", "8"], g)
    assert a == run.tippecanoe_key("68ab8dc", "f" * 64, ["-z14", "-b", "8"], g)
    assert a != run.tippecanoe_key("68ab8dd", "f" * 64, ["-z14", "-b", "8"], g)
    assert a != run.tippecanoe_key("68ab8dc", "e" * 64, ["-z14", "-b", "8"], g)
    assert a != run.tippecanoe_key("68ab8dc", "f" * 64, ["-z14", "-b", "4"], g)


def test_cell_key_changes_with_flags_and_repeats():
    base = dict(binary_sha="b" * 64, input_sha="i" * 64, convert=["--simplify-factor", "0"],
                valve="merge", max_zoom=12, max_tile_size=512000,
                max_compressed_tile_size=0, repeats=3, harness="h")
    k = run.cell_key(**base)
    assert k == run.cell_key(**base)
    assert k != run.cell_key(**{**base, "convert": ["--simplify-factor", "0.25"]})
    assert k != run.cell_key(**{**base, "repeats": 1})
    assert k != run.cell_key(**{**base, "binary_sha": "c" * 64})


# --- fetch ------------------------------------------------------------------


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def test_fetch_downloads_and_verifies(tmp_path):
    src = tmp_path / "src.bin"
    src.write_bytes(b"hello")
    dest = tmp_path / "cache" / "hello.bin"
    run.fetch(src.as_uri(), dest, _sha(b"hello"))
    assert dest.read_bytes() == b"hello"


def test_fetch_rejects_a_wrong_checksum_and_leaves_nothing(tmp_path):
    src = tmp_path / "src.bin"
    src.write_bytes(b"tampered")
    dest = tmp_path / "cache" / "hello.bin"
    with pytest.raises(run.FetchError, match="sha256"):
        run.fetch(src.as_uri(), dest, _sha(b"hello"))
    assert not dest.exists()
    assert not list(dest.parent.glob("*"))


def test_fetch_reuses_a_verified_copy(tmp_path):
    dest = tmp_path / "hello.bin"
    dest.write_bytes(b"hello")
    run.fetch((tmp_path / "does-not-exist").as_uri(), dest, _sha(b"hello"))
    assert dest.read_bytes() == b"hello"


def test_fetch_replaces_a_corrupt_copy(tmp_path):
    src = tmp_path / "src.bin"
    src.write_bytes(b"hello")
    dest = tmp_path / "hello.bin"
    dest.write_bytes(b"half")
    run.fetch(src.as_uri(), dest, _sha(b"hello"))
    assert dest.read_bytes() == b"hello"


# --- cells and tables -------------------------------------------------------


def test_median():
    assert run.median([3.0, 1.0, 2.0]) == 2.0
    assert run.median([1.0, 2.0]) == 1.5
    assert run.median([]) is None


def test_coverage_cell_shows_added_only_from_half_a_percent():
    assert run.coverage_cell({"missing": 0.285, "added": 0.51}) == "28.5% (+51%)"
    assert run.coverage_cell({"missing": 0.032, "added": 0.004}) == "3.2%"
    assert run.coverage_cell(None) == ""


def test_coverage_cell_flags_changes_beyond_the_noise_floor():
    base = {"missing": 0.10, "added": 0.0}
    assert run.coverage_cell({"missing": 0.129, "added": 0.0}, base, 3.0) == "12.9% (+2.9)"
    assert run.coverage_cell({"missing": 0.131, "added": 0.0}, base, 3.0) == "13.1% (+3.1) ▲"
    assert run.coverage_cell({"missing": 0.069, "added": 0.0}, base, 3.0) == "6.9% (−3.1) ▼"
    # Added area that grows beyond the floor is flagged even when missing holds.
    assert run.coverage_cell({"missing": 0.10, "added": 0.04}, base, 3.0) == \
        "10.0% (+4%) (+0.0, added +4.0) ▲"


def test_time_cell_flags_changes_beyond_the_noise_floor():
    assert run.time_cell(8.61) == "8.61 s"
    assert run.time_cell(8.61, 8.0, 0.10) == "8.61 s (+8%)"
    assert run.time_cell(8.90, 8.0, 0.10) == "8.90 s (+11%) ▲"
    assert run.time_cell(7.10, 8.0, 0.10) == "7.10 s (−11%) ▼"
    assert run.time_cell(None) == ""


def test_summarize_cell_takes_medians_per_stage():
    convert_runs = [{"status": "ok", "wall_s": w, "peak_rss_kib": r}
                    for w, r in ((2.0, 100), (1.0, 300), (3.0, 200))]
    exports = [{"export_run": {"status": "ok", "wall_s": w, "peak_rss_kib": 50},
                "pmtiles_bytes": 10, "pmtiles_sha256": "x", "merged_features": 7,
                "oversized_tiles": 0, "valve_dropped_features": 1}
               for w in (5.0, 4.0, 6.0)]
    s = run.summarize_cell(convert_runs, exports, overview_bytes=99)
    assert s["status"] == "ok"
    assert s["convert"]["median_s"] == 2.0
    assert s["convert"]["peak_rss_kib"] == 200
    assert s["export"]["median_s"] == 5.0
    assert s["total_s"] == 7.0
    assert s["merged_features"] == 7
    assert s["overview_bytes"] == 99


def test_summarize_cell_reports_a_failed_stage():
    s = run.summarize_cell([{"status": "TIMEOUT", "wall_s": 9.0, "peak_rss_kib": 1}], [], None)
    assert s["status"] == "convert TIMEOUT"
    assert s["total_s"] is None


def _report() -> dict:
    cell = {"status": "ok", "convert": {"median_s": 2.0, "peak_rss_kib": 2 * 1048576},
            "export": {"median_s": 6.5, "peak_rss_kib": 1048576}, "total_s": 8.5,
            "overview_bytes": 233_000_000, "pmtiles_bytes": 84_000_000, "merged_features": 900,
            "oversized_tiles": 0}
    before = {**cell, "total_s": 6.5, "convert": {"median_s": 1.7, "peak_rss_kib": 1048576},
              "export": {"median_s": 4.8, "peak_rss_kib": 1048576}}
    return {
        "profile": "small",
        "noise": {"coverage_pp": 3.0, "time_ratio": 0.10},
        "host": {"cpu": "test", "cores": 4, "mem_gib": 8.0},
        "binary": {"git": "abc123", "sha256": "0" * 64},
        "datasets": {"moldova": {"label": "FTW Moldova", "features": 631910}},
        "settings": {"before-745": {"label": "Before #745"}, "default": {"label": "Defaults"}},
        "cells": {"moldova/before-745": before, "moldova/default": cell},
        "windows": {"moldova-country": {
            "label": "Moldova, whole country", "dataset": "moldova",
            "zooms": {"3": {"before-745": {"missing": 0.998, "added": 0.0},
                            "default": {"missing": 0.158, "added": 0.51}}}}},
        "coverage": {}, "metrics": {}, "tippecanoe": {},
    }


def test_markdown_has_performance_and_coverage_rows():
    md = run.render_markdown(_report())
    assert "| Defaults | 2.00 s | 2.00 GiB | 6.50 s | 1.00 GiB | 8.50 s | 1.31× |" in md
    assert "| Moldova, whole country | 3 | 99.8% | 15.8% (+51%) |" in md
    assert "noise floor" in md.lower()


def test_markdown_against_a_baseline_flags_only_beyond_the_floor():
    cur, base = _report(), _report()
    cur["windows"]["moldova-country"]["zooms"]["3"]["default"]["missing"] = 0.20
    md = run.render_markdown(cur, base)
    assert "20.0% (+51%) (+4.2) ▲" in md
    assert "99.8% (+0.0) |" in md


# --- run_matrix argv (the STAGES path must not change) ----------------------


def test_convert_argv_matches_the_stages_path():
    argv = run_matrix.convert_argv("/bin/tt", Path("in.parquet"), Path("ov.parquet"), 12,
                                   Path("c.json"), ["--polygon-thinning", "1"])
    assert argv == ["/bin/tt", "overview", "in.parquet", "ov.parquet", "--min-zoom", "0",
                    "--max-zoom", "12", "--force", "--report", "c.json",
                    "--polygon-thinning", "1"]


def test_export_argv_matches_the_stages_path():
    argv = run_matrix.export_argv("/bin/tt", Path("ov.parquet"), Path("o.pmtiles"), "500K", "0",
                                  "merge", Path("r.json"))
    assert argv == ["/bin/tt", "export-pmtiles", "ov.parquet", "o.pmtiles", "--force",
                    "--max-tile-size", "500K", "--max-compressed-tile-size", "0",
                    "--size-valve", "merge", "--report", "r.json"]


# --- scorers ----------------------------------------------------------------


def test_run_scorers_records_done_and_logs(tmp_path):
    ok = (tmp_path / "ok", "k1", "ok job", [sys.executable, "-c", "print('hi')"])
    run.run_scorers([ok], {"score"}, jobs=2)
    assert run.read_cached(tmp_path / "ok" / "done.json", "k1")
    assert (tmp_path / "ok" / "score.log").read_text().strip() == "hi"


def test_run_scorers_stops_on_a_failure_without_marking_it_done(tmp_path):
    bad = (tmp_path / "bad", "k2", "bad job", [sys.executable, "-c", "raise SystemExit(3)"])
    with pytest.raises(SystemExit, match="bad job: exit 3"):
        run.run_scorers([bad], {"score"}, jobs=2)
    assert not (tmp_path / "bad" / "done.json").exists()


def test_run_scorers_refuses_when_the_score_step_is_off(tmp_path):
    job = (tmp_path / "x", "k", "window x", [sys.executable, "-c", "pass"])
    with pytest.raises(SystemExit, match="window x is not scored"):
        run.run_scorers([job], {"report"}, jobs=1)


@pytest.mark.skipif(not Path("/usr/bin/time").exists(), reason="needs GNU time")
def test_run_cell_starts_every_convert_repeat_without_the_previous_overview(tmp_path):
    # A stand-in binary: convert logs whether its output already exists, then
    # writes it; export writes the archive and a minimal report.
    log = tmp_path / "seen.log"
    fake = tmp_path / "fake-tylertoo"
    fake.write_text(f"""#!/bin/sh
if [ "$1" = overview ]; then
  if [ -e "$3" ]; then echo exists >> {log}; else echo missing >> {log}; fi
  echo ov > "$3"
else
  echo pm > "$3"
  for last; do :; done
  echo '{{"zooms": []}}' > "$last"
fi
""")
    fake.chmod(0o755)
    src = tmp_path / "in.parquet"
    src.write_text("x")
    old = run_matrix.LOAD_MAX
    run_matrix.LOAD_MAX = 1e9
    try:
        recs = run_matrix.run_cell(str(fake), src, tmp_path, "c", [], ["merge"], max_zoom=1,
                                   max_tile_size="0", max_compressed_tile_size="0",
                                   wall_s=60, rss_gib=4, repeats=1, convert_repeats=3)
    finally:
        run_matrix.LOAD_MAX = old
    assert log.read_text().split() == ["missing", "missing", "missing"]
    assert [r["status"] for r in recs[0]["convert_runs"]] == ["ok"] * 3
    assert recs[1]["export_run"]["status"] == "ok"


def test_load_gate_scales_with_cores():
    assert run_matrix.default_load_max(64) == pytest.approx(6.4)
    assert run_matrix.default_load_max(8) == 3.0
    assert run_matrix.default_load_max(None) == 3.0


def test_markdown_reports_a_failed_tippecanoe_run():
    rep = _report()
    rep["settings"]["tippecanoe"] = {"label": "tippecanoe", "external": True}
    rep["tippecanoe"] = {"moldova": {"status": "exit 100", "wall_s": 2038.0,
                                     "peak_rss_kib": 3585908, "pmtiles_bytes": None,
                                     "error": "Can't increase feature gap threshold further"}}
    md = run.render_markdown(rep)
    assert "| tippecanoe | failed: exit 100 after 2038 s (Can't increase feature gap " \
           "threshold further) |" in md


# --- review fixes: timing flags ----------------------------------------------


def test_time_cell_flags_on_the_displayed_rounding():
    assert run.time_cell(10.96, 10.0, 0.10) == "10.96 s (+10%)"
    assert run.time_cell(11.04, 10.0, 0.10) == "11.04 s (+10%)"
    assert run.time_cell(11.60, 10.0, 0.10) == "11.60 s (+16%) ▲"


def test_time_cell_floor_widens_to_the_repeat_spread():
    assert run.time_cell(9.0, 8.0, 0.10, spread=0.22) == "9.00 s (+12%)"
    assert run.time_cell(10.0, 8.0, 0.10, spread=0.22) == "10.00 s (+25%) ▲"


def test_time_cell_does_not_flag_seconds_across_hosts():
    assert run.time_cell(16.0, 8.0, 0.10, flags=False) == "16.00 s (+100%)"


def test_rel_spread():
    assert run.rel_spread([125.5, 153.6, 131.1]) == pytest.approx((153.6 - 125.5) / 131.1)
    assert run.rel_spread([1.0]) == 0.0
    assert run.rel_spread([]) == 0.0


def _cell(convert_s, export_s, walls=None, load=1.0):
    walls = walls or [export_s]
    return {"status": "ok",
            "convert": {"median_s": convert_s, "peak_rss_kib": 1048576, "wall_s": [convert_s],
                        "load_before_max": load},
            "export": {"median_s": export_s, "peak_rss_kib": 1048576, "wall_s": walls,
                       "load_before_max": load},
            "total_s": round(convert_s + export_s, 3), "overview_bytes": 1, "pmtiles_bytes": 1,
            "merged_features": 0, "oversized_tiles": 0, "repeats_identical": True}


def test_markdown_flags_the_ratio_across_hosts_but_not_seconds():
    cur, base = _report(), _report()
    base["host"] = {"cpu": "other", "cores": 8, "mem_gib": 16.0}
    cur["cells"]["moldova/default"] = _cell(4.0, 13.0)       # 17 s, 2.62x before-745
    md = run.render_markdown(cur, base)
    assert "different host" in md.lower()
    assert "17.00 s (+100%) |" in md                        # seconds: shown, not flagged
    assert "2.62× (+100%) ▲" in md                          # ratio: flagged


def test_markdown_ignores_a_failed_baseline_cell():
    cur, base = _report(), _report()
    base["cells"]["moldova/default"] = {**base["cells"]["moldova/default"],
                                        "status": "export TIMEOUT", "total_s": None}
    md = run.render_markdown(cur, base)
    assert "| Defaults | 2.00 s | 2.00 GiB | 6.50 s | 1.00 GiB | 8.50 s | 1.31× |" in md


def test_markdown_ignores_a_failed_tippecanoe_baseline():
    cur, base = _report(), _report()
    for r in (cur, base):
        r["settings"]["tippecanoe"] = {"label": "tippecanoe", "external": True}
    cur["tippecanoe"] = {"moldova": {"wall_s": 100.0, "peak_rss_kib": 1048576,
                                     "pmtiles_bytes": 10}}
    base["tippecanoe"] = {"moldova": {"status": "TIMEOUT", "wall_s": 10800.0,
                                      "peak_rss_kib": 1, "pmtiles_bytes": None}}
    md = run.render_markdown(cur, base)
    assert "100.00 s |" in md
    assert "−99%" not in md


def test_markdown_marks_loaded_seeded_and_unrepeatable_cells():
    rep = _report()
    rep["load_max"] = 6.4
    rep["cells"]["moldova/default"] = {**_cell(2.0, 6.5, load=7.0), "repeats_identical": False}
    rep["settings"]["tippecanoe"] = {"label": "tippecanoe", "external": True}
    rep["tippecanoe"] = {"moldova": {"wall_s": 1900.0, "peak_rss_kib": 1048576,
                                     "pmtiles_bytes": 10, "seeded_from": "the #745 run"}}
    md = run.render_markdown(rep)
    assert "| Defaults † ‡ |" in md
    assert "† The load average was at or above 6.4" in md
    assert "‡ Repeats wrote different bytes" in md
    assert "seeded from the #745 run" in md


def test_markdown_renders_coverage_and_metrics_sections():
    rep = _report()
    rep["coverage"] = {"mg-z8": {"label": "Madagascar z8", "zoom": 8, "tiles": {},
                                 "settings": {"default": {"missing": 0.0023, "added": 0.00002,
                                                          "features": 7782}}}}
    rep["metrics"] = {"moldova": {"label": "FTW Moldova", "zooms": [6], "named": ["6/1/2"],
                                  "settings": {"default": {
                                      "zooms": {"6": {"median": 0.789, "min": 0.786,
                                                      "edge_gap": 0.2, "excess_change": None}},
                                      "named": {"6/1/2": 0.5}}}}}
    md = run.render_markdown(rep)
    assert "| Madagascar z8 | Defaults | 0.23% | 0.002% | 7,782 |" in md
    assert "| Defaults | 78.9% (78.6%) | 50.0% | z6 0.20 |  |" in md


# --- review fixes: caches and failures ---------------------------------------


def test_cell_key_includes_the_harness():
    base = dict(binary_sha="b", input_sha="i", convert=[], valve="merge", max_zoom=12,
                max_tile_size=1, max_compressed_tile_size=0, repeats=3, harness="h1")
    assert run.cell_key(**base) != run.cell_key(**{**base, "harness": "h2"})


def test_tippecanoe_key_includes_the_gpio_pin():
    assert run.tippecanoe_key("s", "i", ["-z12"], "geoparquet-io==1.7.0") != \
        run.tippecanoe_key("s", "i", ["-z12"], "geoparquet-io==1.8.0")


def test_tippecanoe_pin_comes_from_the_setup_script():
    assert run.tippecanoe_pin() == "68ab8dcc229f95b8b25877697d5e8d66783af503"


def test_tippecanoe_failure_from_gnu_time_stderr():
    stderr = ("Can't increase feature gap threshold further\n"
              "Command exited with non-zero status 100\n"
              "\tCommand being timed: \"tippecanoe -o x\"\n\tUser time (seconds): 1.0\n")
    assert run.tippecanoe_failure({"status": "FAILED", "stderr_tail": stderr}) == \
        ("exit 100", "Can't increase feature gap threshold further")
    assert run.tippecanoe_failure({"status": "TIMEOUT", "stderr_tail": ""}) == ("TIMEOUT", "")


def test_cached_cell_rules(tmp_path):
    pm = tmp_path / "c.merge.pmtiles"
    pm.write_bytes(b"x" * 10)
    st = pm.stat()
    summary = tmp_path / "c.json"

    def write(**kw):
        summary.write_text(json.dumps({"key": "k", **kw}))

    write(status="ok", pmtiles_bytes=10, pmtiles_mtime_ns=st.st_mtime_ns)
    assert run.cached_cell(summary, "k", pm)
    assert run.cached_cell(summary, "other", pm) is None
    write(status="ok", pmtiles_bytes=11, pmtiles_mtime_ns=st.st_mtime_ns)
    assert run.cached_cell(summary, "k", pm) is None          # archive changed
    pm.unlink()
    write(status="convert FAILED")
    assert run.cached_cell(summary, "k", pm)                  # a failure is a result
    write(status="export TIMEOUT")
    assert run.cached_cell(summary, "k", pm) is None          # a cap is retried


def test_window_missing_a_key_is_an_error(cfg):
    bad = copy.deepcopy(cfg)
    del bad["windows"][0]["report_zooms"]
    with pytest.raises(run.ConfigError, match="report_zooms"):
        run.validate_config(bad)


def test_load_baseline_fails_early_and_tolerates_missing_sections(tmp_path):
    with pytest.raises(SystemExit, match="--compare"):
        run.load_baseline(tmp_path / "missing.json")
    p = tmp_path / "b.json"
    p.write_text("{}")
    b = run.load_baseline(p)
    assert b["binary"] == {} and b["host"] == {}
    assert run.render_markdown(_report(), b)


def test_verify_input_rejects_a_missing_or_changed_file(tmp_path):
    f = tmp_path / "a.bin"
    with pytest.raises(SystemExit, match="missing"):
        run.verify_input(f, "0" * 64)
    f.write_bytes(b"hello")
    with pytest.raises(SystemExit, match="sha256"):
        run.verify_input(f, "0" * 64)
    run.verify_input(f, _sha(b"hello"))
