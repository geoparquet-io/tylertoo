"""Diagnostics belong to one conversion, including its warnings."""

import json
import logging
import struct
from pathlib import Path

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


def test_large_losses_are_exact_caller_owned_jsonl(tmp_path, monkeypatch):
    """The Python dict stays bounded and its spilled rows outlive the call."""
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    rows = 4096
    source = tmp_path / "losses.parquet"
    metadata = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["Point"]}},
    }
    table = pa.table(
        {"geometry": [struct.pack("<BIdd", 1, 1, 0.0, 0.0)] * rows, "id": range(rows)}
    ).replace_schema_metadata({b"geo": json.dumps(metadata).encode()})
    pq.write_table(table, source, row_group_size=rows)
    monkeypatch.setenv("TYLERTOO_AUTO_MEM_LIMIT_BYTES", str(1 << 20))
    for command in ("overview", "convert"):
        output = (
            tmp_path / f"{command}.{'pmtiles' if command == 'convert' else 'parquet'}"
        )
        report = getattr(tylertoo, command)(
            str(source),
            str(output),
            min_zoom=0,
            max_zoom=0,
            filter="id = 0",
            spill_dir=str(tmp_path),
        )
        losses = (
            report["convert"]["source_losses"]
            if command == "convert"
            else report["source_losses"]
        )
        assert losses["count"] == rows - 1
        path = Path(losses["path"])
        del report, losses
        assert path.parent == tmp_path
        with path.open() as stream:
            for row, line in enumerate(stream, 1):
                loss = json.loads(line)
                assert loss["id"] == row
                assert loss["cause"] == "filtered"
        assert row == rows - 1
        path.unlink()

    metadata["columns"]["geometry"]["geometry_types"].append("Polygon")
    table = table.replace_schema_metadata({b"geo": json.dumps(metadata).encode()})
    polygon = struct.pack("<BIII8d", 1, 3, 1, 4, 0, 0, 1e-6, 0, 1e-6, 1e-6, 0, 0)
    geometry = pa.array([struct.pack("<BIdd", 1, 1, 0.0, 0.0)] + [polygon] * (rows - 1))
    pq.write_table(
        table.set_column(0, "geometry", geometry), source, row_group_size=rows
    )
    overview = tmp_path / "quantized.parquet"
    tylertoo.overview(str(source), str(overview), min_zoom=0, max_zoom=0)
    report = tylertoo.export_pmtiles(
        str(overview),
        str(tmp_path / "quantized.pmtiles"),
        feature_id="id",
        spill_dir=str(tmp_path),
    )
    losses = report["source_losses"]
    assert losses["count"] == rows - 1
    path = Path(losses["path"])
    with path.open() as stream:
        assert all(json.loads(line)["cause"] == "quantized" for line in stream)
    path.unlink()
