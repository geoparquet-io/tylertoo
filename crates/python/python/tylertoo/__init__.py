"""Build GeoParquet overview files and export them to PMTiles archives.

Options and defaults match the CLI. Ground sample distance (GSD) is
the meters covered by one pixel at a level. See the
[tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
for generalization options.

Diagnostics go to Python's `logging` under the `tylertoo_core` logger:
the warnings and progress lines the CLI prints. Python shows warnings
by default; call `logging.basicConfig(level=logging.INFO)` to see
progress. Each call reads the logging configuration when it starts.
"""

from __future__ import annotations

from typing import Any, Literal, TypedDict

from tylertoo import _tylertoo

from ._tylertoo import convert, decode, export_pmtiles, overview, pyramid, validate

__all__ = [
    "ConfigError",
    "ConvertReport",
    "DecodeReport",
    "Diagnostic",
    "ExportReport",
    "LevelReport",
    "OutOfRangeExemplar",
    "OverviewReport",
    "PublicationMetadata",
    "PyramidBandReport",
    "PyramidReport",
    "RemoteFetch",
    "SkippedLevel",
    "SkippedPropertyColumn",
    "SourceLoss",
    "SpilledSourceLosses",
    "ValidationCheck",
    "ValidationReport",
    "ZoomReport",
    "convert",
    "decode",
    "export_pmtiles",
    "overview",
    "pyramid",
    "validate",
]


LevelReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "LevelReport",
    {
        "level": "int",
        "gsd": "float",
        "zoom": "int | None",
        "feature_count": "int",
        "vertex_count": "int",
        "uncompressed_bytes": "int",
        "compressed_bytes": "int",
    },
)
LevelReport.__doc__ = """Counts and sizes for one overview level."""


SkippedLevel = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "SkippedLevel",
    {
        "planned_level": "int",
        "gsd": "float",
        "zoom": "int | None",
    },
)
SkippedLevel.__doc__ = """A planned level omitted because no features are visible."""


OutOfRangeExemplar = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "OutOfRangeExemplar",
    {
        "part": "int | None",
        "row": "int",
        "axis": "Literal['lon', 'lat', 'x', 'y']",
        "value": "float",
    },
)
OutOfRangeExemplar.__doc__ = """
Input location of a feature with out-of-range coordinates.

Attributes:
    part: Input file index, or `None` for a single file.
    axis: `"x"` and `"y"` indicate Web Mercator (`EPSG:3857`) input."""


RemoteFetch = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "RemoteFetch",
    {
        "requests": "int",
        "bytes_fetched": "int",
        "object_size": "int",
    },
)
RemoteFetch.__doc__ = """Network traffic for a remote input."""


_DiagnosticDetails = TypedDict(  # noqa: UP013 - serialized report fields
    "_DiagnosticDetails",
    {
        "diagnostics": "list[Diagnostic]",
        "exemplars": "list[OutOfRangeExemplar]",
        "context": "dict[str, Any] | None",
        "repair_kind": "str",
        "cap": "int",
        "name": "str",
        "data_type": "str",
        "reason": "dict[str, Any]",
        "machine": "int | None",
        "limit": "int",
        "input": "str",
        "total": "int",
        "levels": "list[int]",
        "gsd_min": "float",
        "gsd_max": "float",
        "input_features": "int",
        "first_written_gsd": "float",
        "first_written_zoom": "int | None",
    },
    total=False,
)
_DiagnosticDetails.__doc__ = "Variant-specific fields of a serialized core diagnostic."


_DiagnosticRequired = TypedDict(  # noqa: UP013 - serialized report fields
    "_DiagnosticRequired",
    {"kind": "str", "count": "int"},
)


class Diagnostic(_DiagnosticDetails, _DiagnosticRequired):
    """One typed diagnostic aggregated by the core operation."""


SourceLoss = TypedDict(  # noqa: UP013 - serialized report fields
    "SourceLoss",
    {
        "id": "int",
        "feature_id": "int | None",
        "zoom": "int | None",
        "cause": "str",
    },
)
SourceLoss.__doc__ = "An absent source row, with its optional exported MVT feature ID."


