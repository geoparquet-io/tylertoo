"""Diagnostics belong to one conversion, including its warnings."""

import logging
from pathlib import Path

import tylertoo

FIXTURES = (
    Path(__file__).resolve().parents[2] / "core" / "tests" / "fixtures" / "invariants"
)


def test_second_conversion_warns_again_with_its_own_counts(tmp_path, caplog):
    """A repeated conversion must neither inherit counts nor suppress warnings."""
    caplog.set_level(logging.WARNING)
    for index in range(2):
        caplog.clear()
        report = tylertoo.overview(
            str(FIXTURES / "nonfinite-vertex.parquet"),
            str(tmp_path / f"overview-{index}.parquet"),
            max_zoom=4,
        )
        assert (
            report["diagnostics"].count({"kind": "non_finite_geometry", "count": 2})
            == 1
        )
        assert len(report["source_losses"]) == 2
        assert "NaN or infinite" in caplog.text
