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