class SpilledSourceLosses(TypedDict):
    """An exact loss report that exceeds the Python memory allowance.

    `count` is the number of losses. `path` names a JSON Lines file with
    one SourceLoss per line, in zoom and source-row order. It uses
    `spill_dir` when supplied and survives the call. The caller owns
    the file and must delete it after reading. Small reports retain
    list[SourceLoss].
    """

    count: int
    path: str


OverviewReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "OverviewReport",
    {
        "diagnostics": "list[Diagnostic]",
        "source_losses": "list[SourceLoss] | SpilledSourceLosses",
        "input_rows": "int",
        "source_row_ranges": "list[tuple[int, int]]",
        "mode": "Literal['duplicating', 'partitioning']",
        "levels": "list[LevelReport]",
        "skipped_empty_levels": "list[SkippedLevel]",
        "input_features": "int",
        "total_rows": "int",
        "total_vertices": "int",
        "total_compressed_bytes": "int",
        "row_groups_total": "int",
        "row_groups_read": "int",
        "antimeridian_suspect_features": "int",
        "out_of_range_features": "int",
        "unprojectable_features": "int",
        "crs_assumed_wgs84": "bool",
        "out_of_range_exemplars": "list[OutOfRangeExemplar]",
        "wrapped_features": "int",
        "duration_secs": "float",
        "remote_fetch": "RemoteFetch | None",
        "effective_max_row_group_size": "int | None",
    },
)
OverviewReport.__doc__ = """Overview output counts, sizes, and input diagnostics.

Attributes:
    skipped_empty_levels: Planned levels with nothing visible.
    row_groups_read: Row groups selected by `bbox` and `filter`.
    antimeridian_suspect_features: Features wider than 180° of
        longitude, usually a broken antimeridian crossing.
    out_of_range_features: Features dropped or clipped because
        their coordinates are outside the valid range. Longitude
        past ±180° but within ±540° wraps and is not counted.
    unprojectable_features: Features beyond ±85.05° latitude, which
        Web Mercator cannot tile.
    wrapped_features: Features past ±180° longitude but within
        ±540°, drawn wrapped across the antimeridian. Expected for
        0..360 data; otherwise the declared CRS may be wrong. Not a
        loss.
    crs_assumed_wgs84: True when the input declared no CRS and
        lon/lat WGS84 was assumed. If the coordinates are not
        lon/lat degrees, the output is misplaced.
    effective_max_row_group_size: The row-group cap actually used,
        when it had to be raised above `row_group_size` to keep the
        file under Parquet's row-group limit. None otherwise."""


ZoomReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "ZoomReport",
    {
        "zoom": "int",
        "level": "int",
        "level_feature_count": "int",
        "tile_count": "int",
        "tile_feature_count": "int",
        "oversized_tiles": "int",
        "encode_dropped_features": "int",
        "encode_quantized_features": "int",
        "merged_features": "int",
        "valve_dropped_features": "int",
        "max_valve_passes": "int",
        "valve_passes": "int",
        "fill_polygons": "int",
        "fill_vertices": "int",
        "fill_shed": "int",
    },
)
ZoomReport.__doc__ = """Tile and feature counts for one exported zoom."""


SkippedPropertyColumn = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "SkippedPropertyColumn",
    {
        "name": "str",
        "data_type": "str",
    },
)
SkippedPropertyColumn.__doc__ = (
    """A property column omitted because MVT cannot encode its type."""
)


ExportReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "ExportReport",
    {
        "diagnostics": "list[Diagnostic]",
        "source_losses": "list[SourceLoss] | SpilledSourceLosses",
        "mode": "str",
        "min_zoom": "int",
        "max_zoom": "int",
        "zooms": "list[ZoomReport]",
        "total_tiles": "int",
        "total_tile_features": "int",
        "oversized_tiles": "int",
        "merged_features": "int",
        "valve_dropped_features": "int",
        "encode_dropped_features": "int",
        "encode_quantized_features": "int",
        "skipped_property_columns": "list[SkippedPropertyColumn]",
        "duration_secs": "float",
    },
)
ExportReport.__doc__ = """Exported tile counts and encoding diagnostics.

Attributes:
    oversized_tiles: Tiles that exceeded a size limit and triggered
        size reduction.
    merged_features: Tile features merged into a neighbor to fit a
        size limit (`size_valve="merge"`).
    valve_dropped_features: Tile features dropped to fit a size
        limit.
    encode_dropped_features: Features removed by clipping. A
        nonzero count logs a warning for lost content.
    encode_quantized_features: Features that collapsed at the tile
        extent, usually harmless edge slivers.
    skipped_property_columns: Columns MVT cannot encode, such as
        binary. Struct, list, and map columns become JSON strings."""


ConvertReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "ConvertReport",
    {
        "convert": "OverviewReport",
        "export": "ExportReport",
    },
)
ConvertReport.__doc__ = """The combined report of a one-shot `convert()` run.

Both halves of the chain, keyed like the CLI's `tiles --report`
JSON: the overview conversion and the PMTiles export."""


PyramidBandReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "PyramidBandReport",
    {
        "layer": "str",
        "min_zoom": "int",
        "max_zoom": "int",
        "tiles": "int",
    },
)
PyramidBandReport.__doc__ = """Tiles one band contributed to a pyramid."""


PublicationMetadata = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "PublicationMetadata",
    {
        "name": "str | None",
        "description": "str | None",
        "attribution": "str | None",
    },
)
PublicationMetadata.__doc__ = """An archive's `name`, `description` and `attribution`.

Attributes:
    name: The tileset name, or `None` when the archive has none.
    description: The description, or `None`.
    attribution: The attribution text or HTML, or `None`."""


PyramidReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "PyramidReport",
    {
        "diagnostics": "list[Diagnostic]",
        "total_tiles": "int",
        "bands": "list[PyramidBandReport]",
        "skipped_tiles": "int",
        "bands_without_bounds": "list[str]",
        "metadata": "PublicationMetadata",
        "metadata_conflicts": "list[str]",
        "bands_without_metadata": "list[str]",
    },
)
PyramidReport.__doc__ = """What `pyramid()` merged.

Attributes:
    bands: One entry per band, by zoom range, coarsest first.
    skipped_tiles: Tiles in a pre-tiled band at zooms outside its
        declared range, and so left out. Expected when one archive
        is split across several bands.
    bands_without_bounds: Bands whose archive declared no bounds,
        and so did not widen the merged archive's bounds.
    metadata: The `name`, `description` and `attribution` written:
        the keywords, over what the PMTiles bands carry.
    metadata_conflicts: Fields (`"name"`, `"description"`) the
        PMTiles bands disagreed on, and so left out.
    bands_without_metadata: PMTiles bands whose metadata is not
        valid JSON, and so passed on no credit."""


DecodeReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "DecodeReport",
    {
        "tiles_read": "int",
        "features_written": "int",
        "features_skipped": "int",
        "layers": "list[str]",
        "zoom_range": "tuple[int, int] | None",
        "elapsed_secs": "float",
    },
)
DecodeReport.__doc__ = """What `decode()` read and wrote.

Attributes:
    tiles_read: Tiles read, after the zoom filter.
    features_written: Feature rows written.
    features_skipped: Features whose geometry decoded to nothing,
        such as a polygon collapsed at the tile extent.
    layers: MVT layers read, after the layer filter, sorted.
    zoom_range: Coarsest and finest zoom written, or `None` when no
        feature matched the filters.
    elapsed_secs: Wall-clock seconds."""


ValidationCheck = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "ValidationCheck",
    {
        "name": "str",
        "passed": "bool",
        "message": "str",
    },
)
ValidationCheck.__doc__ = """The result of one conformance check."""


ValidationReport = TypedDict(  # noqa: UP013 - keys describe serialized report data
    "ValidationReport",
    {
        "valid": "bool",
        "checks": "list[ValidationCheck]",
    },
)
ValidationReport.__doc__ = """Validation results for an overview file.

Attributes:
    valid: `True` when every check passed."""


ConfigError = _tylertoo.ConfigError
"""An option value or combination refused before any input is read.

A subclass of `ValueError`. The rules are the core library's, the same
ones the CLI enforces: conflicting options, out-of-range values, an
existing directory or missing spill directory, an option a function
cannot honour.
"""


_defaults = _tylertoo._defaults


# doctest does not discover native callables.
# Use their original examples.
__test__ = {
    function.__name__: function.__doc__ or ""
    for function in (convert, decode, export_pmtiles, overview, pyramid, validate)
}
