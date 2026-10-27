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
        {"export": {"source_losses": [{"id": i, "zoom": 10, "cause": "valve_dropped"} for i in range(written, written + dropped)]}},
        (500_000, 500_000),
        True,
        False,
        (100, []),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok is ok, accounting.detail

@pytest.mark.parametrize("losses,ok", [
    ([{"id": 1, "zoom": 10, "cause": "encode_dropped"}], True),
    ([{"id": 0, "zoom": 10, "cause": "encode_dropped"}], False),
    ([{"id": 1, "zoom": 9, "cause": "encode_dropped"}], False),
    ([{"id": 2, "zoom": 10, "cause": "encode_dropped"}], False),
    ([{"id": 1, "zoom": 10, "cause": "encode_dropped"}] * 2, False),
])
def test_feature_accounting_uses_disjoint_source_losses(losses, ok):
    outcomes = evaluate(
        {10: ZoomScan(ids={0})}, {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"convert": {"source_losses": []}, "export": {"source_losses": losses}},
        (500_000, 500_000), True, False, (2, []),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok is ok, accounting.detail
