"""Diagnostics belong to one conversion, including its warnings."""

import json
import logging
import os
import struct
import sys
from pathlib import Path
from typing import Any

import pytest

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


ROWS = 4096
POINT = struct.pack("<BIdd", 1, 1, 0.0, 0.0)
# A ring 1e-6 degrees across: it collapses at the tile extent on export.
SLIVER = struct.pack("<BIII8d", 1, 3, 1, 4, 0, 0, 1e-6, 0, 1e-6, 1e-6, 0, 0)


@pytest.fixture(params=[False, True], ids=["utf8", "non-utf8"])
def spill(request, tmp_path, monkeypatch):
    """A spill directory, also TMPDIR; ``(path, spill_dir argument)``.

    The non-UTF-8 case passes no ``spill_dir``, so the ledger lands in TMPDIR.
    """
    path = tmp_path / "spill"
    if request.param:
        if sys.platform != "linux":
            pytest.skip("non-UTF-8 paths are tested on Linux")
        path = Path(os.fsdecode(os.fsencode(path) + b"-\xff"))
    path.mkdir()
    monkeypatch.setenv("TMPDIR", str(path))
    monkeypatch.setenv("TYLERTOO_AUTO_MEM_LIMIT_BYTES", str(1 << 20))
    return path, None if request.param else str(path)


def _write_source(path: Path, slivers: bool = False) -> Path:
    """ROWS rows: one point (id 0), then points or slivers."""
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    types = ["Point", "Polygon"] if slivers else ["Point"]
    metadata: dict[str, Any] = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": types}},
    }
    rest = SLIVER if slivers else POINT
    table = pa.table(
        {"geometry": [POINT] + [rest] * (ROWS - 1), "id": range(ROWS)}
    ).replace_schema_metadata({b"geo": json.dumps(metadata).encode()})
    pq.write_table(table, path, row_group_size=ROWS)
    return path


def _take_spilled(losses: Any, spill_path: Path) -> list[dict[str, Any]]:
    """Read and delete a spilled loss report the caller now owns."""
    assert losses["count"] == ROWS - 1
    path = Path(losses["path"])
    assert path.parent == spill_path
    with path.open() as stream:
        rows = [json.loads(line) for line in stream]
    path.unlink()
    return rows


@pytest.mark.parametrize("command", ["overview", "convert"])
def test_large_losses_are_exact_caller_owned_jsonl(tmp_path, spill, command):
    """The Python dict stays bounded and its spilled rows outlive the call."""
    spill_path, spill_dir = spill
    source = _write_source(tmp_path / "losses.parquet")
    suffix = "pmtiles" if command == "convert" else "parquet"
    report = getattr(tylertoo, command)(
        str(source),
        str(tmp_path / f"{command}.{suffix}"),
        min_zoom=0,
        max_zoom=0,
        filter="id = 0",
        spill_dir=spill_dir,
    )
    if command == "convert":
        report = report["convert"]
    losses = report["source_losses"]
    del report
    rows = _take_spilled(losses, spill_path)
    assert [(r["id"], r["cause"]) for r in rows] == [
        (row, "filtered") for row in range(1, ROWS)
    ]


def test_export_losses_spill_as_caller_owned_jsonl(tmp_path, spill):
    """Export-stage losses take the same spilled shape as convert's."""
    spill_path, spill_dir = spill
    source = _write_source(tmp_path / "slivers.parquet", slivers=True)
    overview = tmp_path / "quantized.parquet"
    tylertoo.overview(str(source), str(overview), min_zoom=0, max_zoom=0)
    report = tylertoo.export_pmtiles(
        str(overview),
        str(tmp_path / "quantized.pmtiles"),
        feature_id="id",
        spill_dir=spill_dir,
    )
    losses = report["source_losses"]
    assert not isinstance(losses, list)
    causes = {row["cause"] for row in _take_spilled(losses, spill_path)}
    assert causes == {"quantized"}
    assert list(spill_path.iterdir()) == []


def test_failed_conversion_leaves_no_spilled_files(tmp_path, spill):
    """Export fails after convert recorded thousands of losses; nothing leaks."""
    spill_path, spill_dir = spill
    source = _write_source(tmp_path / "losses.parquet")
    with pytest.raises(RuntimeError, match="feature"):
        tylertoo.convert(
            str(source),
            str(tmp_path / "failed.pmtiles"),
            min_zoom=0,
            max_zoom=0,
            filter="id = 0",
            feature_id="geometry",
            spill_dir=spill_dir,
        )
    assert list(spill_path.iterdir()) == []
