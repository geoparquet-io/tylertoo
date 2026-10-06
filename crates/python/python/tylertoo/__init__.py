"""Build GeoParquet overview files and export them to PMTiles archives.

The functions take the same options and defaults as the CLI. GSD means
ground sample distance: the meters one pixel covers at a level. The
tuning reference explains every generalization option:
<https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
"""

from __future__ import annotations

import os
from collections.abc import Sequence
from typing import Literal, TypedDict, cast

from tylertoo import _tylertoo

__all__ = [
    "ExportReport",
    "LevelReport",
    "OutOfRangeExemplar",
    "OverviewReport",
    "RemoteFetch",
    "SkippedLevel",
    "SkippedPropertyColumn",
    "ValidationCheck",
    "ValidationReport",
    "ZoomReport",
    "convert",
    "export_pmtiles",
    "overview",
    "validate",
]


class LevelReport(TypedDict):
    """One written level of an overview file."""

    level: int
    gsd: float
    zoom: int
    feature_count: int
    vertex_count: int
    uncompressed_bytes: int
    compressed_bytes: int


class SkippedLevel(TypedDict):
    """A planned level left out because nothing in it is visible."""

    planned_level: int
    gsd: float
    zoom: int


class OutOfRangeExemplar(TypedDict):
    """Where a feature with out-of-range coordinates sits in the input.

    Attributes:
        part: Input file index, or None for a single file.
        axis: "x" and "y" mean Web Mercator (`EPSG:3857`) input.
    """

    part: int | None
    row: int
    axis: Literal["lon", "lat", "x", "y"]
    value: float


class RemoteFetch(TypedDict):
    """Network traffic for a remote input."""

    requests: int
    bytes_fetched: int
    object_size: int


class OverviewReport(TypedDict):
    """What `overview()` wrote.

    Attributes:
        skipped_empty_levels: Planned levels with nothing visible.
        row_groups_read: Row groups left after `bbox` and `filter`.
        antimeridian_suspect_features: Features wider than 180° of
            longitude, usually a broken antimeridian crossing.
        out_of_range_features: Features dropped or clipped for lying
            outside the valid coordinate range.
        unprojectable_features: Features beyond ±85.05° latitude, which
            Web Mercator cannot tile.
    """

    mode: Literal["duplicating", "partitioning"]
    levels: list[LevelReport]
    skipped_empty_levels: list[SkippedLevel]
    input_features: int
    total_rows: int
    total_vertices: int
    total_compressed_bytes: int
    row_groups_total: int
    row_groups_read: int
    antimeridian_suspect_features: int
    out_of_range_features: int
    unprojectable_features: int
    out_of_range_exemplars: list[OutOfRangeExemplar]
    duration_secs: float
    remote_fetch: RemoteFetch | None


class ZoomReport(TypedDict):
    """One zoom of an exported archive."""

    zoom: int
    level: int
    level_feature_count: int
    tile_count: int
    tile_feature_count: int
    oversized_tiles: int
    encode_dropped_features: int
    encode_quantized_features: int


class SkippedPropertyColumn(TypedDict):
    """A property column MVT cannot encode, left out of the tiles."""

    name: str
    data_type: str


class ExportReport(TypedDict):
    """What `export_pmtiles()` wrote.

    Attributes:
        oversized_tiles: Tiles that shed features to fit
            `tile_size_limit`.
        encode_dropped_features: Features with nothing left after
            clipping. A count above zero means lost content and logs a
            warning.
        encode_quantized_features: Features that collapsed at the tile
            extent, usually harmless edge slivers.
        skipped_property_columns: Columns MVT cannot encode, such as
            binary. Struct, list, and map columns become JSON strings.
    """

    mode: str
    min_zoom: int
    max_zoom: int
    zooms: list[ZoomReport]
    total_tiles: int
    total_tile_features: int
    oversized_tiles: int
    encode_dropped_features: int
    encode_quantized_features: int
    skipped_property_columns: list[SkippedPropertyColumn]
    duration_secs: float


class ValidationCheck(TypedDict):
    """The result of one conformance check."""

    name: str
    passed: bool
    message: str


class ValidationReport(TypedDict):
    """What `validate()` found.

    Attributes:
        valid: True when every check passed.
    """

    valid: bool
    checks: list[ValidationCheck]


