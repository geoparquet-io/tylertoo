"""Source-feature conservation in the independent archive verifier."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from verify_archive import ZoomScan, evaluate


@pytest.mark.parametrize(
    ("written", "dropped", "ok"),
    [(1, 100, False), (1, 99, True), (1, 98, False), (101, 0, False)],
)
def test_feature_accounting_requires_exact_partition(written, dropped, ok):
    """Reject unexplained losses and input overcounts."""
    outcomes = evaluate(
        {10: ZoomScan(ids=set(range(written)))},
        {"vector_layers": [{"id": "test", "fields": {}}]},
        "test",
        {"export": {"zooms": [{"zoom": 10, "valve_dropped_features": dropped}]}},
        (500_000, 500_000),
        True,
        False,
        (100, []),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok is ok, accounting.detail
