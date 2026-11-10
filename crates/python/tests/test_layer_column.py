"""`convert(layer_column=...)`, read back with independent readers (#821).

The archive is opened with the `pmtiles` reader and its tiles decoded
with `mapbox-vector-tile`, neither of which shares code with tylertoo.
"""

import ast
import gzip
import json
import struct
from pathlib import Path
from typing import Any

import mapbox_vector_tile  # type: ignore[import-untyped]
import pyarrow as pa  # type: ignore[import-untyped]
import pyarrow.parquet as pq  # type: ignore[import-untyped]
import pytest
from pmtiles.reader import MmapSource, Reader, all_tiles

import tylertoo

CROPS = ["soy", "corn", None, "   ", " soy ", "", "rice:2024"]


def _write_source(path: Path) -> None:
    points = [
        struct.pack("<BIdd", 1, 1, -60.0 + 20.0 * i, -30.0 + 10.0 * i)
        for i in range(len(CROPS))
    ]
    metadata = {
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["Point"]}},
    }
    table = pa.table(
        {"geometry": points, "crop": CROPS, "id": range(len(CROPS))}
    ).replace_schema_metadata({b"geo": json.dumps(metadata).encode()})
    pq.write_table(table, path)


def _read(path: Path) -> tuple[dict[str, Any], dict[int, dict[str, list[Any]]]]:
    """The archive's metadata, and per zoom each layer's features."""
    with path.open("rb") as f:
        reader = Reader(MmapSource(f))
        metadata = reader.metadata()
        zooms: dict[int, dict[str, list[Any]]] = {}
        for (z, _, _), data in all_tiles(reader.get_bytes):
            tile = mapbox_vector_tile.decode(gzip.decompress(data))
            for name, layer in tile.items():
                zooms.setdefault(z, {}).setdefault(name, []).extend(layer["features"])
    return metadata, zooms


def test_each_value_is_its_own_layer(tmp_path: Path) -> None:
    source = tmp_path / "crops.parquet"
    out = tmp_path / "crops.pmtiles"
    _write_source(source)

    report = tylertoo.convert(str(source), str(out), max_zoom=4, layer_column="crop")

    names = [layer["layer"] for layer in report["layers"]]
    assert names == ["_unknown", "corn", "rice:2024", "soy"]
    assert {layer["layer"]: layer["input_rows"] for layer in report["layers"]} == {
        "_unknown": 3,
        "corn": 1,
        "rice:2024": 1,
        "soy": 2,
    }

    metadata, zooms = _read(out)
    # A point on a tile edge is in every tile its buffer reaches: count ids.
    ids = {
        name: sorted({f["properties"]["id"] for f in features})
        for name, features in zooms[4].items()
    }
    assert ids == {
        "_unknown": [2, 3, 5],
        "corn": [1],
        "rice:2024": [6],
        "soy": [0, 4],
    }
    for features in zooms[4].values():
        for feature in features:
            assert "crop" not in feature["properties"]

    assert {layer["id"] for layer in metadata["vector_layers"]} == set(names)
    assert {s["layer"] for s in metadata["tilestats"]["layers"]} == set(names)
    for layer in metadata["vector_layers"]:
        assert "crop" not in layer["fields"]


def test_layered_mlt_matches_layered_mvt(tmp_path: Path) -> None:
    """#823 meets #821: every layer is encoded as MLT and merged as one."""
    source = tmp_path / "crops.parquet"
    _write_source(source)
    mvt, mlt = tmp_path / "mvt.pmtiles", tmp_path / "mlt.pmtiles"
    tylertoo.convert(str(source), str(mvt), max_zoom=4, layer_column="crop")
    tylertoo.convert(
        str(source), str(mlt), max_zoom=4, layer_column="crop", tile_format="mlt"
    )

    assert mlt.read_bytes()[99] == 6  # the header's tile type: MLT
    counts = [
        sorted(
            (r["z"], r["layer"], r["feature_count"])
            for r in tylertoo.stats(str(p), layers=True)["per_layer"]
        )
        for p in (mvt, mlt)
    ]
    assert counts[0] == counts[1]
    assert {layer for _, layer, _ in counts[1]} == {
        "_unknown",
        "corn",
        "rice:2024",
        "soy",
    }


def test_layer_column_excludes_layer_name(tmp_path: Path) -> None:
    source = tmp_path / "crops.parquet"
    _write_source(source)
    with pytest.raises(tylertoo.ConfigError, match="--layer-name"):
        tylertoo.convert(
            str(source),
            str(tmp_path / "o.pmtiles"),
            layer_name="x",
            layer_column="crop",
        )


def test_missing_layer_column_is_a_value_error(tmp_path: Path) -> None:
    source = tmp_path / "crops.parquet"
    _write_source(source)
    with pytest.raises(ValueError, match="not a column"):
        tylertoo.convert(str(source), str(tmp_path / "o.pmtiles"), layer_column="nope")


def test_the_layered_overload_takes_every_convert_parameter() -> None:
    """The two `convert` overloads differ only in `layer_column`'s type.

    A parameter added to one alone makes a valid call match neither.
    """
    stub = Path(tylertoo.__file__).with_name("_tylertoo.pyi")
    overloads = [
        node
        for node in ast.parse(stub.read_text(encoding="utf-8")).body
        if isinstance(node, ast.FunctionDef) and node.name == "convert"
    ]

    def signature(fn: ast.FunctionDef) -> list[tuple[str, str]]:
        params = fn.args.args + fn.args.kwonlyargs
        return [
            (p.arg, "" if p.arg == "layer_column" else ast.unparse(p.annotation or p))
            for p in params
        ]

    assert len(overloads) == 2
    assert signature(overloads[0]) == signature(overloads[1])
