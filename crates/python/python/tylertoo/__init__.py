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

import os
from collections.abc import Sequence
from typing import Any, Literal, TypedDict, cast

from tylertoo import _tylertoo

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


class LevelReport(TypedDict):
    """Counts and sizes for one overview level."""

    level: int
    gsd: float
    zoom: int | None
    feature_count: int
    vertex_count: int
    uncompressed_bytes: int
    compressed_bytes: int


class SkippedLevel(TypedDict):
    """A planned level omitted because no features are visible."""

    planned_level: int
    gsd: float
    zoom: int | None


class OutOfRangeExemplar(TypedDict):
    """Input location of a feature with out-of-range coordinates.

    Attributes:
        part: Input file index, or `None` for a single file.
        axis: `"x"` and `"y"` indicate Web Mercator (`EPSG:3857`) input.
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


class _DiagnosticDetails(TypedDict, total=False):
    """Variant-specific fields of a serialized core diagnostic."""

    exemplars: list[OutOfRangeExemplar]
    context: dict[str, Any] | None
    repair_kind: str
    cap: int
    name: str
    data_type: str
    reason: dict[str, Any]
    machine: int | None
    limit: int
    input: str
    total: int
    levels: list[int]
    gsd_min: float
    gsd_max: float
    input_features: int
    first_written_gsd: float
    first_written_zoom: int | None


class Diagnostic(_DiagnosticDetails):
    """One aggregated event from this operation."""

    kind: str
    count: int


class SourceLoss(TypedDict):
    """An absent source row or exported feature ID."""

    id: int
    zoom: int | None
    cause: str


class OverviewReport(TypedDict):
    """Overview output counts, sizes, and input diagnostics.

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
            file under Parquet's row-group limit. None otherwise.
    """

    diagnostics: list[Diagnostic]
    source_losses: list[SourceLoss]
    input_rows: int
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
    crs_assumed_wgs84: bool
    out_of_range_exemplars: list[OutOfRangeExemplar]
    wrapped_features: int
    duration_secs: float
    remote_fetch: RemoteFetch | None
    effective_max_row_group_size: int | None


class ZoomReport(TypedDict):
    """Tile and feature counts for one exported zoom."""

    zoom: int
    level: int
    level_feature_count: int
    tile_count: int
    tile_feature_count: int
    oversized_tiles: int
    encode_dropped_features: int
    encode_quantized_features: int
    merged_features: int
    valve_dropped_features: int
    max_valve_passes: int
    valve_passes: int
    fill_polygons: int
    fill_vertices: int
    fill_shed: int


class SkippedPropertyColumn(TypedDict):
    """A property column omitted because MVT cannot encode its type."""

    name: str
    data_type: str


class ExportReport(TypedDict):
    """Exported tile counts and encoding diagnostics.

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
            binary. Struct, list, and map columns become JSON strings.
    """

    diagnostics: list[Diagnostic]
    source_losses: list[SourceLoss]
    mode: str
    min_zoom: int
    max_zoom: int
    zooms: list[ZoomReport]
    total_tiles: int
    total_tile_features: int
    oversized_tiles: int
    merged_features: int
    valve_dropped_features: int
    encode_dropped_features: int
    encode_quantized_features: int
    skipped_property_columns: list[SkippedPropertyColumn]
    duration_secs: float


class ConvertReport(TypedDict):
    """The combined report of a one-shot `convert()` run.

    Both halves of the chain, keyed like the CLI's `tiles --report`
    JSON: the overview conversion and the PMTiles export.
    """

    convert: OverviewReport
    export: ExportReport


class PyramidBandReport(TypedDict):
    """Tiles one band contributed to a pyramid."""

    layer: str
    min_zoom: int
    max_zoom: int
    tiles: int


class PublicationMetadata(TypedDict):
    """An archive's `name`, `description` and `attribution`.

    Attributes:
        name: The tileset name, or `None` when the archive has none.
        description: The description, or `None`.
        attribution: The attribution text or HTML, or `None`.
    """

    name: str | None
    description: str | None
    attribution: str | None


class PyramidReport(TypedDict):
    """What `pyramid()` merged.

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
            valid JSON, and so passed on no credit.
    """

    diagnostics: list[Diagnostic]
    total_tiles: int
    bands: list[PyramidBandReport]
    skipped_tiles: int
    bands_without_bounds: list[str]
    metadata: PublicationMetadata
    metadata_conflicts: list[str]
    bands_without_metadata: list[str]


class DecodeReport(TypedDict):
    """What `decode()` read and wrote.

    Attributes:
        tiles_read: Tiles read, after the zoom filter.
        features_written: Feature rows written.
        features_skipped: Features whose geometry decoded to nothing,
            such as a polygon collapsed at the tile extent.
        layers: MVT layers read, after the layer filter, sorted.
        zoom_range: Coarsest and finest zoom written, or `None` when no
            feature matched the filters.
        elapsed_secs: Wall-clock seconds.
    """

    tiles_read: int
    features_written: int
    features_skipped: int
    layers: list[str]
    zoom_range: tuple[int, int] | None
    elapsed_secs: float


class ValidationCheck(TypedDict):
    """The result of one conformance check."""

    name: str
    passed: bool
    message: str


class ValidationReport(TypedDict):
    """Validation results for an overview file.

    Attributes:
        valid: `True` when every check passed.
    """

    valid: bool
    checks: list[ValidationCheck]


ConfigError = _tylertoo.ConfigError
"""An option value or combination refused before any input is read.

