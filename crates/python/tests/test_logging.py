"""Core diagnostics reach Python's `logging` (#445, #376).

The bindings forward the Rust `log` records through pyo3-log, so the
warnings the CLI prints (out-of-range input, CRS assumptions, density
budget) are visible to Python callers under the `tylertoo_core`
logger instead of being silently dropped.
"""

import logging
import tempfile
from pathlib import Path

import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
BEYOND_WRAP = FIXTURES_DIR / "streaming" / "out-of-range-beyond-wrap.parquet"
STREAMING_SMALL = FIXTURES_DIR / "streaming" / "multi-rowgroup-small.parquet"


def _core_records(caplog: pytest.LogCaptureFixture) -> list[logging.LogRecord]:
    return [r for r in caplog.records if r.name.startswith("tylertoo_core")]


def test_out_of_range_warning_reaches_python_logging(
    caplog: pytest.LogCaptureFixture,
) -> None:
    caplog.set_level(logging.WARNING, logger="tylertoo_core")
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "o.parquet"
        report = tylertoo.overview(str(BEYOND_WRAP), str(out), max_zoom=4)
    assert report["out_of_range_features"] == 1
    warnings = [r for r in _core_records(caplog) if r.levelno == logging.WARNING]
    assert any("range" in r.getMessage() for r in warnings), [
        r.getMessage() for r in warnings
    ]


def test_records_use_dotted_module_logger_names(
    caplog: pytest.LogCaptureFixture,
) -> None:
    caplog.set_level(logging.INFO, logger="tylertoo_core")
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "o.parquet"
        tylertoo.overview(str(STREAMING_SMALL), str(out), max_zoom=6)
    names = {r.name for r in _core_records(caplog)}
    assert names
    assert all("::" not in name for name in names)
    assert all(name.split(".")[0] == "tylertoo_core" for name in names)


def test_a_level_change_between_calls_takes_effect(
    caplog: pytest.LogCaptureFixture,
) -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "o.parquet"
        caplog.set_level(logging.WARNING, logger="tylertoo_core")
        tylertoo.overview(str(STREAMING_SMALL), str(out), max_zoom=6)
        quiet = [r for r in _core_records(caplog) if r.levelno == logging.INFO]
        caplog.clear()
        caplog.set_level(logging.INFO, logger="tylertoo_core")
        tylertoo.overview(str(STREAMING_SMALL), str(out), max_zoom=6)
        loud = [r for r in _core_records(caplog) if r.levelno == logging.INFO]
    assert quiet == []
    assert loud


def test_debug_logging_from_worker_threads_does_not_deadlock(
    caplog: pytest.LogCaptureFixture,
) -> None:
    caplog.set_level(logging.DEBUG, logger="tylertoo_core")
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "o.parquet"
        report = tylertoo.overview(
            str(STREAMING_SMALL),
            str(out),
            max_zoom=8,
            read_workers=2,
            in_flight_batches=2,
        )
    assert report["input_features"] == 1000
    assert any(r.levelno == logging.DEBUG for r in _core_records(caplog))
