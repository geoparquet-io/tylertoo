<!-- GENERATED FILE — do not edit by hand.
     Regenerate: cd crates/python && uv run --no-sync \
       --with docstring-parser==0.18.0 \
       python scripts/gen_reference.py > ../../docs/reference/python.md
     CI fails if this file drifts from python/tylertoo/__init__.py. -->

# Python reference

Build GeoParquet overview files and export them to PMTiles archives.

Options and defaults match the CLI. Ground sample distance (GSD) is the meters covered by one pixel at a level. See the [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/) for generalization options.

## `overview`

```python
overview(
    input: str | Sequence[str],
    output: str,
    *,
    mode: Literal['duplicating', 'partitioning'] = "duplicating",
    min_zoom: int = 0,
    max_zoom: int | Literal['auto'] = 6,
    gsds: Sequence[float] | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    magnitude_ladder: str | None = None,
    ladder_step: int = 1,
    sort_direction: Literal['desc', 'asc'] = "desc",
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
    accumulate_attributes: dict[str, Literal['sum', 'max', 'min', 'mean']] | None = None,
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
    profile: Literal['auto', 'speed', 'bounded'] = "auto",
    in_flight_batches: int = 0,
    read_workers: int = 0,
    spill_dir: str | os.PathLike[str] | None = None,
) -> OverviewReport
```

Build a GeoParquet overview file: one generalized level per zoom.

### Parameters

#### Input and levels

* `input`: GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator (`EPSG:3857`). Pass a file, directory, glob, `s3://`, `gs://`, or `https://` URL or prefix, or a list of files.
* `output`: Overview file to write.
* `mode`: `"duplicating"` makes each level a complete map; `"partitioning"` stores each feature once.
* `min_zoom`: Coarsest zoom.
* `max_zoom`: Finest zoom, or `"auto"` to estimate it (at most 16).
* `gsds`: Per-level GSDs in meters, coarse to fine, instead of zooms.
* `gsd_base`: Pixels per tile edge when mapping zooms to GSDs. A larger base keeps more detail.

#### Feature priority

* `sort_key`: Numeric column used to choose each cell's winner.
* `magnitude_ladder`: Column whose values set each feature's first zoom, largest first.
* `ladder_step`: Zooms between `magnitude_ladder` values.
* `sort_direction`: Whether larger or smaller `sort_key` values win.
* `class_rank_column`: String column ranked by `class_ranks`.
* `class_ranks`: Finite priority per class; higher values win.
* `class_rank_unknown`: Priority of unlisted classes, or `None` to rank them last.
* `no_auto_rank`: Skip automatic ranking of Overture roads and places.

#### Geometry and thinning

* `simplify_factor`: Simplification tolerance, in GSDs.
* `collapse`: Represent small polygons as points.
* `collapse_square`: Represent small polygons as small squares.
* `representation`: Geometry kind per zoom band, such as `"0-7:point,8-14:geom"`.
* `cascade`: Speed up simplification by deriving each level from the next finer one.
* `point_thinning`: Point grid cell size in GSDs, or `None` for 4 (16 with `cluster`).
* `line_thinning`: Line grid cell size in GSDs.
* `polygon_thinning`: Polygon grid cell size in GSDs.
* `line_visibility`: Smallest line bounding-box diagonal kept, in GSDs.
* `polygon_visibility`: Smallest polygon bounding-box diagonal kept, in GSDs.
* `drop_rate`: Ratio between feature budgets of adjacent levels.
* `drop_gamma`: How strongly the budget spares sparse areas.
* `density_drop`: Apply the per-level feature budget.

#### Clustering

* `cluster`: Merge each cell's points and add `point_count`.
* `accumulate_attributes`: Aggregation method for each column in a cluster.

#### Line coalescing

* `coalesce_lines`: Join touching same-class lines at coarse levels.
* `coalesce_snap`: Largest gap between joined line ends, in GSDs.
* `coalesce_junction_angle`: Largest turn through a junction, in degrees. Set `0` to stop at every junction.
* `coalesce_max_level_rows`: Skip joining above this line count per level to bound memory.

