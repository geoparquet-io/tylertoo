"""`convert(bands=...)`, the `tiles --band` keyword (#332).

The archive is read back with the `pmtiles` reader and `mapbox-vector-tile`,
neither of which shares code with tylertoo.
"""

import gzip
import json
import struct
import subprocess
import sys
from pathlib import Path
from typing import Any

import mapbox_vector_tile  # type: ignore[import-untyped]
import pyarrow as pa  # type: ignore[import-untyped]
import pyarrow.parquet as pq  # type: ignore[import-untyped]
import pytest
from pmtiles.reader import MmapSource, Reader, all_tiles

import tylertoo

N = 120
BANDS = ["0-2:grid", "3-4:geom"]


def _write_source(path: Path) -> None:
    points = [
        struct.pack("<BIdd", 1, 1, -74.0 + (i % 12) * 0.05, 40.6 + (i // 12) * 0.05)
        for i in range(N)
    ]
    metadata = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["Point"]}},
    }
    table = pa.table(
        {"geometry": points, "area": [float(i) for i in range(N)]}
    ).replace_schema_metadata({b"geo": json.dumps(metadata).encode()})
    pq.write_table(table, path)


def _features(path: Path) -> dict[int, list[dict[str, Any]]]:
    """Per zoom, the properties of every feature in every tile."""
    with path.open("rb") as f:
        reader = Reader(MmapSource(f))
        zooms: dict[int, list[dict[str, Any]]] = {}
        for (z, _, _), data in all_tiles(reader.get_bytes):
            for layer in mapbox_vector_tile.decode(gzip.decompress(data)).values():
                zooms.setdefault(z, []).extend(
                    feature["properties"] for feature in layer["features"]
                )
    return zooms


def test_cells_below_and_points_above(tmp_path: Path) -> None:
    source = tmp_path / "points.parquet"
    out = tmp_path / "out.pmtiles"
    _write_source(source)

    report = tylertoo.convert(
        str(source), str(out), bands=BANDS, accumulate_attributes={"area": "sum"}
    )

    assert [z["zoom"] for z in report["export"]["zooms"]] == [0, 1, 2, 3, 4]
    for z, features in _features(out).items():
        if z <= 2:
            cells = {f["cell"]: f for f in features}
            assert sum(f["point_count"] for f in cells.values()) == N, z
            assert sum(f["area"] for f in cells.values()) == sum(range(N)), z
        else:
            assert all("cell" not in f for f in features), z


def test_bands_match_the_cli(tmp_path: Path) -> None:
    source = tmp_path / "points.parquet"
    _write_source(source)
    python = tmp_path / "python.pmtiles"
    cli = tmp_path / "cli.pmtiles"

    tylertoo.convert(str(source), str(python), bands=BANDS)
    flags = [arg for band in BANDS for arg in ("--band", band)]
    subprocess.run(
        [sys.executable, "-m", "tylertoo", "tiles", str(source), str(cli), *flags],
        check=True,
        capture_output=True,
    )

    assert python.read_bytes() == cli.read_bytes()


@pytest.mark.parametrize(
    "keyword", [{"min_zoom": 3}, {"max_zoom": 9}, {"representation": "0-4:point"}]
)
def test_bands_refuse_a_zoom_keyword(tmp_path: Path, keyword: dict[str, Any]) -> None:
    source = tmp_path / "points.parquet"
    _write_source(source)
    with pytest.raises(tylertoo.ConfigError, match="--band cannot be combined with"):
        tylertoo.convert(
            str(source), str(tmp_path / "o.pmtiles"), bands=BANDS, **keyword
        )


def test_overlapping_bands_are_refused_before_reading(tmp_path: Path) -> None:
    with pytest.raises(tylertoo.ConfigError, match="overlap at z2"):
        tylertoo.convert(
            str(tmp_path / "missing.parquet"),
            str(tmp_path / "o.pmtiles"),
            bands=["0-2:a5", "2-4:geom"],
        )
