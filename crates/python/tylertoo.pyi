# Type stubs for the tylertoo pyo3 extension module.
#
# Kept honest by CI: `python -m mypy.stubtest tylertoo` verifies these
# signatures against the compiled module on every PR. If you change a
# #[pyo3(signature = ...)] in crates/python/src/lib.rs, update this file.
"""tylertoo: fast GeoParquet to PMTiles converter.

Python bindings for the tylertoo Rust library. Convert GeoParquet into
multi-resolution vector overviews (COG-style vector pyramids) and export
them to PMTiles archives, with native Arrow integration and remote
byte-range reads. The primary path is `overview()` (build the overview
GeoParquet) followed by `export_pmtiles()` (render it to tiles); `convert()`
is a one-shot facade, and `validate()` checks a file against the overviews
spec.
"""

from pathlib import Path
from typing import Any

__all__ = ["convert", "export_pmtiles", "overview", "validate"]

def convert(
    input: str,
    output: str,
    min_zoom: int = 0,
    max_zoom: int = 14,
    layer_name: str | None = None,
    tile_size_limit: int | None = 512000,
    simple_clip_fastpath: bool = True,
) -> None:
    """Convert GeoParquet to PMTiles in one shot (overview facade).

    Deprecated:
        `convert()` no longer runs the removed legacy per-tile pipeline.
        It is now a thin facade that chains `overview()` (convert, with
        default knobs) into a temporary GeoParquet file and then
        `export_pmtiles()` to the requested output. For control over
        generalization quality (ranking, clustering, coalescing, thinning),
        call `overview()` and `export_pmtiles()` directly.

    The legacy keyword arguments `drop_density`, `compression`, `include`,
    `exclude`, `exclude_all`, `deterministic`, `drop_smallest_as_needed`,
    `drop_smallest_threshold` and `progress_callback` were removed with the
    legacy pipeline; passing them raises `TypeError`.

    Args:
        input: Path to input GeoParquet file (EPSG:4326 or EPSG:3857), or a
            remote URL (`s3://`, `https://`, `gs://`) read via byte-range
            requests.
        output: Path to output PMTiles file.
        min_zoom: Minimum (coarsest) zoom level. Defaults to 0.
        max_zoom: Maximum (finest) zoom level. Defaults to 14.
        layer_name: Override the MVT layer name (defaults to the input
            filename stem).
        tile_size_limit: Per-tile MVT size cap in bytes. A tile exceeding it
            sheds features in a single pass (largest-first for
            polygons/lines; a uniform spatial stride for point tiles).
            Defaults to 512000 (500 KiB, tippecanoe parity); pass 0 (or
            None) to disable the cap.
        simple_clip_fastpath: Skip the i_overlay boundary-bridge fallback for
            features whose rings are already simple (issue #239). Faster
            fine-zoom polygon export; output is render-equivalent on simple
            rings but stores them rotated to a different start vertex.
            Defaults to True; set False for byte-stable tile output.

    Returns:
        None.

    Raises:
        ValueError: Invalid zoom range or conversion options.
        RuntimeError: The conversion or export failed.

    Example:
        >>> from tylertoo import convert
        >>> convert("buildings.parquet", "buildings.pmtiles", min_zoom=0, max_zoom=14)
        >>> convert("buildings.parquet", "buildings.pmtiles", layer_name="my_layer")
    """
    ...
