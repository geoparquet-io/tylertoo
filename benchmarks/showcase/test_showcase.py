"""
Tests for showcase.py (projection, region and provenance logic) and the
tippecanoe shim (#810, track D).

    uv run --no-project --with pytest pytest benchmarks/showcase/test_showcase.py
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(ROOT / "scripts"))

import gen_bench_docs as g
import showcase as sc

sys.path.insert(0, str(HERE.parent))
import run_manifest as rm

CALIB = {"est_rows": 1_000_000, "tylertoo_s3_s": 60.0, "tylertoo_s3_disk": 1e9,
         "tylertoo_local_s": 50.0, "tylertoo_local_disk": 0.5e9, "extract_s": 30.0,
         "extract_bytes": 0.25e9, "convert_s": 40.0, "tile_s": 130.0, "tippecanoe_disk": 1e9}


def regs(**rows: int) -> dict:
    return {n: {"bbox": (0, 0, 1, 1), "est_rows": r, "est_bytes": 0} for n, r in rows.items()}


def proj(points, **rows):
    return {p["region"]: p for p in sc.project(points, regs(**rows))["regions"]}


# --- projection -------------------------------------------------------------


def test_one_point_scales_linearly_with_each_tools_margin():
    p = proj([CALIB], ten=10_000_000)["ten"]
    assert p["tylertoo_s3_s"] == pytest.approx(60 * 10 * 1.2)
    assert p["tylertoo_local_s"] == pytest.approx(50 * 10 * 1.2)
    assert p["tippecanoe_e2e_s"] == pytest.approx((30 + 40 + 130) * 10 * 1.3)
    # tylertoo's disk is the larger of the s3:// run and extract + local run
    assert p["tylertoo_disk"] == pytest.approx(1e9 * 10 * 1.2)
    assert p["tippecanoe_disk"] == pytest.approx((0.25e9 + 1e9) * 10 * 1.3)


def test_two_points_keep_the_fixed_cost_out_of_the_slope():
    # The smoke bbox costs 20 s of extract (the footer scan) for nothing;
    # the calibration bbox 30 s: 10 s per million on top of the fixed 20.
    small = {**{k: 0.0 for k in sc.METRICS}, "est_rows": 1, "extract_s": 20.0}
    p = proj([small, CALIB], ten=10_000_000)["ten"]
    assert p["tippecanoe_e2e_s"] == pytest.approx((20 + 10 * 10 + (40 + 130) * 10) * 1.3, rel=1e-6)


def test_a_falling_metric_never_projects_below_the_larger_point():
    small = {**CALIB, "est_rows": 1, "tile_s": 500.0}
    assert sc.at([small, CALIB], "tile_s", 10_000_000) == pytest.approx(130.0)


def test_recommends_the_largest_region_inside_every_limit():
    # tippecanoe e2e is 200 s x 1.3 per million: 20M -> 86.7 min fits, 25M -> 108 min does not
    pj = sc.project([CALIB], regs(small=5_000_000, mid=20_000_000, big=25_000_000))
    assert pj["recommended"] == "mid"
    assert [p["fits"] for p in pj["regions"]] == [True, True, False]


def test_disk_limit_alone_rules_a_region_out():
    calib = {**CALIB, "tippecanoe_disk": 10e9}  # (0.25 + 10) GB x 1.3 per million: 4M is 53 GB
    pj = sc.project([calib], regs(two=2_000_000, four=4_000_000))
    assert pj["recommended"] == "two"
    assert [p["fits"] for p in pj["regions"]] == [True, False]


def test_tylertoo_time_limit_rules_a_region_out():
    calib = {**CALIB, "tylertoo_s3_s": 400.0}  # 400 s x 1.2 per million: 8M is 64 min
    assert sc.project([calib], regs(seven=7_000_000, eight=8_000_000))["recommended"] == "seven"


def test_smoke_and_calibration_regions_are_never_recommended():
    pj = sc.project([CALIB], regs(smoke=1_000, **{sc.CALIBRATION: 1_000_000}))
    assert pj["recommended"] is None
    assert "recommended: None" in sc.format_projection(pj)


# --- regions and parts --------------------------------------------------------


def test_overlap_fraction_and_intersection():
    assert sc.overlap_fraction((0, 0, 2, 2), (1, 1, 3, 3)) == pytest.approx(0.25)
    assert sc.overlap_fraction((0, 0, 2, 2), (2.5, 0, 3, 1)) == 0.0
    assert sc.overlap_fraction((1, 1, 1, 1), (0, 0, 2, 2)) == 1.0  # a degenerate row group inside
    assert sc.intersects((0, 0, 1, 1), (1, 1, 2, 2))  # touching edges count
    assert sc.intersects((1, 1, 2, 2), (0, 0, 1, 1))


def test_parts_touching_picks_the_intersecting_parts():
    parts = [{"url": "a", "bbox": (0, 0, 1, 1)}, {"url": "b", "bbox": (5, 5, 6, 6)}]
    assert [p["url"] for p in sc.parts_touching((0.5, 0.5, 2, 2), parts)] == ["a"]


def test_committed_tables_are_complete():
    parts = sc.read_parts()
    assert len(parts) == 512
    assert sum(p["rows"] for p in parts) == 2_533_842_612
    assert sum(p["bytes"] for p in parts) == 276_943_764_116
    regions = sc.read_regions()
    assert [n for n, _ in sc.CANDIDATES] == list(regions)
    assert 1_000_000 <= regions[sc.CALIBRATION]["est_rows"] <= 3_000_000


# --- manifest -----------------------------------------------------------------


def test_rendered_manifest_loads_and_pins_the_region(tmp_path):
    pin = {"file": "x.parquet", "sha256": "a" * 64, "bytes": 10}
    path = tmp_path / "m.toml"
    path.write_text(sc.render_manifest("texas", (-106.65, 25.84, -93.51, 36.5), pin))
    m = rm.load_manifest(path)
    assert m["inputs"]["overture-texas"]["sha256"] == "a" * 64
    s3 = m["cells"]["ovt-s3"]
    argv = rm.build_argv(s3, {"input": "I", "files_from": "F", "out": "O"})
    assert argv[1] == sc.PREFIX and "--bbox=-106.65,25.84,-93.51,36.5" in argv
    assert s3["env"]["AWS_SKIP_SIGNATURE"] == "true"
    local = rm.build_argv(m["cells"]["ovt-local"], {"input": "I", "files_from": "F", "out": "O"})
    assert local[1] == "I" and not any(a.startswith("--bbox") for a in local)


def test_committed_manifest_loads():
    m = rm.load_manifest(sc.MANIFEST)
    assert set(m["cells"]) == {"ovt-s3", "ovt-local"}
    assert m["inputs"]["overture-listing"]["sha256"] == rm.sha256_file(sc.PARTS_TSV)


# --- provenance ---------------------------------------------------------------


def run(wall: float, rss_mib: float = 100, fetched: int | None = None, shim: dict | None = None,
        rows: int = 1050) -> dict:
    return {"status": "ok", "gate": {}, "wall_s": wall, "cpu_s": wall * 32, "peak_rss_kib": rss_mib * 1024,
            "scratch_peak_mib": 10, "output": {"bytes": 1000, "sha256": "f" * 64},
            "profile": {"convert": {"pass1_rows": rows, "true_peak_mib": rss_mib}},
            "bytes_fetched": fetched, "shim": shim}


def doc(**kw) -> dict:
    parts = [{"url": "s3://b/p0", "etag": "e0", "bytes": 5, "bbox": (0, 0, 1, 1), "rows": 1},
             {"url": "s3://b/p1", "etag": "e1", "bytes": 7, "bbox": (9, 9, 10, 10), "rows": 1}]
    prov = {k: None for k in g.PROVENANCE_KEYS} | {"tool_versions": {}, "inputs": []}
    tc = [run(300, shim={"fgb_convert_s": 40.0, "tippecanoe_s": 250.0, "fgb_bytes": 9})]
    return sc.build_doc("r", (0, 0, 2, 2), {"wall_s": 20.0, "argv": ["gpio", "extract"], "peak_rss_kib": 1,
                                            "scratch_peak_mib": 1},
                        {"file": "x.parquet", "sha256": "a" * 64, "bytes": 1234},
                        [run(100, fetched=500)], [run(50), run(52), run(54)], tc, parts, prov, **kw)


def test_doc_rows_split_tippecanoe_into_its_steps():
    d = doc()
    s3, local, tc = d["rows"]
    assert s3["fetched_bytes"] == 500 and s3["conversion_wall_s"] is None
    assert local["runs_wall_s"] == [50, 52, 54] and local["conversion_wall_s"] == 20.0
    assert tc["runs_wall_s"] == [250.0]  # the tile step, not the shim's whole wall
    assert tc["conversion_wall_s"] == pytest.approx(60.0)  # extract + FlatGeobuf
    assert tc["steps"] == {"extract_s": 20.0, "fgb_convert_s": 40.0, "tile_s": 250.0}
    assert local["effective_cores"] == 32.0 and local["peak_rss_bytes"] == 100 << 20


def test_doc_provenance_names_bbox_and_touched_part_etags():
    d = doc()
    g.check_provenance("x", d)
    ovt, ext = d["provenance"]["inputs"]
    assert ovt["bbox"] == [0, 0, 2, 2] and ovt["release"] == sc.RELEASE
    assert ovt["parts"] == [{"url": "s3://b/p0", "etag": "e0", "bytes": 5}]
    assert ext["sha256"] == "a" * 64 and d["parts_bytes"] == 5 and d["features"] == 1050


def test_doc_renders_with_the_head_to_head_renderer():
    out = g.vida_head_to_head(doc())
    assert "| tylertoo, local extract | 72 s |" in out  # median 52 + extract 20
    assert "| tippecanoe v2.79.0 | 310 s |" in out  # tile 250 + extract 20 + convert 40


def test_profile_and_shim_parsing(tmp_path):
    prof = tmp_path / "p.jsonl"
    prof.write_text(json.dumps({"phase": "convert", "pass0": {"bytes_fetched": 42}}) + "\n"
                    + json.dumps({"phase": "export"}) + "\n")
    assert sc.bytes_fetched(prof) == 42
    assert sc.bytes_fetched(tmp_path / "none") is None
    log = tmp_path / "x.log"
    log.write_text("noise\nshowcase: fgb_convert_s=1.5 tippecanoe_s=2.25 fgb_bytes=7\n--- stderr ---\n")
    assert sc.shim_phases(log) == {"fgb_convert_s": 1.5, "tippecanoe_s": 2.25, "fgb_bytes": 7.0}


# --- the shim -----------------------------------------------------------------


def test_shim_translates_tylertoo_argv_and_reports_steps(tmp_path):
    gpio = tmp_path / "gpio"
    gpio.write_text('#!/bin/sh\necho "$@" > "$(dirname "$0")/gpio.args"\nprintf 12345 > "$4"\n')
    tip = tmp_path / "tippecanoe"
    tip.write_text('#!/bin/sh\necho "$@" > "$(dirname "$0")/tc.args"\nprintf x > "$2"\n')
    for f in (gpio, tip):
        f.chmod(0o755)
    (tmp_path / "tmp").mkdir()
    env = {**os.environ, "TMPDIR": str(tmp_path / "tmp"), "GPIO": str(gpio), "TIPPECANOE_BIN": str(tip),
           "TIPPECANOE_FLAGS": "--simplification=10", "SHOWCASE_KEEP": str(tmp_path / "keep")}
    out = subprocess.run([str(sc.SHIM), "tiles", "in.parquet", str(tmp_path / "o.pmtiles"),
                          "--min-zoom", "2", "--max-zoom", "9", "--layer-name", "b", "--tile-buffer", "8",
                          "--max-compressed-tile-size", "500000", "--max-tile-size", "0",
                          "--include-property", "class"], env=env, capture_output=True, text=True, check=True)
    assert (tmp_path / "gpio.args").read_text().split()[:3] == ["convert", "flatgeobuf", "in.parquet"]
    tc = (tmp_path / "tc.args").read_text().split()
    assert tc[tc.index("-Z") + 1] == "2" and tc[tc.index("-z") + 1] == "9"
    assert tc[tc.index("-l") + 1] == "b" and tc[tc.index("-b") + 1] == "8"
    assert tc[tc.index("--maximum-tile-bytes") + 1] == "500000"
    assert "--simplification=10" in tc and "class" not in tc
    line = next(ln for ln in out.stdout.splitlines() if ln.startswith("showcase:"))
    assert "fgb_bytes=5" in line
    assert (tmp_path / "keep" / "tippecanoe.pmtiles").exists()
    assert not (tmp_path / "tmp" / "input.fgb").exists()
