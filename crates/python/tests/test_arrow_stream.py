"""Arrow stream input (#824): DuckDB, Polars and PyArrow tile like the file."""

import json
from pathlib import Path

import duckdb
import polars as pl
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import tylertoo

FIXTURE = (
    Path(__file__).parents[3]
    / "tests"
    / "fixtures"
    / "streaming"
    / "multi-rowgroup-small.parquet"
)
KNOBS = {"max_zoom": 8, "layer_name": "layer"}


def geoarrow_table() -> pa.Table:
    """The fixture with a `geoarrow.wkb` geometry carrying its CRS."""
    table = pq.read_table(FIXTURE).replace_schema_metadata(None)
    i = table.schema.get_field_index("geometry")
    field = table.schema.field(i).with_metadata(
        {
            "ARROW:extension:name": "geoarrow.wkb",
            "ARROW:extension:metadata": json.dumps({"crs": "OGC:CRS84"}),
        }
    )
    return table.cast(table.schema.set(i, field))


STREAMS = {
    "pyarrow-table": lambda: (geoarrow_table(), {}),
    "pyarrow-reader": lambda: (
        pa.RecordBatchReader.from_batches(
            geoarrow_table().schema, geoarrow_table().to_batches(max_chunksize=64)
        ),
        {},
    ),
    "polars": lambda: (
        pl.read_parquet(FIXTURE),
        {"geometry_column": "geometry", "crs": "EPSG:4326"},
    ),
    "duckdb": lambda: (
        # DuckDB exports GEOMETRY as geoarrow.wkb with its PROJJSON CRS.
        duckdb.sql(f"SELECT * FROM read_parquet('{FIXTURE}')"),
        {},
    ),
}


@pytest.fixture(scope="module")
def file_archive(tmp_path_factory: pytest.TempPathFactory) -> bytes:
    out = tmp_path_factory.mktemp("file") / "out.pmtiles"
    tylertoo.convert(str(FIXTURE), str(out), **KNOBS)
    return out.read_bytes()


@pytest.mark.parametrize("kind", sorted(STREAMS))
def test_a_stream_tiles_byte_identical_to_the_file(
    kind: str, file_archive: bytes, tmp_path: Path
) -> None:
    stream, args = STREAMS[kind]()
    spill = tmp_path / "spill"
    spill.mkdir()
    out = tmp_path / "out.pmtiles"
    tylertoo.convert(stream, str(out), spill_dir=str(spill), **args, **KNOBS)
    assert out.read_bytes() == file_archive
    assert list(spill.iterdir()) == [], "the staged file is removed"


def test_overview_takes_a_stream(tmp_path: Path) -> None:
    from_file = tmp_path / "file.parquet"
    from_stream = tmp_path / "stream.parquet"
    tylertoo.overview(str(FIXTURE), str(from_file), max_zoom=8)
    tylertoo.overview(geoarrow_table(), str(from_stream), max_zoom=8)
    assert pq.read_table(from_stream).equals(pq.read_table(from_file))


def test_an_unsupported_crs_is_refused_and_the_stage_removed(tmp_path: Path) -> None:
    spill = tmp_path / "spill"
    spill.mkdir()
    with pytest.raises(ValueError, match="unsupported CRS"):
        tylertoo.convert(
            pl.read_parquet(FIXTURE),
            str(tmp_path / "out.pmtiles"),
            spill_dir=str(spill),
            geometry_column="geometry",
            crs="EPSG:2263",
        )
    assert list(spill.iterdir()) == []


def test_stream_arguments_are_refused_for_a_file(tmp_path: Path) -> None:
    with pytest.raises(tylertoo.ConfigError, match="Arrow stream"):
        tylertoo.convert(str(FIXTURE), str(tmp_path / "o.pmtiles"), crs="EPSG:4326")


def test_a_stream_without_geometry_names_the_argument(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="geometry_column="):
        tylertoo.convert(pl.read_parquet(FIXTURE), str(tmp_path / "o.pmtiles"))


def test_a_saved_plan_is_refused_for_a_stream(tmp_path: Path) -> None:
    with pytest.raises(tylertoo.ConfigError, match="file input"):
        tylertoo.overview(
            geoarrow_table(),
            str(tmp_path / "o.parquet"),
            save_plan=str(tmp_path / "plan"),
        )