def overview(  # noqa: PLR0913 - one keyword per CLI flag, on purpose
    input: str | Sequence[str],  # noqa: A002 - public keyword name
    output: str,
    *,
    mode: Literal["duplicating", "partitioning"] = "duplicating",
    min_zoom: int = 0,
    max_zoom: int | Literal["auto"] = 6,
    gsds: Sequence[float] | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    magnitude_ladder: str | None = None,
    ladder_step: int = 1,
    sort_direction: Literal["desc", "asc"] = "desc",
    class_rank_column: str | None = None,
    class_ranks: dict[str, float] | None = None,
    class_rank_unknown: float | None = None,
    no_auto_rank: bool = False,
    simplify_factor: float = 1.0,
    collapse: bool = False,
    collapse_square: bool = False,
    representation: str | None = None,
    cascade: bool = True,
    point_thinning: float | None = None,
    line_thinning: float = 1.0,
    polygon_thinning: float = 1.0,
    line_visibility: float = 2.0,
    polygon_visibility: float = 2.0,
    drop_rate: float = 1.65,
    drop_gamma: float = 1.5,
    density_drop: bool = True,
    cluster: bool = False,
    accumulate_attributes: dict[str, Literal["sum", "max", "min", "mean"]]
    | None = None,
    coalesce_lines: bool = True,
    coalesce_snap: float = 1.0,
    coalesce_junction_angle: float = 0.0,
    coalesce_max_level_rows: int = 2_000_000,
    cogp_compat: bool = False,
    row_group_size: int = 10_000,
    full_column_stats: bool = False,
    streaming: bool = True,
    read_batch_size: int = 8192,
    bbox: tuple[float, float, float, float] | None = None,
    filter: str | None = None,  # noqa: A002 - public keyword name
    profile: Literal["auto", "speed", "bounded"] = "auto",
    in_flight_batches: int = 0,
    read_workers: int = 0,
    spill_dir: str | os.PathLike[str] | None = None,
) -> OverviewReport:
    """Build a GeoParquet overview file: one generalized level per zoom.

    Args:
        input: GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
            (`EPSG:3857`). Pass a file, directory, glob, `s3://`,
            `gs://`, or `https://` URL or prefix, or a list of files.
        output: Overview file to write.
        mode: `"duplicating"` makes each level a complete map, and
            `"partitioning"` stores each feature once.
        min_zoom: Coarsest zoom.
        max_zoom: Finest zoom, or `"auto"` to estimate it (at most 16).
        gsds: Per-level GSDs in meters, coarse to fine, instead of
            zooms.
        gsd_base: Pixels per tile edge when mapping zooms to GSDs. A
            larger base keeps more detail.
        sort_key: Numeric column that decides which feature wins a cell.
        magnitude_ladder: Column whose values set each feature's first
            zoom, largest first.
        ladder_step: Zooms between `magnitude_ladder` values.
        sort_direction: Whether larger or smaller `sort_key` values win.
        class_rank_column: String column ranked by `class_ranks`.
        class_ranks: Finite priority per class, where higher wins.
        class_rank_unknown: Priority of unlisted classes, or None to
            rank them last.
        no_auto_rank: Skip automatic ranking of Overture roads and
            places.
        simplify_factor: Simplification tolerance, in GSDs.
        collapse: Keep too-small polygons as points.
        collapse_square: Keep too-small polygons as small squares.
        representation: Geometry kind per zoom band, such as
            `"0-7:point,8-14:geom"`.
        cascade: Simplify each level from the next finer one, which is
            faster.
        point_thinning: Point grid cell size in GSDs, or None for 4
            (16 with `cluster`).
        line_thinning: Line grid cell size in GSDs.
        polygon_thinning: Polygon grid cell size in GSDs.
        line_visibility: Smallest line bounding-box diagonal kept, in
            GSDs.
        polygon_visibility: Smallest polygon bounding-box diagonal kept,
            in GSDs.
        drop_rate: Ratio between feature budgets of adjacent levels.
        drop_gamma: How strongly the budget spares sparse areas.
        density_drop: Apply the per-level feature budget.
        cluster: Merge each cell's points into one, with a
            `point_count`.
        accumulate_attributes: Columns to aggregate per cluster, and
            how.
        coalesce_lines: Join touching same-class lines at coarse levels.
        coalesce_snap: Largest gap between joined line ends, in GSDs.
        coalesce_junction_angle: Largest turn through a junction, in
            degrees. Set 0 to stop at every junction.
        coalesce_max_level_rows: Skip joining on a level with more lines
            than this, to bound memory.
        cogp_compat: Also write the third-party `cogp` footer key, for
            readers of that overview format.
        row_group_size: Most rows per output row group.
        full_column_stats: Keep Parquet statistics for every column.
        streaming: Read the input in batches. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>.
        read_batch_size: Rows per read batch. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>.
        bbox: Keep features that intersect this lon/lat box.
        filter: SQL-style predicate, such as `"confidence > 0.8"`.
        profile: Hold output in RAM, on disk, or choose per run. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>.
        in_flight_batches: Batches processed at once, or 0 to size from
            the CPU count. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>.
        read_workers: Reader threads, or 0 to size from the CPU count.
            See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>.
        spill_dir: Directory for staging a remote input. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>.

    Returns:
        A report on each level written.

    Raises:
        ValueError: Invalid or conflicting options, a missing or
            mistyped column, an unsupported projection, or almost
            nothing to tile.
        MemoryError: Too little memory for this input.
        RuntimeError: Reading or writing failed.

    Example:
        >>> report = overview(
        ...     "buildings.parquet", "overview.parquet", max_zoom=10
        ... )
        >>> report["input_features"]
        1000
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("OverviewReport", _tylertoo.overview(**locals()))


def export_pmtiles(  # noqa: PLR0913 - one keyword per CLI flag, on purpose
    input: str,  # noqa: A002 - public keyword name
    output: str,
    *,
    layer_name: str = "overview",
    tile_buffer: int = 8,
    extent: int = 4096,
    tile_size_limit: int | None = 512000,
    simple_clip_fastpath: bool = True,
    partition_wave: int = 0,
    feature_order: Literal["input"] | str = "input",
    min_zoom: int | None = None,
    feature_id: str | None = None,
    spill_dir: str | os.PathLike[str] | None = None,
) -> ExportReport:
    """Export an overview file to a PMTiles archive, one zoom per level.

    Args:
        input: Overview file from `overview()`.
        output: PMTiles archive to write.
        layer_name: MVT layer name.
        tile_buffer: Tile edge buffer in pixels, at most 256.
        extent: Tile resolution in MVT units.
        tile_size_limit: Largest tile in bytes, or 0 or None for no
            limit.
        simple_clip_fastpath: Clip simple polygons on a fast path. False
            gives byte-stable output.
        partition_wave: Partitions held in memory at once, or 0 to size
            it from CPUs and RAM. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>.
        feature_order: Paint order within a tile: `"input"`, or a
            property name with an optional `:asc` or `:desc`.
        min_zoom: Coarsest zoom to declare when the coarsest levels are
            empty.
        feature_id: Integer column to write as the MVT feature id.
        spill_dir: Where spill files go when memory runs short. See
            <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>.

    Returns:
        A report on each zoom written.

    Raises:
        ValueError: `feature_order` does not parse.
        RuntimeError: Not an overview file, an option is out of range,
            or reading or writing failed.

    Example:
        >>> report = export_pmtiles("overview.parquet", "out.pmtiles")
        >>> report["max_zoom"]
        10
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("ExportReport", _tylertoo.export_pmtiles(**locals()))


