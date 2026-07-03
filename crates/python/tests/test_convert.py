"""Tests for gpq_tiles Python bindings."""

import tempfile
from pathlib import Path

import gpq_tiles
import pytest

# Path to test fixtures (relative to workspace root)
FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
REALDATA_DIR = FIXTURES_DIR / "realdata"


class TestConvertFunction:
    """Tests for the convert() function."""

    def test_convert_exists(self):
        """Verify convert function is exported."""
        assert hasattr(gpq_tiles, "convert")
        assert callable(gpq_tiles.convert)

    def test_convert_has_docstring(self):
        """Verify convert function has documentation."""
        assert gpq_tiles.convert.__doc__ is not None
        assert "GeoParquet" in gpq_tiles.convert.__doc__
        assert "PMTiles" in gpq_tiles.convert.__doc__

    def test_convert_signature_defaults(self):
        """Test that convert() has expected default parameters."""
        # This will fail with TypeError if required args are missing
        with pytest.raises(TypeError) as exc_info:
            gpq_tiles.convert()

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
                gpq_tiles.convert(
                    input="/nonexistent/path/to/file.parquet",
                    output=str(output),
                )

            # Should raise RuntimeError with meaningful message
            assert exc_info.type.__name__ in ("RuntimeError", "Exception")

    def test_convert_invalid_drop_density(self):
        """Test error for invalid drop_density value."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            with pytest.raises(ValueError) as exc_info:
                gpq_tiles.convert(
                    input="/some/input.parquet",
                    output=str(output),
                    drop_density="invalid_value",
                )

            assert "invalid drop density" in str(exc_info.value).lower()

    def test_convert_invalid_zoom_levels(self):
        """Test error when max_zoom < min_zoom."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            # This should fail during conversion, not argument parsing
            with pytest.raises(Exception):
                gpq_tiles.convert(
                    input="/nonexistent/file.parquet",
                    output=str(output),
                    min_zoom=10,
                    max_zoom=5,
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
            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=8,
            )

            # Output file should exist
            # Note: File content validation is tested in Rust integration tests
            assert output.exists()

    def test_convert_with_drop_density_low(self):
        """Test conversion with low drop density."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                drop_density="low",
            )

            assert output.exists()

    def test_convert_with_drop_density_high(self):
        """Test conversion with high drop density."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                drop_density="high",
            )

            assert output.exists()

    def test_convert_single_zoom(self):
        """Test conversion with single zoom level."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=8,
                max_zoom=8,
            )

            assert output.exists()


class TestPropertyFiltering:
    """Tests for property filtering parameters."""

    def test_convert_accepts_include_parameter(self):
        """Test that convert() accepts include parameter for property whitelist."""
        # Should accept without TypeError
        with pytest.raises(Exception) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                include=["name", "population"],
            )
        # Should fail on file not found, not parameter error
        assert "TypeError" not in str(type(exc_info.value))

    def test_convert_accepts_exclude_parameter(self):
        """Test that convert() accepts exclude parameter for property blacklist."""
        # Should accept without TypeError
        with pytest.raises(Exception) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                exclude=["internal_id"],
            )
        # Should fail on file not found, not parameter error
        assert "TypeError" not in str(type(exc_info.value))

    def test_convert_accepts_exclude_all_parameter(self):
        """Test that convert() accepts exclude_all parameter for geometry-only output."""
        # Should accept without TypeError
        with pytest.raises(Exception) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                exclude_all=True,
            )
        # Should fail on file not found, not parameter error
        assert "TypeError" not in str(type(exc_info.value))

    def test_convert_rejects_include_with_exclude(self):
        """Test that using both include and exclude raises ValueError."""
        with pytest.raises(ValueError) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                include=["name"],
                exclude=["population"],
            )
        assert "include" in str(exc_info.value).lower() or "exclude" in str(exc_info.value).lower()

    def test_convert_rejects_include_with_exclude_all(self):
        """Test that using include with exclude_all raises ValueError."""
        with pytest.raises(ValueError) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                include=["name"],
                exclude_all=True,
            )
        assert "include" in str(exc_info.value).lower() or "exclude" in str(exc_info.value).lower()

    def test_convert_rejects_exclude_with_exclude_all(self):
        """Test that using exclude with exclude_all raises ValueError."""
        with pytest.raises(ValueError) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                exclude=["temp"],
                exclude_all=True,
            )
        assert "exclude" in str(exc_info.value).lower()


class TestLayerNameOverride:
    """Tests for layer_name parameter."""

    def test_convert_accepts_layer_name_parameter(self):
        """Test that convert() accepts layer_name parameter."""
        # Should accept without TypeError
        with pytest.raises(Exception) as exc_info:
            gpq_tiles.convert(
                input="/nonexistent/file.parquet",
                output="/tmp/output.pmtiles",
                layer_name="custom_layer",
            )
        # Should fail on file not found, not parameter error
        assert "TypeError" not in str(type(exc_info.value))


# NOTE: The former TestStreamingMode and TestParallelControls classes tested
# `streaming_mode`, `parallel_tiles`, and `parallel_geoms` kwargs that were
# never implemented in the binding or in gpq-tiles-core (issue #174). The
# tiles pipeline exposes `deterministic=True` for sequential processing
# instead; the overview pipeline exposes `streaming`/`read_batch_size`
# (see test_overview.py).


class TestProgressCallback:
    """Tests for progress callback functionality."""

    def test_convert_accepts_progress_callback(self):
        """Verify convert() accepts a progress_callback parameter."""
        # This should not raise TypeError for unknown parameter
        import inspect

        sig = inspect.signature(gpq_tiles.convert)
        param_names = list(sig.parameters.keys())
        assert "progress_callback" in param_names

    def test_progress_callback_none_is_valid(self):
        """Test that progress_callback=None is accepted (no callback)."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            # Should not raise - None means no callback
            with pytest.raises(Exception):
                # Will fail because input doesn't exist, but that's fine
                # We're just checking the parameter is accepted
                gpq_tiles.convert(
                    input="/nonexistent/file.parquet",
                    output=str(output),
                    progress_callback=None,
                )

    def test_progress_callback_invalid_type_raises(self):
        """Test that non-callable progress_callback raises TypeError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            with pytest.raises(TypeError):
                gpq_tiles.convert(
                    input="/nonexistent/file.parquet",
                    output=str(output),
                    progress_callback="not a callable",
                )


@pytest.mark.skipif(
    not (REALDATA_DIR / "open-buildings.parquet").exists(),
    reason="Test fixture not available",
)
class TestPropertyFilteringIntegration:
    """Integration tests for property filtering with real data."""

    def test_convert_with_include_filter(self):
        """Test conversion with include filter."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                include=["area_in_meters"],  # Only include area
            )

            assert output.exists()

    def test_convert_with_exclude_filter(self):
        """Test conversion with exclude filter."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                exclude=["confidence"],  # Exclude confidence
            )

            assert output.exists()

    def test_convert_with_exclude_all(self):
        """Test conversion with exclude_all (geometry only)."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                exclude_all=True,
            )

            assert output.exists()

    def test_convert_with_layer_name_override(self):
        """Test conversion with custom layer name."""
        input_file = REALDATA_DIR / "open-buildings.parquet"

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=6,
                layer_name="buildings",
            )

            assert output.exists()