def overview(
    input: str | list[str],
    output: str,
    *,
    mode: str = "duplicating",
    min_zoom: int = 0,
    max_zoom: int = 6,
    gsds: list[float] | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    sort_direction: str = "desc",
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
    accumulate_attributes: dict[str, str] | None = None,
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
    filter: str | None = None,
    profile: str = "auto",
    in_flight_batches: int = 0,
    spill_dir: str | Path | None = None,
) -> dict[str, Any]:
    """Build a multi-resolution GeoParquet overview file (COG-style vector overviews).

    This is the Python equivalent of `tylertoo overview` with the full CLI
    knob surface and identical defaults. The pipeline reads a (gpio-sorted)
    GeoParquet file, thins features per level with grid cell-winner
    selection, applies the per-level density budget, simplifies geometry in
    world space, and writes a level-banded GeoParquet file validated by
    `validate()` and exportable to PMTiles by `export_pmtiles()`.

    Args:
        input: Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a
            directory or glob of partitions, a remote URL (`s3://`,
            `https://`, `gs://`), an `s3://…/` or `gs://…/` prefix (listed to
            its `.parquet` objects, sorted by key), or an explicit ordered
            `list[str]` of files/URLs (each a single file/object — no
            expansion; list order defines the dataset row order). Remote
            inputs are read via byte-range requests. With `bbox`, only the
            matching row groups of a remote input are ever downloaded.
        output: Output overview GeoParquet file.
        mode: Level materialization mode, "duplicating" (each level is a
            self-contained rendering) or "partitioning" (each feature appears
            once at its coarsest level; prefix reads). Defaults to
            "duplicating".
        min_zoom: Coarsest Web Mercator zoom of the level range. Defaults to
            0.
        max_zoom: Finest (canonical) Web Mercator zoom of the level range.
            Defaults to 6.
        gsds: Explicit per-level GSD list in meters, strictly decreasing
            coarse-to-fine. Overrides min_zoom/max_zoom.
        gsd_base: GSD tile-band base for the zoom-to-GSD mapping: gsd(z) =
            40075016.69 / base / 2^z. Larger = finer (denser) levels, smaller
            = coarser. No effect with explicit gsds. Defaults to 1024.0.
        sort_key: Numeric column used as the cell-winner priority key (higher
            wins by default; see sort_direction). Mutually exclusive with
            class_rank_column.
        sort_direction: "desc" (larger sort_key wins, default) or "asc"
            (smaller wins, e.g. rank columns where 1 is best).
        class_rank_column: String column carrying categorical classes for
            cell-winner ranking. Requires class_ranks. Mutually exclusive
            with sort_key.
        class_ranks: Map of class value to priority; higher priority wins a
            cell. Present-but-unlisted values rank below every listed value
            (but above nulls) unless class_rank_unknown overrides that.
        class_rank_unknown: Priority for present-but-unlisted class values.
            Defaults to min(class_ranks.values()) - 1.
        no_auto_rank: Disable auto-detection of well-known schemas (Overture
            roads class/road_class, Overture places confidence). Defaults to
            False.
        simplify_factor: RDP tolerance = factor * gsd (duplicating mode
            only). Lower = crisper but heavier levels; higher = cruder and
            lighter. Defaults to 1.0.
        collapse: Collapse below-visibility polygons to a representative
            point instead of dropping them. Defaults to False.
        collapse_square: Collapse below-visibility polygons to a ~1xGSD
            area-dithered placeholder square at the representative point
            (tippecanoe tiny-polygon reduction; type-preserving, so fill
            styles keep working). Mutually exclusive with collapse. Defaults
            to False.
        representation: Zoom-band representation selector: comma-separated
            "LO-HI:KIND" bands (KIND: geom, point, square), e.g.
            "0-7:point,8-14:geom". Point bands render ALL polygonal features
            as centroids; square bands emit dithered placeholder squares for
            below-tolerance polygons. Requires a zoom-range plan and
            duplicating mode. Defaults to None (all levels geom).
        cascade: Cascading simplification (duplicating mode only): simplify
            each coarser level from the next-finer level's already-simplified
            output (tippecanoe-style) and repair invalid RDP candidates via a
            boolean overlay instead of epsilon-retrying. Much faster;
            coarse-level coordinates differ slightly from the non-cascaded
            pipeline. Set False to reproduce pre-cascade output
            byte-for-byte. Defaults to True.
        point_thinning: Point thinning grid factor (cell size = factor *
            gsd). Defaults to 4.0, or 16.0 when cluster=True (absorbed points
            are summarized rather than dropped).
        line_thinning: Line thinning grid factor. Defaults to 1.0.
        polygon_thinning: Polygon thinning grid factor. Defaults to 1.0.
        line_visibility: A line is eligible at a level only if its bbox
            diagonal >= factor * gsd. Defaults to 2.0.
        polygon_visibility: Same gate for polygons. Defaults to 2.0 (retuned
            from 4.0 in the #259 coarse-zoom sweep; see corpus/SWEEPS.md
            Decision 6).
        drop_rate: Per-level density budget drop rate: each coarser level
            keeps 1/rate of the next finer level's budget. Defaults to 1.65.
        drop_gamma: Spatial-fairness strength for the density budget (1.0 =
            proportional cut; larger protects sparse neighborhoods). Defaults
            to 1.5.
        density_drop: Master switch for the per-level density budget.
            Defaults to True.
        cluster: Enable point clustering (duplicating mode only): the
            surviving point per grid cell absorbs the other points in its
            cell and the output gains a point_count INT64 column. Defaults to
            False.
        accumulate_attributes: Numeric per-cluster attribute aggregation,
            mapping column name to operator ("sum", "max", "min", "mean").
            Requires cluster=True.
        coalesce_lines: Chain touching same-class line segments into single
            "stroke" LineStrings at non-canonical levels; the output gains a
            coalesced_count INT32 column. Defaults to True. Inert in
            partitioning mode (feature-once/verbatim contract).
        coalesce_snap: Endpoint snap tolerance in GSD multiples; <= 0
            requires exact endpoint matches. Defaults to 1.0.
        coalesce_junction_angle: Junction continuation threshold in degrees;
            0 disables (junctions terminate chains). Defaults to 0.0.
        coalesce_max_level_rows: Per-level candidate-line ceiling (memory
            guard); larger levels skip coalescing with a log. Defaults to
            2_000_000.
        cogp_compat: Emit the optional COGP compatibility footer key.
            Defaults to False.
        row_group_size: Maximum output row-group size in rows (interpreted
            per level). Defaults to 10_000.
        full_column_stats: Keep full Parquet statistics on every column
            instead of suppressing high-cardinality property and geometry
            stats. Defaults to False.
        streaming: Use the two-pass bounded-memory streaming pipeline.
            Defaults to True.
        read_batch_size: Rows per Arrow read batch in the streaming pipeline.
            Defaults to 8192.
        bbox: Regional extract as `(xmin, ymin, xmax, ymax)` in EPSG:4326
            lon/lat degrees. Only features whose bbox intersects the region
            are converted; input row groups whose bbox covering statistics
            don't intersect are skipped without reading their data pages
            (inputs without covering stats read everything and rely on the
            exact per-feature filter). Defaults to None (full extent).
        filter: Attribute filter — a SQL-WHERE-style predicate over the
            input's property columns, e.g. `"confidence > 0.8"` or
            `"crop IN ('soy', 'corn')"`. Supports `=, !=, <, <=, >, >=`,
            `IN (...)`, `IS [NOT] NULL`, `AND`/`OR`/`NOT`, and parentheses;
            nulls follow SQL three-valued logic (a row is kept only when the
            predicate is TRUE). Composes with `bbox`; input row groups whose
            parquet column statistics preclude any match are skipped without
            reading their data pages. Defaults to None (no filtering).
        profile: Memory/throughput profile for the single-read pass-2 engine:
            "speed" (buffer output in RAM), "bounded" (spill to temporary
            Arrow IPC files), or "auto" (pick per mode + estimated size).
            Output is byte-identical across profiles. Defaults to "auto".
        in_flight_batches: Read batches allowed in flight through the pass-2
            pipeline at once (read/compute-overlap knob). Higher improves core
            utilization at proportionally more peak memory. Defaults to 0,
            which auto-sizes to the machine's available cores (clamped to
            4..=16); pass an explicit positive integer to override.
        spill_dir: Directory for the remote-input spill file. A remote
            convert stages every fetched chunk on local disk (≈1x the touched
            input bytes) so later passes re-read from disk instead of the
            network; a free-space preflight warns if the projected spill may
            not fit there. The directory must exist. Local inputs never
            spill. Defaults to None (the process temp dir, $TMPDIR).

    Returns:
        Conversion report with keys "mode", "levels" (list of dicts with
        "level", "gsd", "zoom", "feature_count", "vertex_count",
        "uncompressed_bytes", "compressed_bytes"), "skipped_empty_levels"
        (list of dicts with "planned_level", "gsd", "zoom": planned levels
        omitted because no feature is visible at their scale — the written
        pyramid is auto-clamped to the non-empty levels), "input_features",
        "total_rows", "total_vertices", "total_compressed_bytes",
        "row_groups_total", "row_groups_read", "duration_secs", and
        "remote_fetch" (None for local inputs; for remote URLs a dict with
        "requests", "bytes_fetched", "object_size").

    Raises:
        ValueError: Invalid options (bad mode/direction/op, conflicting or
            incomplete ranking options, invalid level plan, missing or
            mistyped columns).
        RuntimeError: The conversion itself failed (I/O, decode, unsupported
            CRS, writer errors).

    Example:
        >>> from tylertoo import overview
        >>> report = overview("moldova.parquet", "moldova-overviews.parquet",
        ...                   min_zoom=0, max_zoom=10)
        >>> report = overview("nyc-trees.parquet", "nyc-trees-overviews.parquet",
        ...                   max_zoom=12, cluster=True,
        ...                   accumulate_attributes={"count": "sum"})
    """
    ...
