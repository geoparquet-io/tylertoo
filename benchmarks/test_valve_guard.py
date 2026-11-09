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


def _cores_cell(pass2=40.0, levels=40.0, whole=30.0, share=0.05, speed=3.0, cpu=100.0, sha="aa") -> dict:
    return {**_cell(10.0), "effective_cores": whole, "serial_share": share,
            "phase_cores": {PASS2: pass2, LEVELS: levels},
            "speedup_16_to_full": {PASS2: speed, LEVELS: speed, "whole": speed},
            "phase_cpu_s": {PASS2: cpu, LEVELS: cpu}, "output_sha256": sha,
            "phase_rounds": {PASS2: 3, LEVELS: 3}, "serial_share_rounds": 3,
            "low_phase_rounds": {PASS2: 3, LEVELS: 3}, "low_rounds": 3}


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


# --- scaling and core-seconds checks (#817 follow-up) --------------------------
def _scaled(speed=3.0, cpu=100.0, sha="aa", **kw) -> dict:
    return _cores_cell(speed=speed, cpu=cpu, sha=sha, **kw)


def _scaled_check(cell: dict, host=VM) -> dict:
    base = {"hosts": {vg.host_class_key(VM): {"host": VM, "cells": {"moldova-tiles": _scaled()}}}}
    return vg.verdict({"moldova-tiles": cell}, base, host)


def test_a_20pct_speedup_drop_fails_and_10pct_passes():
    bad = _scaled_check(_scaled(speed=2.4))
    assert bad["baseline"]["status"] == "fail"
    assert any("speedup" in f for f in bad["baseline"]["failures"])
    assert _scaled_check(_scaled(speed=2.7))["baseline"]["status"] == "pass"


def test_cpu_core_seconds_up_30pct_fails_and_20pct_passes():
    bad = _scaled_check(_scaled(cpu=130.0))
    assert any("core-seconds" in f for f in bad["baseline"]["failures"])
    assert _scaled_check(_scaled(cpu=120.0))["baseline"]["status"] == "pass"


def test_core_seconds_are_not_compared_across_different_outputs():
    v = _scaled_check(_scaled(cpu=300.0, sha="bb"))
    assert v["baseline"]["status"] == "pass"


def test_scaling_checks_are_not_applicable_on_another_class():
    v = _scaled_check(_scaled(speed=0.5, cpu=900.0), host=GH)
    assert v["baseline"]["status"] == "not applicable" and vg.exit_status(v) == 0


def test_cell_summary_derives_speedup_and_core_seconds():
    def run(wall, p2, lv):
        ph = lambda w, c: {"wall_secs": w, "cpu_user_secs": c - 1, "cpu_sys_secs": 1, "effective_cores": c / w}  # noqa: E731
        prof = {"convert": {"phases": {"pass2 (output sink)": ph(p2, 50)}},
                "export": {"phases": {"levels": ph(lv, 40)}}}
        return {"wall_s": wall, "peak_rss_kib": 5, "effective_cores": 3, "profile": prof,
                "output_sha256": "aa"}
    out = vg.cell_summary([run(10, 4, 5)], [run(20, 12, 10)])
    assert out["speedup_16_to_full"] == {"whole": 2.0, PASS2: 3.0, LEVELS: 2.0}
    assert out["phase_cpu_s"] == {PASS2: 50.0, LEVELS: 40.0}
    assert out["output_sha256"] == "aa"


# --- review fixes (#817) ----------------------------------------------------------
def _vs(cell: dict, base_cell: dict | None = None, floors=FLOORS) -> dict:
    rec = {"host": VM, "cells": {"moldova-tiles": base_cell or _cores_cell()}}
    if floors:
        rec["floors"] = floors
    return vg.verdict({"moldova-tiles": cell}, {"hosts": {vg.host_class_key(VM): rec}}, VM)


def test_a_baseline_without_the_new_fields_is_not_applicable_not_pass():
    v = _vs(_cores_cell(), base_cell=_cell(10.0))
    assert v["baseline"]["status"] == "not applicable"
    assert "run record" in v["baseline"]["reason"] and vg.exit_status(v) == 0
    assert _vs(_cell(20.0), base_cell=_cell(10.0))["baseline"]["status"] == "fail"  # wall still gates


def test_missing_phase_telemetry_fails():
    gone = {**_cores_cell(), "phase_cores": {}, "phase_cpu_s": {}, "speedup_16_to_full": {"whole": 3.0},
            "phase_rounds": {}}
    v = _vs(gone)
    assert v["baseline"]["status"] == "fail"
    assert any("telemetry missing" in f for f in v["baseline"]["failures"])


def test_phase_telemetry_in_only_some_rounds_fails():
    part = {**_cores_cell(), "phase_rounds": {PASS2: 3, LEVELS: 1}}
    v = _vs(part)
    assert any("telemetry missing in 2 of 3 rounds" in f for f in v["baseline"]["failures"])