def validate(file: str) -> ValidationReport:
    """Check an overview file against the overview format specification.

    Args:
        file: Overview file to check.

    Returns:
        Each check's result. A failed check shows up here instead of
        raising.

    Raises:
        RuntimeError: The file is not readable Parquet.

    Example:
        >>> validate("overview.parquet")["valid"]
        True
    """
    return cast("ValidationReport", _tylertoo.validate(file))


def convert(  # noqa: PLR0913 - mirrors the CLI's one-shot flags
    input: str,  # noqa: A002 - public keyword name
    output: str,
    min_zoom: int = 0,
    max_zoom: int | Literal["auto"] = 14,
    layer_name: str | None = None,
    tile_size_limit: int | None = 512000,
    simple_clip_fastpath: bool = True,
    feature_order: Literal["input"] | str = "input",
) -> None:
    """Convert GeoParquet to PMTiles in one call (deprecated).

    Use `overview()` and then `export_pmtiles()`, which take every
    option.

    Args:
        input: See `overview()`.
        output: PMTiles archive to write.
        min_zoom: See `overview()`.
        max_zoom: See `overview()`.
        layer_name: MVT layer name, or None for the input's file stem.
        tile_size_limit: See `export_pmtiles()`.
        simple_clip_fastpath: See `export_pmtiles()`.
        feature_order: See `export_pmtiles()`.

    Raises:
        ValueError: Invalid zoom range or options.
        MemoryError: Too little memory for this input.
        RuntimeError: The conversion or the export failed.

    Example:
        >>> convert("buildings.parquet", "out.pmtiles", max_zoom=10)
    """
    # Every argument goes to the extension unchanged, by keyword.
    _tylertoo.convert(**locals())
