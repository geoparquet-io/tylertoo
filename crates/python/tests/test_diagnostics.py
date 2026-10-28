"""Diagnostics belong to one conversion, including its warnings."""

import json
import logging
import os
import struct
import sys
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


@pytest.mark.parametrize("non_utf8", [False, True])
def test_large_losses_are_exact_caller_owned_jsonl(tmp_path, monkeypatch, non_utf8):
    """The Python dict stays bounded and its spilled rows outlive the call."""
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")
    spill = tmp_path / "spill"
    if non_utf8:
        if sys.platform != "linux":
            pytest.skip("non-UTF-8 paths are tested on Linux")
        spill = Path(os.fsdecode(os.fsencode(spill) + b"-\xff"))
    spill.mkdir()
    monkeypatch.setenv("TMPDIR", str(spill))
    spill_options = {} if non_utf8 else {"spill_dir": str(spill)}
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
            **spill_options,
        )
        losses = (
            report["convert"]["source_losses"]
            if command == "convert"
            else report["source_losses"]
        )
        assert losses["count"] == rows - 1
        path = Path(losses["path"])
        del report, losses
        assert path.parent == spill
        assert path.exists()
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
        **spill_options,
    )
    losses = report["source_losses"]
    assert losses["count"] == rows - 1
    path = Path(losses["path"])
    with path.open() as stream:
        assert all(json.loads(line)["cause"] == "quantized" for line in stream)
    path.unlink()
    assert list(spill.iterdir()) == []

    # Export fails after the convert stage has recorded thousands of losses.
    # None of the temporary ledgers or report files may survive that error.
    with pytest.raises(RuntimeError, match="feature"):
        tylertoo.convert(
            str(source),
            str(tmp_path / "failed.pmtiles"),
            min_zoom=0,
            max_zoom=0,
            filter="id = 0",
            feature_id="geometry",
            **spill_options,
        )
    assert list(spill.iterdir()) == []