@pytest.mark.skipif(
    not (REALDATA_DIR / "open-buildings.parquet").exists(),
    reason="Test fixture not available",
)
class TestProgressCallbackIntegration:
    """Integration tests for progress callback with real data."""

    def test_progress_callback_is_called(self):
        """Test that progress callback is actually invoked during conversion."""
        input_file = REALDATA_DIR / "open-buildings.parquet"
        events = []

        def callback(event):
            events.append(event)

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=4,  # Small range for faster test
                progress_callback=callback,
            )

            # Should have received at least some events
            assert len(events) > 0

    def test_progress_callback_event_structure_phase_start(self):
        """Test that PhaseStart events have correct structure."""
        input_file = REALDATA_DIR / "open-buildings.parquet"
        events = []

        def callback(event):
            events.append(event)

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=4,
                progress_callback=callback,
            )

        # Find PhaseStart events
        phase_start_events = [e for e in events if e.get("phase") == "start"]
        assert len(phase_start_events) > 0

        # Check structure
        for event in phase_start_events:
            assert "phase" in event
            assert event["phase"] == "start"
            assert "phase_num" in event
            assert "name" in event
            assert isinstance(event["phase_num"], int)
            assert isinstance(event["name"], str)

    def test_progress_callback_event_structure_complete(self):
        """Test that Complete event has correct structure."""
        input_file = REALDATA_DIR / "open-buildings.parquet"
        events = []

        def callback(event):
            events.append(event)

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=4,
                progress_callback=callback,
            )

        # Find Complete event
        complete_events = [e for e in events if e.get("phase") == "complete"]
        assert len(complete_events) == 1

        complete = complete_events[0]
        assert "total_tiles" in complete
        assert "peak_memory_bytes" in complete
        assert "duration_secs" in complete
        assert isinstance(complete["total_tiles"], int)
        assert isinstance(complete["peak_memory_bytes"], int)
        assert isinstance(complete["duration_secs"], float)

    def test_progress_callback_receives_all_phases(self):
        """Test that callback receives events from all processing phases."""
        input_file = REALDATA_DIR / "open-buildings.parquet"
        events = []

        def callback(event):
            events.append(event)

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=4,
                progress_callback=callback,
            )

        # Extract unique phase types
        phase_types = {e.get("phase") for e in events}

        # Should have start, progress phases, and complete
        assert "start" in phase_types
        assert "complete" in phase_types
        # At least one progress type (phase1_progress, phase2_start, etc.)
        progress_phases = {"phase1_progress", "phase1_complete", "phase2_start", "phase2_complete", "phase3_progress"}
        assert len(phase_types & progress_phases) > 0

    def test_progress_callback_lambda(self):
        """Test that lambda callbacks work."""
        input_file = REALDATA_DIR / "open-buildings.parquet"
        call_count = [0]

        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "output.pmtiles"

            gpq_tiles.convert(
                input=str(input_file),
                output=str(output),
                min_zoom=0,
                max_zoom=4,
                progress_callback=lambda e: call_count.__setitem__(0, call_count[0] + 1),
            )

        assert call_count[0] > 0