def export_pmtiles(
    input: str,
    output: str,
    *,
    layer_name: str = "overview",
    tile_buffer: int = 8,
    extent: int = 4096,
    tile_size_limit: int | None = 512000,
    simple_clip_fastpath: bool = True,
    partition_wave: int = 0,
) -> dict[str, Any]:
    """Export an overview GeoParquet file to a PMTiles archive.

    Python equivalent of `tylertoo export-pmtiles`: each overview level
    becomes one Web Mercator zoom of MVT tiles (gzip-compressed).

    Args:
        input: Input overview GeoParquet file (produced by `overview()`).
        output: Output PMTiles archive.
        layer_name: MVT layer name written into every tile and the archive
            metadata. Defaults to "overview".
        tile_buffer: Per-tile edge buffer in tile pixels (feature seam
            continuity). Defaults to 8.
        extent: MVT tile extent (tile-local resolution). Defaults to 4096.
        tile_size_limit: Per-tile MVT size cap in bytes. A tile exceeding it
            sheds features in a single non-iterative drop pass (largest-first
            for polygons/lines; a uniform spatial stride for point tiles).
            Defaults to 512000 (500 KiB, tippecanoe parity); pass 0 (or None)
            to disable the cap.
        simple_clip_fastpath: Skip the i_overlay boundary-bridge fallback for
            features whose rings are already simple (issue #239). Faster
            fine-zoom polygon export; output is render-equivalent on simple
            rings but stores them rotated to a different start vertex.
            Defaults to True; set False for byte-stable tile output.
        partition_wave: Partitions processed per band read during export (the
            export concurrency knob). Defaults to 0, which auto-sizes via a
            memory-budget preflight: the machine's core count, capped by how
            many estimated per-partition transients fit in a fraction of
            available RAM (floor 6; fixed cap 16 only when RAM cannot be
            probed; override the RAM figure with the TYLERTOO_AUTO_MEM_LIMIT_BYTES
            env var). Pass an explicit positive integer to override. Wider
            waves keep more cores busy at proportionally more peak memory.
            Output is byte-identical for every value (the wave is a scheduling
            concern).

    Returns:
        Export report with keys "mode", "min_zoom", "max_zoom", "zooms"
        (list of dicts with "zoom", "level", "level_feature_count",
        "tile_count", "tile_feature_count", "oversized_tiles"), "total_tiles",
        "total_tile_features", "oversized_tiles", "duration_secs".

    Raises:
        RuntimeError: The export failed (not an overview file, unsupported
            CRS, I/O errors).

    Example:
        >>> from tylertoo import export_pmtiles
        >>> report = export_pmtiles("moldova-overviews.parquet", "moldova.pmtiles",
        ...                         layer_name="admin")
        >>> print(report["total_tiles"])
    """
    ...

def validate(file: str) -> dict[str, Any]:
    """Validate a GeoParquet overview file against the overviews spec checklist.

    Python equivalent of `tylertoo validate`: runs every structural
    conformance check (footer metadata, level column, row-group banding, bbox
    covering, provenance blocks, ...) and returns the structured results
    instead of raising on failure.

    Args:
        file: GeoParquet overview file to validate.

    Returns:
        `{"valid": bool, "checks": [{"name": str, "passed": bool,
        "message": str}, ...]}` where "valid" is True iff every check passed.

    Raises:
        RuntimeError: The file could not be opened or its Parquet footer could
            not be parsed (validation never started).

    Example:
        >>> from tylertoo import validate
        >>> result = validate("moldova-overviews.parquet")
        >>> assert result["valid"], [c for c in result["checks"] if not c["passed"]]
    """
    ...