def test_missing_serial_share_fails_the_floor():
    v = _vs({**_cores_cell(), "serial_share": None, "serial_share_rounds": 0})
    assert v["floors"]["status"] == "fail"
    assert "serial share" in v["floors"]["failures"][0]
    part = _vs({**_cores_cell(), "serial_share_rounds": 2})
    assert part["floors"]["status"] == "fail"


def test_a_zero_cpu_baseline_is_skipped_with_a_reason_not_by_truthiness():
    base = _cores_cell(cpu=0.0)
    v = _vs(_cores_cell(cpu=50.0), base_cell=base)
    assert v["baseline"]["status"] == "pass" and v["baseline"]["skipped"]
    # a real baseline of 0.5 core-s is compared
    v = _vs(_cores_cell(cpu=0.7), base_cell=_cores_cell(cpu=0.5))
    assert any("core-seconds" in f for f in v["baseline"]["failures"])


def test_cell_summary_keeps_precision_and_counts_rounds():
    def run(ph):
        prof = {"convert": {"phases": {"pass2 (output sink)": ph} if ph else {}}}
        return {"wall_s": 1.0, "peak_rss_kib": 5, "effective_cores": 3, "profile": prof}
    tiny = {"wall_secs": 0.0004, "cpu_user_secs": 0.0003, "cpu_sys_secs": 0.0, "effective_cores": 0.7}
    out = vg.cell_summary([run(tiny), run(tiny), run(None)])
    assert out["phase_cpu_s"][PASS2] == 0.0003  # noqa: PLR2004
    assert out["phase_rounds"][PASS2] == 2  # noqa: PLR2004 and rounds == 3


def test_record_merges_per_cell_and_keeps_floors():
    old = _cores_baseline()
    new = vg.with_record(old, {"host": VM, "cells": {"792-def": _cell(9.0)}})
    entry = new["hosts"][vg.host_class_key(VM)]
    assert set(entry["cells"]) == {"moldova-tiles", "792-def"} and entry["floors"] == FLOORS


def test_thread_env_is_explicit_per_arm():
    env = {"PATH": "/bin", "RAYON_NUM_THREADS": "16"}
    assert "RAYON_NUM_THREADS" not in vg.run_env(env, None, "p")
    assert vg.run_env(env, 16, "p")["RAYON_NUM_THREADS"] == "16"
    assert vg.run_env(env, 1, "p")["TYLERTOO_PROFILE_JSON"] == "p"


def test_record_and_untagged_check_refuse_a_parent_thread_limit():
    for mode, inject in (("record", None), ("check", None), ("record", 1)):
        try:
            vg.refuse_thread_env(mode, inject, {"RAYON_NUM_THREADS": "16"})
        except SystemExit:
            pass
        else:
            raise AssertionError((mode, inject))
    vg.refuse_thread_env("check", 1, {})  # --inject-threads on check is the supported path


def test_an_injected_run_is_labelled():
    v = vg.verdict({"moldova-tiles": _cores_cell()}, None, VM, injected_threads=1)
    assert v["injected_threads"] == 1
    assert "INJECTED" in vg.summary_lines(v)[0]


# --- roborev fixes (#817) -------------------------------------------------------
def test_a_baseline_missing_a_required_phase_is_stale_not_pass():
    # a recording whose 16-thread runs lacked the convert line has no pass-2 speedup
    holed = {**_cores_cell(), "speedup_16_to_full": {LEVELS: 3.0, "whole": 3.0},
             "low_phase_rounds": {LEVELS: 3}}
    v = _vs(_cores_cell(), base_cell=holed)
    assert v["baseline"]["status"] == "not applicable" and "run record" in v["baseline"]["reason"]


def test_current_run_missing_a_required_low_thread_phase_fails():
    holed = {**_cores_cell(), "low_phase_rounds": {LEVELS: 3}}
    v = _vs(holed)
    assert any(PASS2 in f and "16-thread" in f for f in v["baseline"]["failures"])


def test_export_cells_require_only_the_levels_phase():
    cell = {**_cores_cell(), "phase_rounds": {LEVELS: 3}}
    assert vg.missing_telemetry("792-def", cell) == []
    assert vg.missing_telemetry("moldova-tiles", cell)


def test_record_refuses_an_incomplete_measurement():
    assert vg.record_problems({"moldova-tiles": _cores_cell()}) == []
    assert vg.record_problems({"moldova-tiles": {**_cores_cell(), "phase_rounds": {PASS2: 3}}})
    assert vg.record_problems({"moldova-tiles": {**_cores_cell(), "serial_share_rounds": 2}})


def test_skipped_checks_are_printed_and_a_changed_output_says_so():
    v = _vs(_cores_cell(cpu=900.0, sha="bb"))
    assert v["baseline"]["status"] == "pass"
    lines = vg.report_lines(v)
    assert any(line.startswith("SKIP ") and "output differs" in line for line in lines)
    assert lines[-3].startswith("ratio:")