#### Output, filtering, and memory

* `cogp_compat`: Write the third-party `cogp` footer key for readers of that overview format.
* `row_group_size`: Maximum rows per output row group.
* `full_column_stats`: Keep Parquet statistics for every column.
* `streaming`: Read the input in [batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
* `read_batch_size`: Rows per [read batch](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
* `bbox`: Keep features that intersect this lon/lat box.
* `filter`: SQL-style predicate, such as `"confidence > 0.8"`.
* `profile`: [Memory profile](https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles): `"speed"` holds output in RAM, `"bounded"` uses disk, and `"auto"` chooses per run.
* `in_flight_batches`: [Concurrent batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency), or `0` to size from the CPU count.
* `read_workers`: [Reader threads](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency), or `0` to size from the CPU count.
* `spill_dir`: [Staging directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files) for remote input.

### Returns

[`OverviewReport`](#overviewreport): Counts and sizes for each level written, with input diagnostics.

### Raises

* `ValueError`: Invalid or conflicting options, a missing or mistyped column, an unsupported projection, or almost nothing to tile.
* `MemoryError`: Too little memory for this input.
* `RuntimeError`: Reading or writing failed.

### Example

```python
>>> report = overview(
...     "buildings.parquet", "overview.parquet", max_zoom=10
... )
>>> report["input_features"]
1000
```

## `export_pmtiles`

```python
export_pmtiles(
    input: str,
    output: str,
    *,
    layer_name: str = "overview",
    tile_buffer: int = 8,
    extent: int = 4096,
    tile_size_limit: int | None = 512_000,
    simple_clip_fastpath: bool = True,
    partition_wave: int = 0,
    feature_order: Literal['input'] | str = "input",
    min_zoom: int | None = None,
    feature_id: str | None = None,
    spill_dir: str | os.PathLike[str] | None = None,
) -> ExportReport
```

Export an overview file to a PMTiles archive, one zoom per level.

### Parameters

* `input`: Overview file from `overview()`.
* `output`: PMTiles archive to write.
* `layer_name`: MVT layer name.
* `tile_buffer`: Tile edge buffer in pixels, at most 256.
* `extent`: Tile resolution in MVT units.
* `tile_size_limit`: Maximum tile size in bytes, or `0` or `None` for no limit.
* `simple_clip_fastpath`: Use faster clipping for simple polygons. `False` gives byte-stable output.
* `partition_wave`: [Partitions in memory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves) at once, or `0` to size from CPUs and RAM.
* `feature_order`: Paint order within a tile: `"input"`, or a property name with an optional `:asc` or `:desc`.
* `min_zoom`: Coarsest zoom to declare when the coarsest levels are empty.
* `feature_id`: Integer column to use as the MVT feature ID.
* `spill_dir`: [Spill directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files) used when memory runs short.

### Returns

[`ExportReport`](#exportreport): Tile counts for each zoom written, with encoding diagnostics.

### Raises

* `ValueError`: Invalid `feature_order` syntax.
* `RuntimeError`: Not an overview file, an option is out of range, or reading or writing failed.

### Example

```python
>>> report = export_pmtiles("overview.parquet", "out.pmtiles")
>>> report["max_zoom"]
10
```

## `validate`

```python
validate(
    file: str,
) -> ValidationReport
```

Check an overview file against the overview format specification.

### Parameters

* `file`: Overview file to check.

### Returns

[`ValidationReport`](#validationreport): Each check's result. Failed checks are reported without raising.

### Raises

* `RuntimeError`: The file is not readable Parquet.

### Example

```python
>>> validate("overview.parquet")["valid"]
True
```

## `convert`

```python
convert(
    input: str,
    output: str,
    min_zoom: int = 0,
    max_zoom: int | Literal['auto'] = 14,
    layer_name: str | None = None,
    tile_size_limit: int | None = 512_000,
    simple_clip_fastpath: bool = True,
    feature_order: Literal['input'] | str = "input",
) -> None
```

Convert GeoParquet to PMTiles in one call (deprecated).

Use `overview()` followed by `export_pmtiles()` for all options.

### Parameters

* `input`: See `overview()`.
* `output`: PMTiles archive to write.
* `min_zoom`: See `overview()`.
* `max_zoom`: See `overview()`.
* `layer_name`: MVT layer name, or `None` for the input's file stem.
* `tile_size_limit`: See `export_pmtiles()`.
* `simple_clip_fastpath`: See `export_pmtiles()`.
* `feature_order`: See `export_pmtiles()`.

### Raises

* `ValueError`: Invalid zoom range or options.
* `MemoryError`: Too little memory for this input.
* `RuntimeError`: Conversion or export failed.

### Example

```python
>>> convert("buildings.parquet", "out.pmtiles", max_zoom=10)
```

## Report types

### `OverviewReport`

Overview output counts, sizes, and input diagnostics.

```python
class OverviewReport(TypedDict):
    mode: Literal['duplicating', 'partitioning']
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
```

* `skipped_empty_levels`: Planned levels with nothing visible.
* `row_groups_read`: Row groups selected by `bbox` and `filter`.
* `antimeridian_suspect_features`: Features wider than 180° of longitude, usually a broken antimeridian crossing.
* `out_of_range_features`: Features dropped or clipped because their coordinates are outside the valid range. Longitude past ±180° but within ±540° wraps and is not counted.
* `unprojectable_features`: Features beyond ±85.05° latitude, which Web Mercator cannot tile.

### `LevelReport`

Counts and sizes for one overview level.

```python
class LevelReport(TypedDict):
    level: int
    gsd: float
    zoom: int
    feature_count: int
    vertex_count: int
    uncompressed_bytes: int
    compressed_bytes: int
```

### `SkippedLevel`

A planned level omitted because no features are visible.

```python
class SkippedLevel(TypedDict):
    planned_level: int
    gsd: float
    zoom: int
```

### `OutOfRangeExemplar`

Input location of a feature with out-of-range coordinates.

```python
class OutOfRangeExemplar(TypedDict):
    part: int | None
    row: int
    axis: Literal['lon', 'lat', 'x', 'y']
    value: float
```

* `part`: Input file index, or `None` for a single file.
* `axis`: `"x"` and `"y"` indicate Web Mercator (`EPSG:3857`) input.

### `RemoteFetch`

Network traffic for a remote input.

```python
class RemoteFetch(TypedDict):
    requests: int
    bytes_fetched: int
    object_size: int
```

### `ExportReport`

Exported tile counts and encoding diagnostics.

```python
class ExportReport(TypedDict):
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
```

* `oversized_tiles`: Tiles that dropped features to fit `tile_size_limit`.
* `encode_dropped_features`: Features removed by clipping. A nonzero count logs a warning for lost content.
* `encode_quantized_features`: Features that collapsed at the tile extent, usually harmless edge slivers.
* `skipped_property_columns`: Columns MVT cannot encode, such as binary. Struct, list, and map columns become JSON strings.

### `ZoomReport`

Tile and feature counts for one exported zoom.

```python
class ZoomReport(TypedDict):
    zoom: int
    level: int
    level_feature_count: int
    tile_count: int
    tile_feature_count: int
    oversized_tiles: int
    encode_dropped_features: int
    encode_quantized_features: int
```

### `SkippedPropertyColumn`

A property column omitted because MVT cannot encode its type.

```python
class SkippedPropertyColumn(TypedDict):
    name: str
    data_type: str
```

### `ValidationReport`

Validation results for an overview file.

```python
class ValidationReport(TypedDict):
    valid: bool
    checks: list[ValidationCheck]
```

* `valid`: `True` when every check passed.

### `ValidationCheck`

The result of one conformance check.

```python
class ValidationCheck(TypedDict):
    name: str
    passed: bool
    message: str
```
