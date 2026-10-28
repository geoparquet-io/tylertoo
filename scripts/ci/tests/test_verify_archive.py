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

@pytest.mark.parametrize("scanned", [None, [[1, 3]]])
def test_accounting_maps_original_rows_to_nonsequential_feature_ids(scanned):
    convert = {"input_rows": 3 if scanned is None else 2,
               "source_losses": [{"id": 1, "zoom": None, "cause": "missing_geometry"}]}
    if scanned is not None:
        convert["source_row_ranges"] = scanned
    outcomes = evaluate(
        {10: ZoomScan(ids={100, 300} if scanned is None else {300})},
        {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"convert": convert, "export": {"source_losses": []}},
        (500_000, 500_000), True, False, (3, [], [100, 200, 300]),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok, accounting.detail


def test_accounting_uses_export_feature_id_instead_of_band_row():
    outcomes = evaluate(
        {10: ZoomScan(ids={100})},
        {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"export": {"source_losses": [{"id": 0, "feature_id": 200, "zoom": 10,
                                          "cause": "quantized"}]}},
        (500_000, 500_000), True, False, (2, [], [100, 200]),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok, accounting.detail
