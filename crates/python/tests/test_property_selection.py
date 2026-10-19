"""Property selection kwargs on overview, export_pmtiles and convert (#445).

The fixture is an Overture-shaped input with an integer `id`, a `names`
struct, a `sources` list and a binary `raw` column that the export
cannot encode, so the exported fields without a selection are
`id`, `names` and `sources`.
"""

import gzip
import json
import struct
import tempfile
from pathlib import Path
from typing import Any

import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
OVERTURE_NAMES = FIXTURES_DIR / "properties" / "overture-names.parquet"


def _fields(path: Path) -> set[str]:
    """Property names in a PMTiles archive's `vector_layers` metadata."""
    buf = path.read_bytes()
    off, length = struct.unpack_from("<QQ", buf, 24)
    meta = json.loads(gzip.decompress(buf[off : off + length]))
    return {name for layer in meta["vector_layers"] for name in layer["fields"]}


def _overview_then_export(
    overview_kwargs: dict[str, Any], export_kwargs: dict[str, Any]
) -> set[str]:
    with tempfile.TemporaryDirectory() as tmpdir:
        ovr = Path(tmpdir) / "overview.parquet"
        pm = Path(tmpdir) / "out.pmtiles"
        tylertoo.overview(str(OVERTURE_NAMES), str(ovr), max_zoom=6, **overview_kwargs)
        tylertoo.export_pmtiles(str(ovr), str(pm), **export_kwargs)
        return _fields(pm)


class TestOverviewPropertySelection:
    """Selection at convert time: unselected columns are never read."""

    def test_default_keeps_every_exportable_property(self) -> None:
        assert _overview_then_export({}, {}) == {"id", "names", "sources"}

    def test_include_keeps_only_the_named_columns(self) -> None:
        fields = _overview_then_export({"include_properties": ["id"]}, {})
        assert fields == {"id"}

    def test_exclude_drops_the_named_columns(self) -> None:
        fields = _overview_then_export({"exclude_properties": ["names"]}, {})
        assert fields == {"id", "sources"}

    def test_exclude_all_writes_geometry_only(self) -> None:
        fields = _overview_then_export({"exclude_all_properties": True}, {})
        assert fields == set()

    def test_include_wins_over_exclude_all(self) -> None:
        fields = _overview_then_export(
            {"include_properties": ["sources"], "exclude_all_properties": True}, {}
        )
        assert fields == {"sources"}

    def test_an_empty_include_list_keeps_no_properties(self) -> None:
        assert _overview_then_export({"include_properties": []}, {}) == set()

    def test_unknown_include_raises_value_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "o.parquet"
            with pytest.raises(ValueError, match="not a column"):
                tylertoo.overview(
                    str(OVERTURE_NAMES), str(out), include_properties=["nope"]
                )

    def test_excluding_the_sort_key_column_raises_value_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            out = Path(tmpdir) / "o.parquet"
            with pytest.raises(ValueError, match="reads it"):
                tylertoo.overview(
                    str(OVERTURE_NAMES),
                    str(out),
                    sort_key="id",
                    exclude_properties=["id"],
                )

    def test_a_plain_string_is_not_a_column_list(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.overview(
                str(OVERTURE_NAMES),
                "/tmp/o.parquet",
                # A str is a Sequence[str] to the type checker, but
                # never a column list to the extension.
                exclude_properties="id",
            )


class TestExportPropertySelection:
    """Selection at export time: the overview file keeps every column."""

    def test_include_keeps_only_the_named_property(self) -> None:
        fields = _overview_then_export({}, {"include_properties": ["names"]})
        assert fields == {"names"}

    def test_exclude_and_exclude_all(self) -> None:
        assert _overview_then_export({}, {"exclude_properties": ["id"]}) == {
            "names",
            "sources",
        }
        assert _overview_then_export({}, {"exclude_all_properties": True}) == set()

    def test_unknown_include_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="not a property this overview file"):
            _overview_then_export({}, {"include_properties": ["nope"]})

    def test_including_an_unencodable_column_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="cannot encode"):
            _overview_then_export({}, {"include_properties": ["raw"]})

    def test_excluding_the_feature_order_column_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="reads it"):
            _overview_then_export(
                {}, {"feature_order": "id", "exclude_properties": ["id"]}
            )


class TestConvertPropertySelection:
    """The one-shot facade applies the selection at convert time."""

    def test_include_keeps_only_the_named_column(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            pm = Path(tmpdir) / "out.pmtiles"
            tylertoo.convert(
                str(OVERTURE_NAMES), str(pm), max_zoom=6, include_properties=["id"]
            )
            assert _fields(pm) == {"id"}

    def test_selection_kwargs_are_keyword_only(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.convert(
                str(OVERTURE_NAMES),
                "/tmp/out.pmtiles",
                0,
                6,
                None,
                512000,
                True,
                "input",
                None,
                ["id"],  # type: ignore[misc]
            )
