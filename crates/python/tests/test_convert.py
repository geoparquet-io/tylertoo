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

    def test_convert_max_zoom_auto(self):
        """#444: max_zoom="auto" estimates the zoom instead of a literal."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom="auto",
            )

            data = output.read_bytes()
            assert data[: len(PMTILES_MAGIC)] == PMTILES_MAGIC
            # PMTiles v3 header byte 101 is the archive's max zoom; the
            # open-buildings estimate pins to z14 (see the core
            # characterization test).
            assert data[101] == 14

    def test_convert_max_zoom_auto_checks_the_output_before_reading(self):
        """#452: "auto" reads the input to estimate, so an output in a
        missing directory is refused first, naming the directory."""
        with tempfile.TemporaryDirectory() as tmpdir:
            missing = Path(tmpdir) / "no-such-dir"
            with pytest.raises(RuntimeError, match="does not exist") as exc:
                tylertoo.convert(
                    input=str(Path(tmpdir) / "missing-input.parquet"),
                    output=str(missing / "out.pmtiles"),
                    max_zoom="auto",
                )
            assert str(missing) in str(exc.value)
            assert "missing-input" not in str(exc.value)

    def test_convert_max_zoom_auto_is_case_insensitive(self):
        """ "AUTO" / "Auto" are accepted the same as "auto"."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            for spelling in ("AUTO", "Auto"):
                output = Path(tmpdir) / f"output-{spelling}.pmtiles"
                tylertoo.convert(
                    input=str(input_file),
                    output=str(output),
                    max_zoom=spelling,  # type: ignore[arg-type]
                )
                assert output.exists()

    def test_convert_max_zoom_garbled_string_raises_value_error(self):
        """A non-"auto" string is a ValueError, not a silent misparse."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"
            with pytest.raises(ValueError, match="auto"):
                tylertoo.convert(
                    input=str(input_file),
                    output=str(output),
                    max_zoom="not-a-zoom",  # type: ignore[arg-type]
                )

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

    @pytest.mark.parametrize("off_value", [0, None])
    def test_convert_tile_size_limit_off_switch(self, off_value):
        """tile_size_limit=0 (or None) disables the raw cap (#280), and
        max_compressed_tile_size=0 the compressed one (#761)."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                tile_size_limit=off_value,
                max_compressed_tile_size=0,
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


PROPERTIES_FIXTURE = FIXTURES_DIR / "properties" / "overture-names.parquet"


@pytest.mark.skipif(
    not PROPERTIES_FIXTURE.exists(),
    reason="overture-names.parquet fixture not available",
)
class TestConvertCombinedReport:
    """convert() returns the combined report the CLI's `tiles --report`
    writes (#439): both halves of the chain, keyed "convert" and "export".

    This is the drift the shared core facade ends: convert() used to
    re-implement the chain with no report, no free-space preflight and no
    knob guards.
    """

    def test_convert_returns_combined_report(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            report = tylertoo.convert(
                input=str(PROPERTIES_FIXTURE),
                output=str(output),
                min_zoom=0,
                max_zoom=4,
            )

            assert output.exists()
            assert isinstance(report, dict)
            assert set(report) == {"convert", "export"}

            convert_half = report["convert"]
            assert convert_half["input_features"] > 0
            assert convert_half["levels"], "per-level stats present"

            export_half = report["export"]
            # #380: the requested minimum zoom is declared even when the
            # coarsest levels generalized away.
            assert export_half["min_zoom"] == 0
            assert export_half["max_zoom"] == 4
            assert export_half["total_tiles"] >= 1
            assert "duration_secs" in export_half

    def test_convert_report_matches_two_step_reports(self):
        """The combined report's halves carry the same keys the two-step
        overview() and export_pmtiles() reports do."""
        with tempfile.TemporaryDirectory() as tmpdir:
            one_shot = tylertoo.convert(
                input=str(PROPERTIES_FIXTURE),
                output=str(Path(tmpdir) / "one.pmtiles"),
                min_zoom=0,
                max_zoom=4,
            )

            ov = Path(tmpdir) / "ov.parquet"
            overview_report = tylertoo.overview(
                input=str(PROPERTIES_FIXTURE), output=str(ov), min_zoom=0, max_zoom=4
            )
            export_report = tylertoo.export_pmtiles(
                input=str(ov),
                output=str(Path(tmpdir) / "two.pmtiles"),
                layer_name="overture-names",
                min_zoom=0,
            )

            assert set(one_shot["convert"]) == set(overview_report)
            assert set(one_shot["export"]) == set(export_report)


class TestConvertSpillDir:
    """The `spill_dir` kwarg (#439 review).

    The one-shot facade puts its intermediate overview in `spill_dir`,
    then `$TMPDIR`, then the output's directory. Before this kwarg a
    Python caller could not redirect an at-least-input-sized file away
    from `$TMPDIR`, which is a small tmpfs in many containers.
    """

    def test_convert_honors_spill_dir(self):
        """The intermediate goes to spill_dir, and is cleaned up."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"
            spill = Path(tmpdir) / "spill"
            spill.mkdir()

            report = tylertoo.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                spill_dir=str(spill),
            )

            assert output.exists()
            assert set(report) == {"convert", "export"}
            # The temp intermediate is removed after the export, so the
            # directory it was created in is empty again.
            assert list(spill.iterdir()) == [], "the intermediate must be cleaned up"

    def test_convert_rejects_missing_spill_dir(self):
        """A spill_dir that does not exist is an error, not a silent fallback."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"
            missing = Path(tmpdir) / "nope" / "deeper"

            with pytest.raises((ValueError, RuntimeError)) as exc_info:
                tylertoo.convert(
                    input=str(input_file),
                    output=str(output),
                    min_zoom=0,
                    max_zoom=6,
                    spill_dir=str(missing),
                )
            assert "nope" in str(exc_info.value), str(exc_info.value)
