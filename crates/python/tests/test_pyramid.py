"""Tests for `tylertoo.pyramid()`: per-band layers in one archive (#445)."""

import tempfile
from pathlib import Path

import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
STREAMING_SMALL = FIXTURES_DIR / "streaming" / "multi-rowgroup-small.parquet"
ROADS_PMTILES = FIXTURES_DIR / "golden" / "road-detections.pmtiles"
PMTILES_MAGIC = b"PMTiles\x03"


class TestPyramidApi:
    """Band-spec and argument errors, raised before any conversion."""

    def test_pyramid_is_exported(self) -> None:
        assert "pyramid" in tylertoo.__all__
        assert callable(tylertoo.pyramid)

    def test_a_plain_string_is_not_a_band_list(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.pyramid(f"0-5:{STREAMING_SMALL}", "/tmp/out.pmtiles")

    def test_empty_band_list_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="at least one"):
            tylertoo.pyramid([], "/tmp/out.pmtiles")

    def test_malformed_band_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="zoom"):
            tylertoo.pyramid(["9-2:" + str(STREAMING_SMALL)], "/tmp/out.pmtiles")

    def test_overlapping_bands_raise_value_error(self) -> None:
        with pytest.raises(ValueError, match="overlap"):
            tylertoo.pyramid(
                [f"0-5:{STREAMING_SMALL}:l", f"5-8:{STREAMING_SMALL}:l"],
                "/tmp/out.pmtiles",
            )

    def test_missing_band_input_is_named(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            missing = Path(tmpdir) / "nope.parquet"
            with pytest.raises(ValueError, match="band input not found"):
                tylertoo.pyramid([f"0-5:{missing}"], str(Path(tmpdir) / "o.pmtiles"))

    def test_options_are_keyword_only(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.pyramid(
                [f"0-5:{STREAMING_SMALL}"],
                "/tmp/out.pmtiles",
                True,  # type: ignore[misc]
            )


class TestPyramidIntegration:
    """Building archives from the committed fixtures.

    The fixture's buildings are smaller than an MVT pixel below about
    z8, so the bands start there.
    """

    def test_two_bands_become_two_layers(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "pyramid.pmtiles"
            decoded = Path(tmpdir) / "decoded.parquet"
            report = tylertoo.pyramid(
                [f"8-9:{STREAMING_SMALL}:coarse", f"10-12:{STREAMING_SMALL}:fine"],
                str(out),
            )
            assert out.read_bytes()[:8] == PMTILES_MAGIC
            back = tylertoo.decode(str(out), str(decoded))
        assert [b["layer"] for b in report["bands"]] == ["coarse", "fine"]
        assert [(b["min_zoom"], b["max_zoom"]) for b in report["bands"]] == [
            (8, 9),
            (10, 12),
        ]
        assert all(b["tiles"] > 0 for b in report["bands"])
        assert report["total_tiles"] == sum(b["tiles"] for b in report["bands"])
        assert report["skipped_tiles"] == 0
        assert report["bands_without_bounds"] == []
        assert back["layers"] == ["coarse", "fine"]
        assert back["zoom_range"] == (8, 12)

    def test_generalize_thins_the_coarse_zooms_of_a_band(self) -> None:
        # A band's finest zoom keeps every feature either way; its
        # coarser zooms are where generalization drops them.
        with tempfile.TemporaryDirectory() as tmpdir:
            verbatim = Path(tmpdir) / "verbatim.pmtiles"
            generalized = Path(tmpdir) / "generalized.pmtiles"
            band = [f"6-8:{STREAMING_SMALL}:b"]
            tylertoo.pyramid(band, str(verbatim))
            tylertoo.pyramid(band, str(generalized), generalize=True)
            kept = tylertoo.decode(
                str(verbatim), str(Path(tmpdir) / "v.parquet"), min_zoom=6, max_zoom=6
            )
            thinned = tylertoo.decode(
                str(generalized),
                str(Path(tmpdir) / "g.parquet"),
                min_zoom=6,
                max_zoom=6,
            )
        assert kept["features_written"] > 0
        assert thinned["features_written"] < kept["features_written"]

    def test_a_pretiled_archive_band_passes_through(self) -> None:
        # The archive holds z0-10; declaring z0-7 drops its finer tiles.
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "pyramid.pmtiles"
            report = tylertoo.pyramid(
                [f"8-9:{STREAMING_SMALL}:coarse", f"0-7:{ROADS_PMTILES}"],
                str(out),
            )
        # Reported by zoom range, coarsest first, not in the order given.
        assert [b["layer"] for b in report["bands"]] == ["road-detections", "coarse"]
        assert all(b["tiles"] > 0 for b in report["bands"])
        assert report["skipped_tiles"] > 0


class TestPyramidMetadata:
    """`name=`, `description=` and `attribution=`, and what the report says."""

    def test_keywords_and_inherited_credit_are_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            credited = Path(tmpdir) / "credited.pmtiles"
            first = tylertoo.pyramid(
                [f"8-9:{STREAMING_SMALL}:b"],
                str(credited),
                attribution="<b>Survey</b>",
            )
            out = Path(tmpdir) / "out.pmtiles"
            report = tylertoo.pyramid(
                [f"8-9:{credited}:b", f"10-11:{STREAMING_SMALL}:c"],
                str(out),
                name="Buildings",
            )
        assert first["metadata"] == {
            "name": None,
            "description": None,
            "attribution": "<b>Survey</b>",
        }
        # The PMTiles band passes its credit on; the keyword sets the name.
        assert report["metadata"] == {
            "name": "Buildings",
            "description": None,
            "attribution": "<b>Survey</b>",
        }
        assert report["metadata_conflicts"] == []
        assert report["bands_without_metadata"] == []

    def test_defaults_write_and_report_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            report = tylertoo.pyramid(
                [f"8-9:{STREAMING_SMALL}:b"], str(Path(tmpdir) / "o.pmtiles")
            )
        assert report["metadata"] == {
            "name": None,
            "description": None,
            "attribution": None,
        }