A subclass of `ValueError`. The rules are the core library's, the same
ones the CLI enforces: conflicting options, out-of-range values, an
existing directory or missing spill directory, an option a function
cannot honour.
"""

_defaults = _tylertoo._defaults
"""Every keyword default, as `{function: {keyword: default}}`.

Built from the core library's defaults; the signatures below read
theirs from it, and the CLI's are checked against it.
"""

_DEFAULTS: dict[str, dict[str, Any]] = _defaults()
_OVERVIEW = _DEFAULTS["overview"]
_EXPORT = _DEFAULTS["export_pmtiles"]
_PYRAMID = _DEFAULTS["pyramid"]
_DECODE = _DEFAULTS["decode"]
_CONVERT = _DEFAULTS["convert"]


def overview(  # noqa: PLR0913 - one keyword per CLI flag, on purpose
    input: str | Sequence[str],  # noqa: A002 - public keyword name
    output: str,
    *,
    mode: Literal["duplicating", "partitioning"] = _OVERVIEW["mode"],
    min_zoom: int = _OVERVIEW["min_zoom"],
    max_zoom: int | Literal["auto"] = _OVERVIEW["max_zoom"],
    gsds: Sequence[float] | None = _OVERVIEW["gsds"],
    gsd_base: float = _OVERVIEW["gsd_base"],
    sort_key: str | None = _OVERVIEW["sort_key"],
    magnitude_ladder: str | None = _OVERVIEW["magnitude_ladder"],
    ladder_step: int = _OVERVIEW["ladder_step"],
    entry_zoom: str | None = _OVERVIEW["entry_zoom"],
    sort_direction: Literal["desc", "asc"] = _OVERVIEW["sort_direction"],
    class_rank_column: str | None = _OVERVIEW["class_rank_column"],
    class_ranks: dict[str, float] | None = _OVERVIEW["class_ranks"],
    class_rank_unknown: float | None = _OVERVIEW["class_rank_unknown"],
    no_auto_rank: bool = _OVERVIEW["no_auto_rank"],
    verbatim: bool = _OVERVIEW["verbatim"],
    simplify_factor: float | None = _OVERVIEW["simplify_factor"],
    collapse: bool = _OVERVIEW["collapse"],
    collapse_square: bool = _OVERVIEW["collapse_square"],
    no_collapse: bool = _OVERVIEW["no_collapse"],
    representation: str | None = _OVERVIEW["representation"],
    cascade: bool = _OVERVIEW["cascade"],
    point_thinning: float | None = _OVERVIEW["point_thinning"],
    line_thinning: float | None = _OVERVIEW["line_thinning"],
    polygon_thinning: float | None = _OVERVIEW["polygon_thinning"],
    line_visibility: float | None = _OVERVIEW["line_visibility"],
    polygon_visibility: float | None = _OVERVIEW["polygon_visibility"],
    drop_rate: float = _OVERVIEW["drop_rate"],
    drop_gamma: float = _OVERVIEW["drop_gamma"],
    density_drop: bool | None = _OVERVIEW["density_drop"],
    drop_lines: bool = _OVERVIEW["drop_lines"],
    drop_polygons: bool = _OVERVIEW["drop_polygons"],
    cluster: bool = _OVERVIEW["cluster"],
    accumulate_attributes: dict[str, Literal["sum", "max", "min", "mean"]]
    | None = _OVERVIEW["accumulate_attributes"],
    coalesce_lines: bool | None = _OVERVIEW["coalesce_lines"],
    coalesce_snap: float = _OVERVIEW["coalesce_snap"],
    coalesce_junction_angle: float = _OVERVIEW["coalesce_junction_angle"],
    coalesce_max_level_rows: int = _OVERVIEW["coalesce_max_level_rows"],
    include_properties: Sequence[str] | None = _OVERVIEW["include_properties"],
    exclude_properties: Sequence[str] | None = _OVERVIEW["exclude_properties"],
    exclude_all_properties: bool = _OVERVIEW["exclude_all_properties"],
    cogp_compat: bool = _OVERVIEW["cogp_compat"],
    row_group_size: int = _OVERVIEW["row_group_size"],
    row_group_size_policy: Literal["constant", "zoom-scaled"] = _OVERVIEW[
        "row_group_size_policy"
    ],
    full_column_stats: bool = _OVERVIEW["full_column_stats"],
    streaming: bool = _OVERVIEW["streaming"],
    read_batch_size: int = _OVERVIEW["read_batch_size"],
    bbox: tuple[float, float, float, float] | None = _OVERVIEW["bbox"],
    filter: str | None = _OVERVIEW["filter"],  # noqa: A002 - public keyword name
    profile: Literal["auto", "speed", "bounded"] = _OVERVIEW["profile"],
    in_flight_batches: int = _OVERVIEW["in_flight_batches"],
    read_workers: int = _OVERVIEW["read_workers"],
    spill_dir: str | os.PathLike[str] | None = _OVERVIEW["spill_dir"],
    save_plan: str | os.PathLike[str] | None = _OVERVIEW["save_plan"],
    plan: str | os.PathLike[str] | None = _OVERVIEW["plan"],
) -> OverviewReport:
    """Build a GeoParquet overview file: one generalized level per zoom.

    Args:
        input: GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
            (`EPSG:3857`). Pass a file, directory, glob, `s3://`,
            `gs://`, or `https://` URL or prefix, or a list of files.
        output: Overview file to write.
        mode: `"duplicating"` makes each level a complete map;
            `"partitioning"` stores each feature once.
        min_zoom: Coarsest zoom.
        max_zoom: Finest zoom, or `"auto"` to estimate it (at most 16).
        gsds: Per-level GSDs in meters, coarse to fine, instead of
            zooms.
        gsd_base: Pixels per tile edge when mapping zooms to GSDs. A
            larger base keeps more detail.
        sort_key: Numeric column used to choose each cell's winner.
        magnitude_ladder: Column whose values set each feature's first
            zoom, largest first.
        ladder_step: Zooms between `magnitude_ladder` values.
        entry_zoom: First zoom for listed values of a column, as
            `"COLUMN:VALUE=ZOOM,VALUE=ZOOM"`, such as
            `"density:5000=4,1000=6"`. Unlisted values use the normal
            visibility gate. Not with `magnitude_ladder`.
        sort_direction: Whether larger or smaller `sort_key` values win.
        class_rank_column: String column ranked by `class_ranks`.
        class_ranks: Finite priority per class; higher values win.
        class_rank_unknown: Priority of unlisted classes, or `None` to
            rank them last.
        no_auto_rank: Skip automatic ranking of Overture roads and
            places.
        verbatim: Turn off thinning, visibility gates, simplification,
            the density budget, and line coalescing, for pre-aggregated
            or pre-levelled input. Sets the defaults of the options
            below to off; an option you pass still applies.
        simplify_factor: Simplification tolerance, in GSDs, or `None`
            for 1 (0 with `verbatim`).
        collapse: Represent small polygons as points instead of the
            default placeholder squares.
        collapse_square: Represent small polygons as placeholder
            squares. Squares are the default except with an entry-zoom
            ladder, which uses points unless you set this option.
        no_collapse: Drop small polygons instead of using placeholders.
            This was the default before #745. Output is smaller, but
            polygon coverages have holes at coarse zooms.
        representation: Geometry kind per zoom band, such as
            `"0-7:point,8-14:geom"`.
        cascade: Speed up simplification by deriving each level from
            the next finer one.
        point_thinning: Point grid cell size in GSDs, or `None` for 4
            (16 with `cluster`, 0 with `verbatim`).
        line_thinning: Line grid cell size in GSDs, or `None` for 1
            (0 with `verbatim`).
        polygon_thinning: Polygon grid cell size in GSDs, or `None` for
            0 (off).
        line_visibility: Smallest line bounding-box diagonal kept, in
            GSDs, or `None` for 2 (0 with `verbatim`).
        polygon_visibility: Smallest polygon bounding-box diagonal kept,
            in GSDs, or `None` for 2 (0 with `verbatim`).
        drop_rate: Ratio between feature budgets of adjacent levels.
        drop_gamma: How strongly the budget spares sparse areas.
        density_drop: Apply the per-level feature budget, or `None` for
            on (off with `verbatim`). It thins points only unless
            `drop_lines` or `drop_polygons` is set.
        drop_lines: Apply the feature budget to lines too.
        drop_polygons: Apply the feature budget to polygons too, which
            leaves holes in a polygon coverage.
        cluster: Merge each cell's points and add `point_count`.
        accumulate_attributes: Aggregation method for each column in
            a cluster.
        coalesce_lines: Join touching same-class lines at coarse levels,
            or `None` for on (off with `verbatim`). `True` is refused in
            partitioning mode.
        coalesce_snap: Largest gap between joined line ends, in GSDs.
        coalesce_junction_angle: Largest turn through a junction, in
            degrees. Set `0` to stop at every junction.
        coalesce_max_level_rows: Skip joining above this line count per
            level to bound memory.
        include_properties: Keep only these property columns and skip
            reading the rest. Columns other options read, such as
            `sort_key`, must stay in the list. `[]` keeps none.
        exclude_properties: Drop these property columns.
            `include_properties` overrides it.
        exclude_all_properties: Drop every property column, writing
            geometry only. `include_properties` overrides it.
        cogp_compat: Write the third-party `cogp` footer key for readers
            of that overview format.
        row_group_size: Maximum rows per output row group.
        row_group_size_policy: `"constant"`, or `"zoom-scaled"` to
            double the row-group cap per zoom coarser than the finest.
        full_column_stats: Keep Parquet statistics for every column.
        streaming: Read the input in
            [batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
        read_batch_size: Rows per
            [read batch](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
        bbox: Keep features that intersect this lon/lat box, also one
            ±360° wrap away, as for 0..360 data.
        filter: SQL-style predicate, such as `"confidence > 0.8"`.
        profile: [Memory profile](https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles):
            `"speed"` holds output in RAM, `"bounded"` uses disk, and
            `"auto"` chooses per run.
        in_flight_batches: [Concurrent batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency),
            or `0` to size from the CPU count.
        read_workers: [Reader threads](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency),
            or `0` to size from the CPU count.
        spill_dir: [Staging directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files)
            for remote input.
        save_plan: Save the convert plan (pass 1 and the level
            assignment) here and continue converting.
        plan: Reuse a saved convert plan, skipping pass 1. Its
            fingerprint must match this run's version, options and
            inputs.

    Returns:
        Counts and sizes for each level written, with input diagnostics.

    Raises:
        ConfigError: Invalid or conflicting options, refused before any
            input is read.
        ValueError: A missing or mistyped column, a property selection
            the input cannot satisfy, an unsupported projection, or
            almost nothing to tile.
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
    layer_name: str = _EXPORT["layer_name"],
    tile_buffer: int = _EXPORT["tile_buffer"],
    extent: int = _EXPORT["extent"],
    tile_size_limit: int | Literal["auto"] | None = _EXPORT["tile_size_limit"],
    simple_clip_fastpath: bool = _EXPORT["simple_clip_fastpath"],
    partition_wave: int = _EXPORT["partition_wave"],
    feature_order: Literal["input"] | str = _EXPORT["feature_order"],
    min_zoom: int | None = _EXPORT["min_zoom"],
    include_properties: Sequence[str] | None = _EXPORT["include_properties"],
    exclude_properties: Sequence[str] | None = _EXPORT["exclude_properties"],
    exclude_all_properties: bool = _EXPORT["exclude_all_properties"],
    feature_id: str | None = _EXPORT["feature_id"],
    spill_dir: str | os.PathLike[str] | None = _EXPORT["spill_dir"],
    name: str | None = _EXPORT["name"],
    description: str | None = _EXPORT["description"],
    attribution: str | None = _EXPORT["attribution"],
    size_valve: Literal["merge", "drop"] = _EXPORT["size_valve"],
    max_compressed_tile_size: int | Literal["auto"] | None = _EXPORT[
        "max_compressed_tile_size"
    ],
    coverage_fill: bool = _EXPORT["coverage_fill"],
    tile_range: str | None = _EXPORT["tile_range"],
    zoom_ceiling: int | None = _EXPORT["zoom_ceiling"],
) -> ExportReport:
    """Export an overview file to a PMTiles archive, one zoom per level.

    Args:
        input: Overview file from `overview()`.
        output: PMTiles archive to write.
        layer_name: MVT layer name.
        tile_buffer: Tile edge buffer in pixels, at most 256.
        extent: Tile resolution in MVT units.
        tile_size_limit: Maximum raw (uncompressed) tile size in
            bytes, a backstop behind `max_compressed_tile_size`;
            `"auto"` is 2 MiB, and `0` or `None` is no limit.
        simple_clip_fastpath: Use faster clipping for simple polygons.
            `False` gives byte-stable output.
        partition_wave: [Partitions in memory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves)
            at once, or `0` to size from CPUs and RAM.
        feature_order: Paint order within a tile: `"input"`, or a
            property name with an optional `:asc` or `:desc`.
        min_zoom: Coarsest zoom to declare when the coarsest levels are
            empty.
        include_properties: Keep only these properties in the tiles.
            The overview file keeps every column, and the
            `feature_order` column must stay. `[]` keeps none.
        exclude_properties: Drop these properties from the tiles.
            `include_properties` overrides it.
        exclude_all_properties: Drop every property, writing
            geometry-only tiles. `include_properties` overrides it.
        feature_id: Integer column to use as the MVT feature ID.
        spill_dir: [Spill directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files)
            used when memory runs short.
        name: Human-readable tileset name, stored in the metadata.
        description: Description of the tileset, stored in the metadata.
        attribution: Attribution text or HTML stored in the archive
            metadata; MapLibre shows it in its attribution control.
            Each value is stored as given, and `""` is kept as `""`.
        size_valve: How to reduce a tile that exceeds its limit:
            `"merge"` merges the densest lines and polygons into a
            neighbor, which keeps its own attributes; `"drop"` removes
            features, leaving holes. Point tiles use feature dropping in
            both modes.
        max_compressed_tile_size: Maximum gzip-compressed tile size
            in bytes, like tippecanoe's `-M`; `"auto"` is 500 KiB, and
            `0` or `None` is no limit.
        coverage_fill: Experimental: under each tile's features, draw
            the area its child tiles cover, so a polygon coverage shows
            no holes at coarse zooms. Holds the finest level's geometry
            in memory. See `docs/OVERVIEW_TUNING.md`.
        tile_range: Export only the tiles in this tile-id range,
            `"LO..HI"`: one shard of a sharded build.
        zoom_ceiling: Export only the tiles at or below this zoom: the
            coarse job of a sharded build.

    Returns:
        Tile counts for each zoom written, with encoding diagnostics.

    Raises:
        ConfigError: An option out of range or refused, such as
            `tile_buffer` past 256, `extent=0`, a missing `spill_dir`,
            or metadata too large for the 16 MiB archive metadata.
        ValueError: A property selection the overview file cannot
            satisfy.
        RuntimeError: Not an overview file, or reading or writing
            failed.

    Example:
        >>> report = export_pmtiles("overview.parquet", "out.pmtiles")
        >>> report["max_zoom"]
        10
        >>> credited = export_pmtiles(
        ...     "overview.parquet",
        ...     "credited.pmtiles",
        ...     attribution="\u00a9 Example contributors",
        ... )
        >>> credited["zooms"] == report["zooms"]
        True
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("ExportReport", _tylertoo.export_pmtiles(**locals()))


def pyramid(  # noqa: PLR0913 - one keyword per CLI flag, on purpose
    bands: Sequence[str],
    output: str,
    *,
    generalize: bool = _PYRAMID["generalize"],
    tile_size_limit: int | Literal["auto"] | None = _PYRAMID["tile_size_limit"],
    feature_order: Literal["input"] | str = _PYRAMID["feature_order"],
    work_dir: str | os.PathLike[str] | None = _PYRAMID["work_dir"],
    allow_missing_zooms: bool = _PYRAMID["allow_missing_zooms"],
    name: str | None = _PYRAMID["name"],
    description: str | None = _PYRAMID["description"],
    attribution: str | None = _PYRAMID["attribution"],
    max_compressed_tile_size: int | Literal["auto"] | None = _PYRAMID[
        "max_compressed_tile_size"
    ],
    size_valve: Literal["merge", "drop"] = _PYRAMID["size_valve"],
    coverage_fill: bool = _PYRAMID["coverage_fill"],
    simple_clip_fastpath: bool = _PYRAMID["simple_clip_fastpath"],
    tile_buffer: int = _PYRAMID["tile_buffer"],
    extent: int = _PYRAMID["extent"],
    partition_wave: int = _PYRAMID["partition_wave"],
    feature_id: str | None = _PYRAMID["feature_id"],
    include_properties: Sequence[str] | None = _PYRAMID["include_properties"],
    exclude_properties: Sequence[str] | None = _PYRAMID["exclude_properties"],
    exclude_all_properties: bool = _PYRAMID["exclude_all_properties"],
) -> PyramidReport:
    """Build one PMTiles archive from per-zoom-band inputs.

    Each band names a zoom range, an input, and an MVT layer. Bands of
    different layers may share zooms, giving several layers per tile;
    bands of one layer must not overlap. The output is overwritten if
    it exists.

    Args:
        bands: Band specs as `"LO-HI:INPUT"` or `"LO-HI:INPUT:LAYER"`,
            such as `"0-7:countries.parquet:admin"`. `INPUT` is a
            GeoParquet source or a PMTiles archive for zooms `LO` to
            `HI`; `LAYER` defaults to the input's file stem. See the
            [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
            for the `LO-HI=INPUT=LAYER` form.
        output: PMTiles archive to write.
        generalize: Generalize each GeoParquet band, as `overview()`
            does. By default each band is tiled verbatim, keeping every
            feature, for input already made for its zooms.
        tile_size_limit: Maximum raw tile size in bytes for bands tiled
            here; `"auto"`, `0` and `None` are no limit, so every
            feature is kept.
        feature_order: Paint order within a tile for GeoParquet bands;
            see `export_pmtiles()`. A band without that column keeps
            input order, with a warning.
        work_dir: Directory for temporary per-band files, deleted after
            the build, or `None` for the system temporary directory.
        allow_missing_zooms: Accept a pre-tiled band with fewer zooms
            than declared. Missing zooms render empty.
        name: The output's tileset name. Unset, the PMTiles bands'
            name is kept when they agree and left out when they don't.
        description: Description for the output, inherited the same way.
        attribution: Attribution text or HTML for the output, `""`
            included. Unset, every PMTiles band's credit is kept, joined
            with `" | "`; GeoParquet bands carry none.
        max_compressed_tile_size: Maximum gzip-compressed tile size in
            bytes for bands tiled here; `"auto"`, `0` and `None` are no
            limit.
        size_valve: See `export_pmtiles()`.
        coverage_fill: See `export_pmtiles()`.
        simple_clip_fastpath: See `export_pmtiles()`.
        tile_buffer: See `export_pmtiles()`.
        extent: See `export_pmtiles()`.
        partition_wave: See `export_pmtiles()`.
        feature_id: See `export_pmtiles()`. Every GeoParquet band must
            have the column.
        include_properties: Keep only these properties in the tiles of
            GeoParquet bands. `[]` keeps none.
        exclude_properties: Drop these properties from the tiles of
            GeoParquet bands. `include_properties` overrides it.
        exclude_all_properties: Drop every property from the tiles of
            GeoParquet bands. `include_properties` overrides it.

    Returns:
        Tile counts per band, with merge diagnostics and the metadata
        written.

    Raises:
        ConfigError: An option out of range or refused.
        ValueError: A malformed band spec, overlapping bands of one
            layer, or a missing band input.
        RuntimeError: A band failed to convert, export, or merge.

    Example:
        >>> report = pyramid(
        ...     [
        ...         "8-9:buildings.parquet:coarse",
        ...         "10-12:buildings.parquet:fine",
        ...     ],
        ...     "pyramid.pmtiles",
        ... )
        >>> [band["layer"] for band in report["bands"]]
        ['coarse', 'fine']
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("PyramidReport", _tylertoo.pyramid(**locals()))


def decode(
    input: str,  # noqa: A002 - public keyword name
    output: str,
    *,
    min_zoom: int | None = _DECODE["min_zoom"],
    max_zoom: int | None = _DECODE["max_zoom"],
    layer: str | None = _DECODE["layer"],
) -> DecodeReport:
    """Decode a PMTiles archive of vector tiles back to GeoParquet.

    Writes one row per feature per tile: the tiled representation, so
    features are simplified, clipped at tile edges, and repeated at
    every zoom and in every tile they cross. The columns are `zoom`,
    `layer`, `mvt_id`, the properties in name order, then `geometry`
    as lon/lat (`EPSG:4326`) WKB. The output is overwritten if it
    exists.

    Args:
        input: PMTiles archive of vector tiles.
        output: GeoParquet file to write.
        min_zoom: Coarsest zoom to decode, or `None` for the
            archive's coarsest.
        max_zoom: Finest zoom to decode, or `None` for the archive's
            finest. Set both to one zoom to decode only that zoom.
        layer: Decode only this MVT layer, or `None` for every layer.

    Returns:
        Counts of tiles read and features written, with the layers and
        zooms found.

    Raises:
        ValueError: `min_zoom` above `max_zoom`, an archive of raster
            tiles, or a property named like a provenance column.
        RuntimeError: Not a PMTiles archive, a damaged tile, or reading
            or writing failed.

    Example:
        >>> report = decode(
        ...     "buildings.pmtiles", "decoded.parquet", min_zoom=10
        ... )
        >>> report["layers"], report["zoom_range"]
        (['overview'], (10, 10))
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("DecodeReport", _tylertoo.decode(**locals()))


def validate(file: str) -> ValidationReport:
    """Check an overview file against the overview format specification.

    Args:
        file: Overview file to check.

    Returns:
        Each check's result. Failed checks are reported without raising.

    Raises:
        RuntimeError: The file is not readable Parquet.

    Example:
        >>> validate("overview.parquet")["valid"]
        True
    """
    return cast("ValidationReport", _tylertoo.validate(file))


def convert(  # noqa: PLR0913 - one keyword per CLI flag, on purpose
    input: str,  # noqa: A002 - public keyword name
    output: str,
    min_zoom: int = _CONVERT["min_zoom"],
    max_zoom: int | Literal["auto"] = _CONVERT["max_zoom"],
    layer_name: str | None = _CONVERT["layer_name"],
    tile_size_limit: int | Literal["auto"] | None = _CONVERT["tile_size_limit"],
    simple_clip_fastpath: bool = _CONVERT["simple_clip_fastpath"],
    feature_order: Literal["input"] | str = _CONVERT["feature_order"],
    spill_dir: str | os.PathLike[str] | None = _CONVERT["spill_dir"],
    *,
    include_properties: Sequence[str] | None = _CONVERT["include_properties"],
    exclude_properties: Sequence[str] | None = _CONVERT["exclude_properties"],
    exclude_all_properties: bool = _CONVERT["exclude_all_properties"],
    size_valve: Literal["merge", "drop"] = _CONVERT["size_valve"],
    max_compressed_tile_size: int | Literal["auto"] | None = _CONVERT[
        "max_compressed_tile_size"
    ],
    coverage_fill: bool = _CONVERT["coverage_fill"],
    gsds: Sequence[float] | None = _CONVERT["gsds"],
    bbox: tuple[float, float, float, float] | None = _CONVERT["bbox"],
    name: str | None = _CONVERT["name"],
    description: str | None = _CONVERT["description"],
    attribution: str | None = _CONVERT["attribution"],
    tile_buffer: int = _CONVERT["tile_buffer"],
    extent: int = _CONVERT["extent"],
    partition_wave: int = _CONVERT["partition_wave"],
    feature_id: str | None = _CONVERT["feature_id"],
    keep_overview: str | os.PathLike[str] | None = _CONVERT["keep_overview"],
    tile_range: str | None = _CONVERT["tile_range"],
    gsd_base: float = _CONVERT["gsd_base"],
    sort_key: str | None = _CONVERT["sort_key"],
    magnitude_ladder: str | None = _CONVERT["magnitude_ladder"],
    ladder_step: int = _CONVERT["ladder_step"],
    entry_zoom: str | None = _CONVERT["entry_zoom"],
    sort_direction: Literal["desc", "asc"] = _CONVERT["sort_direction"],
    class_rank_column: str | None = _CONVERT["class_rank_column"],
    class_ranks: dict[str, float] | None = _CONVERT["class_ranks"],
    class_rank_unknown: float | None = _CONVERT["class_rank_unknown"],
    no_auto_rank: bool = _CONVERT["no_auto_rank"],
    verbatim: bool = _CONVERT["verbatim"],
    simplify_factor: float | None = _CONVERT["simplify_factor"],
    collapse: bool = _CONVERT["collapse"],
    collapse_square: bool = _CONVERT["collapse_square"],
    no_collapse: bool = _CONVERT["no_collapse"],
    representation: str | None = _CONVERT["representation"],
    cascade: bool = _CONVERT["cascade"],
    point_thinning: float | None = _CONVERT["point_thinning"],
    line_thinning: float | None = _CONVERT["line_thinning"],
    polygon_thinning: float | None = _CONVERT["polygon_thinning"],
    line_visibility: float | None = _CONVERT["line_visibility"],
    polygon_visibility: float | None = _CONVERT["polygon_visibility"],
    drop_rate: float = _CONVERT["drop_rate"],
    drop_gamma: float = _CONVERT["drop_gamma"],
    density_drop: bool | None = _CONVERT["density_drop"],
    drop_lines: bool = _CONVERT["drop_lines"],
    drop_polygons: bool = _CONVERT["drop_polygons"],
    cluster: bool = _CONVERT["cluster"],
    accumulate_attributes: dict[str, Literal["sum", "max", "min", "mean"]]
    | None = _CONVERT["accumulate_attributes"],
    coalesce_lines: bool | None = _CONVERT["coalesce_lines"],
    coalesce_snap: float = _CONVERT["coalesce_snap"],
    coalesce_junction_angle: float = _CONVERT["coalesce_junction_angle"],
    coalesce_max_level_rows: int = _CONVERT["coalesce_max_level_rows"],
    row_group_size: int = _CONVERT["row_group_size"],
    row_group_size_policy: Literal["constant", "zoom-scaled"] = _CONVERT[
        "row_group_size_policy"
    ],
    full_column_stats: bool = _CONVERT["full_column_stats"],
    streaming: bool = _CONVERT["streaming"],
    read_batch_size: int = _CONVERT["read_batch_size"],
    filter: str | None = _CONVERT["filter"],  # noqa: A002 - public keyword name
    profile: Literal["auto", "speed", "bounded"] = _CONVERT["profile"],
    in_flight_batches: int = _CONVERT["in_flight_batches"],
    read_workers: int = _CONVERT["read_workers"],
    save_plan: str | os.PathLike[str] | None = _CONVERT["save_plan"],
    plan: str | os.PathLike[str] | None = _CONVERT["plan"],
) -> ConvertReport:
    """Convert GeoParquet to PMTiles in one call, like `tylertoo tiles`.

    Runs the same core facade as `tylertoo tiles`, with the same
    options and defaults, so the chain — the removed-after-export
    intermediate overview, its free-space preflight, the option rules,
    and the declared zoom range — matches the CLI. `overview()`
    followed by `export_pmtiles()` is the same chain in two steps.
    Sharded builds (`--shard`, `--plan-only`) stay CLI-only.

    Args:
        input: See `overview()`.
        output: PMTiles archive to write.
        min_zoom: See `overview()`.
        max_zoom: See `overview()`.
        layer_name: MVT layer name, or `None` for the input's file stem.
        tile_size_limit: See `export_pmtiles()`. `"auto"` is no limit
            with `verbatim`.
        simple_clip_fastpath: See `export_pmtiles()`.
        feature_order: See `export_pmtiles()`.
        spill_dir: Scratch directory for the intermediate overview and
            the export's spill (the CLI's `--spill-dir`). The
            intermediate is at least input-sized; without this it goes
            to `$TMPDIR`, then the output's directory. Set it when
            `$TMPDIR` is small or a tmpfs. A `UserWarning` is raised
            when the chosen volume looks too small, whether or not the
            run then fails.
        include_properties: See `overview()`.
        exclude_properties: See `overview()`.
        exclude_all_properties: See `overview()`.
        size_valve: See `export_pmtiles()`.
        max_compressed_tile_size: See `export_pmtiles()`. `"auto"` is no
            limit with `verbatim`.
        coverage_fill: See `export_pmtiles()`.
        gsds: See `overview()`.
        bbox: See `overview()`.
        name: See `export_pmtiles()`.
        description: See `export_pmtiles()`.
        attribution: See `export_pmtiles()`.
        tile_buffer: See `export_pmtiles()`.
        extent: See `export_pmtiles()`.
        partition_wave: See `export_pmtiles()`.
        feature_id: See `export_pmtiles()`.
        keep_overview: Keep the intermediate overview file at this path
            instead of removing it. Its directory must exist.
        tile_range: See `export_pmtiles()`.
        gsd_base: See `overview()`.
        sort_key: See `overview()`.
        magnitude_ladder: See `overview()`.
        ladder_step: See `overview()`.
        entry_zoom: See `overview()`.
        sort_direction: See `overview()`.
        class_rank_column: See `overview()`.
        class_ranks: See `overview()`.
        class_rank_unknown: See `overview()`.
        no_auto_rank: See `overview()`.
        verbatim: See `overview()`. Also turns both tile size limits
            off unless you pass them.
        simplify_factor: See `overview()`.
        collapse: See `overview()`.
        collapse_square: See `overview()`.
        no_collapse: See `overview()`.
        representation: See `overview()`.
        cascade: See `overview()`.
        point_thinning: See `overview()`.
        line_thinning: See `overview()`.
        polygon_thinning: See `overview()`.
        line_visibility: See `overview()`.
        polygon_visibility: See `overview()`.
        drop_rate: See `overview()`.
        drop_gamma: See `overview()`.
        density_drop: See `overview()`.
        drop_lines: See `overview()`.
        drop_polygons: See `overview()`.
        cluster: See `overview()`.
        accumulate_attributes: See `overview()`.
        coalesce_lines: See `overview()`.
        coalesce_snap: See `overview()`.
        coalesce_junction_angle: See `overview()`.
        coalesce_max_level_rows: See `overview()`.
        row_group_size: See `overview()`.
        row_group_size_policy: See `overview()`.
        full_column_stats: See `overview()`.
        streaming: See `overview()`.
        read_batch_size: See `overview()`.
        filter: See `overview()`.
        profile: See `overview()`.
        in_flight_batches: See `overview()`.
        read_workers: See `overview()`.
        save_plan: See `overview()`.
        plan: See `overview()`.

    Returns:
        Both halves' reports, keyed `"convert"` and `"export"` like the
        CLI's `tiles --report` JSON.

    Raises:
        ConfigError: Invalid or conflicting options, refused before any
            input is read.
        ValueError: A missing or mistyped column, or a property
            selection the input cannot satisfy.
        MemoryError: Too little memory for this input.
        RuntimeError: Conversion or export failed.

    Example:
        >>> report = convert(
        ...     "buildings.parquet", "out.pmtiles", max_zoom=10
        ... )
        >>> sorted(report)
        ['convert', 'export']
    """
    # Every argument goes to the extension unchanged, by keyword.
    return cast("ConvertReport", _tylertoo.convert(**locals()))
