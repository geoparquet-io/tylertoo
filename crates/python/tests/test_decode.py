"""Tests for `tylertoo.decode()`: PMTiles back to GeoParquet (#445)."""

import tempfile
from pathlib import Path

import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
ROADS_PMTILES = FIXTURES_DIR / "golden" / "road-detections.pmtiles"
STREAMING_SMALL = FIXTURES_DIR / "streaming" / "multi-rowgroup-small.parquet"
PARQUET_MAGIC = b"PAR1"


class TestDecodeApi:
    """Argument handling; no decode work needed."""

    def test_decode_is_exported(self) -> None:
        assert "decode" in tylertoo.__all__
        assert callable(tylertoo.decode)

    def test_inverted_zoom_range_raises_value_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            with pytest.raises(ValueError, match="exceeds"):
                tylertoo.decode(str(ROADS_PMTILES), str(out), min_zoom=8, max_zoom=5)
            assert not out.exists()

    def test_zooms_are_keyword_only(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.decode(str(ROADS_PMTILES), "/tmp/x.parquet", 5)  # type: ignore[misc, arg-type]

    def test_missing_input_raises_runtime_error(self) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            pytest.raises(RuntimeError, match="decode failed"),
        ):
            tylertoo.decode(
                str(Path(tmpdir) / "missing.pmtiles"),
                str(Path(tmpdir) / "out.parquet"),
            )

    def test_a_parquet_input_raises_runtime_error(self) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            pytest.raises(RuntimeError, match="decode failed"),
        ):
            tylertoo.decode(str(STREAMING_SMALL), str(Path(tmpdir) / "o.parquet"))


class TestDecodeIntegration:
    """Decoding committed archives."""

    def test_report_describes_the_decode(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            report = tylertoo.decode(str(ROADS_PMTILES), str(out))
            assert out.read_bytes()[:4] == PARQUET_MAGIC
        assert report["tiles_read"] > 0
        assert report["features_written"] > 0
        assert report["features_skipped"] >= 0
        assert report["layers"]
        assert report["zoom_range"] is not None
        assert report["elapsed_secs"] >= 0

    def test_zoom_filter_limits_the_zoom_range(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            report = tylertoo.decode(
                str(ROADS_PMTILES), str(out), min_zoom=7, max_zoom=7
            )
        assert report["zoom_range"] == (7, 7)

    def test_an_unmatched_layer_writes_no_features(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "out.parquet"
            report = tylertoo.decode(str(ROADS_PMTILES), str(out), layer="nope")
        assert report["features_written"] == 0
        assert report["zoom_range"] is None
        assert report["layers"] == []

    def test_round_trip_through_overview_and_export(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            ovr = Path(tmpdir) / "overview.parquet"
            pm = Path(tmpdir) / "out.pmtiles"
            out = Path(tmpdir) / "decoded.parquet"
            tylertoo.overview(str(STREAMING_SMALL), str(ovr), max_zoom=10)
            tylertoo.export_pmtiles(str(ovr), str(pm))
            report = tylertoo.decode(str(pm), str(out), min_zoom=10, max_zoom=10)
        assert report["layers"] == ["overview"]
        assert report["zoom_range"] == (10, 10)
        assert report["features_written"] >= 1000
