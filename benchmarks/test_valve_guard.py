"""Tests for valve_guard.py's verdict: the ratio half always applies, the
baseline half only on the host class it was recorded on (#810).

    uv run --no-project --python 3.12 --with pytest pytest benchmarks/test_valve_guard.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import valve_guard as vg  # noqa: E402

GH = {"machine": "x86_64", "cpu_model": "AMD EPYC 7763 64-Core Processor", "cpus": 4}
VM = {"machine": "aarch64", "cpu_model": "0xd40", "cpus": 64}


def _cell(wall: float, rss: int = 1000) -> dict:
    return {"wall_s": wall, "peak_rss_kib": rss, "peak_rss_min_kib": rss, "rounds": 3}


BASE = {"792-def": _cell(10.0), "792-drop": _cell(7.0)}


def _baseline() -> dict:
    return {"hosts": {vg.host_class_key(VM): {"host": VM, "cells": BASE}}}


def test_a_30pct_slower_cell_fails_the_baseline_on_its_own_host():
    v = vg.verdict({"792-def": _cell(13.0), "792-drop": _cell(7.5)}, _baseline(), VM)
    assert v["ratio"]["status"] == "pass"
    assert v["baseline"]["status"] == "fail"
    assert any("792-def wall" in f for f in v["baseline"]["failures"])


def test_another_host_class_says_baseline_not_applicable():
    v = vg.verdict({"792-def": _cell(13.0), "792-drop": _cell(7.5)}, _baseline(), GH)
    assert v["ratio"]["status"] == "pass"
    assert v["baseline"]["status"] == "not applicable"
    assert vg.host_class_key(VM) in v["baseline"]["reason"]


def test_the_ratio_half_fires_anywhere():
    v = vg.verdict({"792-def": _cell(20.0), "792-drop": _cell(7.0)}, _baseline(), GH)
    assert v["ratio"]["status"] == "fail"
    assert vg.exit_status(v) == 1


def test_not_applicable_is_not_a_failure_but_is_not_a_pass_either():
    v = vg.verdict({"792-def": _cell(10.0), "792-drop": _cell(7.0)}, _baseline(), GH)
    assert vg.exit_status(v) == 0
    assert "baseline: NOT APPLICABLE" in vg.summary_lines(v)[1]


def test_no_baseline_file_is_not_applicable():
    v = vg.verdict({"792-def": _cell(10.0), "792-drop": _cell(7.0)}, None, VM)
    assert v["baseline"]["status"] == "not applicable"


def test_record_keeps_other_host_classes():
    doc = vg.with_record(_baseline(), {"host": GH, "cells": BASE})
    assert set(doc["hosts"]) == {vg.host_class_key(VM), vg.host_class_key(GH)}


def test_the_committed_baseline_is_keyed_by_host_class():
    doc = json.loads(vg.BASELINE.read_text())
    for key, rec in doc["hosts"].items():
        assert key == vg.host_class_key(rec["host"])


# --- effective-cores gate (#817) ---------------------------------------------
PASS2, LEVELS = "convert/pass2 (output sink)", "export/levels"
FLOORS = {"whole_cores_frac": 0.25, "serial_share_max": 0.10}  # 16 of 64 cores


def _cores_cell(pass2=40.0, levels=40.0, whole=30.0, share=0.05) -> dict:
    return {**_cell(10.0), "effective_cores": whole, "serial_share": share,
            "phase_cores": {PASS2: pass2, LEVELS: levels}}


def _cores_baseline(floors=FLOORS) -> dict:
    rec = {"host": VM, "cells": {"moldova-tiles": _cores_cell()}}
    if floors:
        rec["floors"] = floors
    return {"hosts": {vg.host_class_key(VM): rec}}


def _check(cell: dict, host=VM, exempt=frozenset(), floors=FLOORS) -> dict:
    return vg.verdict({"moldova-tiles": cell}, _cores_baseline(floors), host, exempt)


def test_a_20pct_cores_drop_fails_and_10pct_passes():
    bad = _check(_cores_cell(levels=32.0))
    assert bad["baseline"]["status"] == "fail"
    assert any("export/levels" in f for f in bad["baseline"]["failures"])
    assert _check(_cores_cell(pass2=36.0, levels=36.0))["baseline"]["status"] == "pass"


def test_cores_drop_is_not_applicable_on_another_class_and_says_so():
    v = _check(_cores_cell(levels=1.0), host=GH)
    assert v["baseline"]["status"] == "not applicable" and vg.exit_status(v) == 0
    assert v["floors"]["status"] == "not applicable"
    assert "floors: NOT APPLICABLE" in vg.summary_lines(v)[2]


def test_whole_run_cores_below_the_floor_fails():
    v = _check(_cores_cell(whole=15.0))
    assert v["floors"]["status"] == "fail" and vg.exit_status(v) == 1
    assert _check(_cores_cell(whole=16.0))["floors"]["status"] == "pass"


def test_serial_share_above_the_floor_fails():
    v = _check(_cores_cell(share=0.11))
    assert v["floors"]["status"] == "fail"
    assert _check(_cores_cell(share=0.10))["floors"]["status"] == "pass"


def test_an_exemption_suppresses_the_floor_for_that_cell_only():
    both = {"moldova-tiles": _cores_cell(whole=2.0), "792-def": _cores_cell(whole=2.0)}
    v = vg.verdict(both, _cores_baseline(), VM, frozenset({"moldova-tiles"}))
    assert [f for f in v["floors"]["failures"] if "792-def" in f]
    assert not [f for f in v["floors"]["failures"] if "moldova-tiles" in f]


def test_no_floors_section_means_floors_not_applicable():
    assert _check(_cores_cell(whole=1.0), floors=None)["floors"]["status"] == "not applicable"


def test_serialized_levels_fail_the_gate():
    v = _check(_cores_cell(levels=1.0, whole=1.2, share=0.6))
    assert vg.exit_status(v) == 1
    assert v["baseline"]["status"] == "fail" and v["floors"]["status"] == "fail"


def test_cell_summary_takes_medians_of_whole_phase_and_serial_share():
    def run(c, ph, ss):
        prof = {"convert": {"phases": {"pass2 (output sink)": {"effective_cores": ph}}},
                "export": {"phases": {"levels": {"effective_cores": ph}}, "serial_share": ss}}
        return {**_cell(1.0), "wall_s": 1.0, "peak_rss_kib": 5, "effective_cores": c, "profile": prof}
    out = vg.cell_summary([run(10, 8, 0.1), run(20, 9, 0.3), run(30, 10, 0.2)])
    assert out["effective_cores"] == 20 and out["serial_share"] == 0.2  # noqa: PLR2004
    assert out["phase_cores"] == {PASS2: 9, LEVELS: 9}


def test_exemptions_file_is_read_by_cell(tmp_path):
    f = tmp_path / "e.toml"
    f.write_text('[cores_floor.moldova-tiles]\nreason = "r"\nissue = 813\n[noisy."x/*"]\nreason = "r"\n')
    assert vg.load_exemptions(f) == {"moldova-tiles"}


def test_record_keeps_the_classes_floors():
    doc = vg.with_record(_cores_baseline(), {"host": VM, "cells": BASE})
    assert doc["hosts"][vg.host_class_key(VM)]["floors"] == FLOORS
