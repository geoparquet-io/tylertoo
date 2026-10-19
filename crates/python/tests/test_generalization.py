"""overview(verbatim=...) and overview(entry_zoom=...) (#445).

The fixture is 1000 small building polygons, which the default
visibility and thinning gates drop at coarse levels.
"""

import inspect
import tempfile
from pathlib import Path
from typing import Any

import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
STREAMING_SMALL = FIXTURES_DIR / "streaming" / "multi-rowgroup-small.parquet"
INPUT_FEATURES = 1000


def _overview(**kwargs: Any) -> tylertoo.OverviewReport:
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "overview.parquet"
        return tylertoo.overview(str(STREAMING_SMALL), str(out), **kwargs)


class TestVerbatim:
    """`verbatim` turns generalization off; explicit knobs still win."""

    def test_every_level_reproduces_the_input(self) -> None:
        report = _overview(max_zoom=8, verbatim=True)
        assert report["skipped_empty_levels"] == []
        counts = {lvl["feature_count"] for lvl in report["levels"]}
        assert counts == {INPUT_FEATURES}

    def test_an_explicit_knob_wins_over_verbatim(self) -> None:
        # The visibility gate alone hides these small polygons below z8;
        # empty levels are omitted from `levels` and listed as skipped.
        report = _overview(max_zoom=8, verbatim=True, polygon_visibility=2.0)
        assert [s["zoom"] for s in report["skipped_empty_levels"]] == list(range(8))

    def test_unset_knobs_keep_the_documented_defaults(self) -> None:
        explicit = _overview(
            max_zoom=8,
            simplify_factor=1.0,
            line_thinning=1.0,
            polygon_thinning=1.0,
            line_visibility=2.0,
            polygon_visibility=2.0,
            density_drop=True,
            coalesce_lines=True,
        )
        implicit = _overview(max_zoom=8)
        assert implicit["total_rows"] == explicit["total_rows"]
        assert implicit["total_vertices"] == explicit["total_vertices"]

    def test_the_default_run_generalizes(self) -> None:
        report = _overview(max_zoom=8)
        assert report["skipped_empty_levels"] != []

    def test_verbatim_with_partitioning_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="duplicating"):
            _overview(max_zoom=8, verbatim=True, mode="partitioning")

    def test_tuning_knobs_default_to_none(self) -> None:
        params = inspect.signature(tylertoo.overview).parameters
        for name in (
            "simplify_factor",
            "line_thinning",
            "polygon_thinning",
            "line_visibility",
            "polygon_visibility",
            "density_drop",
            "coalesce_lines",
        ):
            assert params[name].default is None, name
        assert params["verbatim"].default is False
