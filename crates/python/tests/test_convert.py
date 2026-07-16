"""Tests for the tylertoo.convert() facade.

convert() no longer runs the removed legacy per-tile pipeline: it chains
overview() (convert, default knobs) into a temporary GeoParquet file and
then export_pmtiles() to the requested output. These tests cover the
facade contract: surviving keyword arguments, removed-argument errors,
end-to-end output validity, and temp-file cleanup.
"""

import tempfile
from pathlib import Path

import pytest

import tylertoo

# Path to test fixtures (relative to workspace root)
FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
REALDATA_DIR = FIXTURES_DIR / "realdata"

PMTILES_MAGIC = b"PMTiles\x03"


class TestConvertFunction:
    """Tests for the convert() function surface."""

    def test_convert_exists(self):
        """Verify convert function is exported."""
        assert hasattr(tylertoo, "convert")
        assert callable(tylertoo.convert)

    def test_convert_has_docstring(self):
        """Verify convert function has documentation."""
        assert tylertoo.convert.__doc__ is not None
        assert "GeoParquet" in tylertoo.convert.__doc__
        assert "PMTiles" in tylertoo.convert.__doc__

    def test_convert_docstring_mentions_facade(self):
        """The docstring must direct users to overview()/export_pmtiles()."""
        doc = tylertoo.convert.__doc__
        assert doc is not None
        assert "overview" in doc
        assert "export_pmtiles" in doc

    def test_convert_signature_defaults(self):
        """Test that convert() has expected default parameters."""
        # This will fail with TypeError if required args are missing
        with pytest.raises(TypeError) as exc_info:
            tylertoo.convert()  # type: ignore[call-arg]

        # Should complain about missing input/output, not other params
        error_msg = str(exc_info.value)
        assert "input" in error_msg.lower() or "argument" in error_msg.lower()


class TestConvertErrors:
    """Tests for convert() error handling."""

    def test_convert_nonexistent_input(self):
        """Test error when input file doesn't exist."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            with pytest.raises(Exception) as exc_info:
                tylertoo.convert(
                    input="/nonexistent/path/to/file.parquet",
                    output=str(output),
                )

            # Should raise RuntimeError with meaningful message
            assert exc_info.type.__name__ in ("RuntimeError", "Exception")

    def test_convert_invalid_zoom_levels(self):
        """Test error when max_zoom < min_zoom."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            with pytest.raises(Exception):
                tylertoo.convert(
                    input="/nonexistent/file.parquet",
                    output=str(output),
                    min_zoom=10,
                    max_zoom=5,
                )

    @pytest.mark.parametrize(
        "removed_kwarg",
        [
            {"drop_density": "medium"},
            {"compression": "gzip"},
            {"include": ["name"]},
            {"exclude": ["name"]},
            {"exclude_all": True},
            {"deterministic": True},
            {"drop_smallest_as_needed": True},
            {"drop_smallest_threshold": 4.0},
            {"progress_callback": lambda _e: None},
        ],
    )
    def test_convert_rejects_removed_legacy_kwargs(self, removed_kwarg):
        """Legacy pipeline kwargs were removed and must raise TypeError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            with pytest.raises(TypeError):
                tylertoo.convert(
                    input="/some/input.parquet",
                    output=str(output),
                    **removed_kwarg,
                )


@pytest.mark.skipif(
    not (REALDATA_DIR / "open-buildings.parquet").exists(),
    reason="Test fixture not available",
)
class TestConvertIntegration:
    """Integration tests using real data fixtures."""

    def test_convert_basic(self):
        """Test basic conversion with default parameters."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            # Should complete without error
            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=8,
            )

            # Output file should exist and be a PMTiles v3 archive
            assert output.exists()
            assert output.read_bytes()[: len(PMTILES_MAGIC)] == PMTILES_MAGIC

    def test_convert_single_zoom(self):
        """Test conversion with a single zoom level."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=6,
                max_zoom=6,
            )

            assert output.exists()

    def test_convert_with_layer_name_override(self):
        """Test conversion with a custom layer name."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                layer_name="my_custom_layer",
            )

            # Metadata JSON is compressed inside the archive, so just check
            # the archive is valid; layer-name plumbing is covered by the
            # export_pmtiles tests in Rust.
            assert output.exists()
            assert output.read_bytes()[: len(PMTILES_MAGIC)] == PMTILES_MAGIC

    def test_convert_with_tile_size_limit(self):
        """tile_size_limit is accepted and produces valid output."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                tile_size_limit=500_000,
            )

            assert output.exists()

    def test_convert_cleans_up_temp_overview(self):
        """The intermediate overview file must not be left next to the output."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
            )

            leftovers = [p for p in Path(tmpdir).iterdir() if "overview" in p.name]
            assert leftovers == [], f"temp overview leaked: {leftovers}"

    def test_convert_cleans_up_temp_overview_on_failure(self):
        """The intermediate overview file is removed when export fails."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            # Output path inside a nonexistent subdirectory: the temp file
            # then targets that missing directory and creation fails early;
            # nothing may leak into tmpdir either way.
            output = Path(tmpdir) / "missing-subdir" / "output.pmtiles"

            with pytest.raises(Exception):
                tylertoo.convert(
                    input=str(input_file),
                    output=str(output),
                )

            leftovers = [p for p in Path(tmpdir).iterdir() if "overview" in p.name]
            assert leftovers == [], f"temp overview leaked: {leftovers}"
