<!-- GENERATED FILE — do not edit by hand.
     Regenerate: cd crates/python && uv run --no-sync \
       --with docstring-parser==0.18.0 \
       python scripts/gen_reference.py > ../../docs/reference/python.md
     CI fails if this file drifts from python/tylertoo/__init__.py. -->

# Python reference

Build GeoParquet overview files and export them to PMTiles archives.

Options and defaults match the CLI. Ground sample distance (GSD) is the meters covered by one pixel at a level. See the [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/) for generalization options.

Diagnostics go to Python's `logging` under the `tylertoo_core` logger: the warnings and progress lines the CLI prints. Python shows warnings by default; call `logging.basicConfig(level=logging.INFO)` to see progress. Each call reads the logging configuration when it starts.

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
    entry_zoom: str | None = None,
    sort_direction: Literal['desc', 'asc'] = "desc",
    class_rank_column: str | None = None,
    class_ranks: dict[str, float] | None = None,
    class_rank_unknown: float | None = None,
    no_auto_rank: bool = False,
    verbatim: bool = False,
    simplify_factor: float | None = None,
    collapse: bool = False,
    collapse_square: bool = False,
    no_collapse: bool = False,
    representation: str | None = None,
    cascade: bool = True,
    point_thinning: float | None = None,
    line_thinning: float | None = None,
    polygon_thinning: float | None = None,
    line_visibility: float | None = None,
    polygon_visibility: float | None = None,
    drop_rate: float = 1.65,
    drop_gamma: float = 1.5,
    density_drop: bool | None = None,
    drop_lines: bool = False,
    drop_polygons: bool = False,
    cluster: bool = False,
    accumulate_attributes: dict[str, Literal['sum', 'max', 'min', 'mean']] | None = None,
    coalesce_lines: bool | None = None,
    coalesce_snap: float = 1.0,
    coalesce_junction_angle: float = 0.0,
    coalesce_max_level_rows: int = 2_000_000,
    include_properties: Sequence[str] | None = None,
    exclude_properties: Sequence[str] | None = None,
    exclude_all_properties: bool = False,
    cogp_compat: bool = False,
    row_group_size: int = 10_000,
    row_group_size_policy: Literal['constant', 'zoom-scaled'] = "constant",
    full_column_stats: bool = False,
    streaming: bool = True,
    read_batch_size: int = 8192,
    bbox: tuple[float, float, float, float] | None = None,
    filter: str | None = None,
    profile: Literal['auto', 'speed', 'bounded'] = "auto",
    in_flight_batches: int = 0,
    read_workers: int = 0,
    spill_dir: str | os.PathLike[str] | None = None,
    save_plan: str | os.PathLike[str] | None = None,
    plan: str | os.PathLike[str] | None = None,
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
* `entry_zoom`: First zoom for listed values of a column, as `"COLUMN:VALUE=ZOOM,VALUE=ZOOM"`, such as `"density:5000=4,1000=6"`. Unlisted values use the normal visibility gate. Not with `magnitude_ladder`.
* `sort_direction`: Whether larger or smaller `sort_key` values win.
* `class_rank_column`: String column ranked by `class_ranks`.
* `class_ranks`: Finite priority per class; higher values win.
* `class_rank_unknown`: Priority of unlisted classes, or `None` to rank them last.
* `no_auto_rank`: Skip automatic ranking of Overture roads and places.

#### Geometry and thinning

* `verbatim`: Turn off thinning, visibility gates, simplification, the density budget, and line coalescing, for pre-aggregated or pre-levelled input. Sets the defaults of the options below to off; an option you pass still applies.
* `simplify_factor`: Simplification tolerance, in GSDs, or `None` for 1 (0 with `verbatim`).
* `collapse`: Represent small polygons as points instead of the default placeholder squares.
* `collapse_square`: Represent small polygons as placeholder squares. Squares are the default except with an entry-zoom ladder, which uses points unless you set this option.
* `no_collapse`: Drop small polygons instead of using placeholders. This was the default before #745. Output is smaller, but polygon coverages have holes at coarse zooms.
* `representation`: Geometry kind per zoom band, such as `"0-7:point,8-14:geom"`.
* `cascade`: Speed up simplification by deriving each level from the next finer one.
* `point_thinning`: Point grid cell size in GSDs, or `None` for 4 (16 with `cluster`, 0 with `verbatim`).
* `line_thinning`: Line grid cell size in GSDs, or `None` for 1 (0 with `verbatim`).
* `polygon_thinning`: Polygon grid cell size in GSDs, or `None` for 0 (off).
* `line_visibility`: Smallest line bounding-box diagonal kept, in GSDs, or `None` for 2 (0 with `verbatim`).
* `polygon_visibility`: Smallest polygon bounding-box diagonal kept, in GSDs, or `None` for 2 (0 with `verbatim`).
* `drop_rate`: Ratio between feature budgets of adjacent levels.
* `drop_gamma`: How strongly the budget spares sparse areas.
* `density_drop`: Apply the per-level feature budget, or `None` for on (off with `verbatim`). It thins points only unless `drop_lines` or `drop_polygons` is set.
* `drop_lines`: Apply the feature budget to lines too.
* `drop_polygons`: Apply the feature budget to polygons too, which leaves holes in a polygon coverage.

#### Clustering

* `cluster`: Merge each cell's points and add `point_count`.
* `accumulate_attributes`: Aggregation method for each column in a cluster.

#### Line coalescing

* `coalesce_lines`: Join touching same-class lines at coarse levels, or `None` for on (off with `verbatim`). `True` is refused in partitioning mode.
* `coalesce_snap`: Largest gap between joined line ends, in GSDs.
* `coalesce_junction_angle`: Largest turn through a junction, in degrees. Set `0` to stop at every junction.
* `coalesce_max_level_rows`: Skip joining above this line count per level to bound memory.

#### Properties

* `include_properties`: Keep only these property columns and skip reading the rest. Columns other options read, such as `sort_key`, must stay in the list. `[]` keeps none.
* `exclude_properties`: Drop these property columns. `include_properties` overrides it.
* `exclude_all_properties`: Drop every property column, writing geometry only. `include_properties` overrides it.

#### Output, filtering, and memory

* `cogp_compat`: Write the third-party `cogp` footer key for readers of that overview format.
* `row_group_size`: Maximum rows per output row group.
* `row_group_size_policy`: `"constant"`, or `"zoom-scaled"` to double the row-group cap per zoom coarser than the finest.
* `full_column_stats`: Keep Parquet statistics for every column.
* `streaming`: Read the input in [batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
* `read_batch_size`: Rows per [read batch](https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory).
* `bbox`: Keep features that intersect this lon/lat box, also one ±360° wrap away, as for 0..360 data.
* `filter`: SQL-style predicate, such as `"confidence > 0.8"`.
* `profile`: [Memory profile](https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles): `"speed"` holds output in RAM, `"bounded"` uses disk, and `"auto"` chooses per run.
* `in_flight_batches`: [Concurrent batches](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency), or `0` to size from the CPU count.
* `read_workers`: [Reader threads](https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency), or `0` to size from the CPU count.
* `spill_dir`: [Staging directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files) for remote input.
* `save_plan`: Save the convert plan (pass 1 and the level assignment) here and continue converting.
* `plan`: Reuse a saved convert plan, skipping pass 1. Its fingerprint must match this run's version, options and inputs.

### Returns

[`OverviewReport`](#overviewreport): Counts and sizes for each level written, with input diagnostics.

### Raises

* `ConfigError`: Invalid or conflicting options, refused before any input is read.
* `ValueError`: A missing or mistyped column, a property selection the input cannot satisfy, an unsupported projection, or almost nothing to tile.
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
    tile_size_limit: int | Literal['auto'] | None = "auto",
    simple_clip_fastpath: bool = True,
    partition_wave: int = 0,
    feature_order: Literal['input'] | str = "input",
    min_zoom: int | None = None,
    include_properties: Sequence[str] | None = None,
    exclude_properties: Sequence[str] | None = None,
    exclude_all_properties: bool = False,
    feature_id: str | None = None,
    spill_dir: str | os.PathLike[str] | None = None,
    name: str | None = None,
    description: str | None = None,
    attribution: str | None = None,
    size_valve: Literal['merge', 'drop'] = "merge",
    max_compressed_tile_size: int | Literal['auto'] | None = "auto",
    coverage_fill: bool = False,
    tile_range: str | None = None,
    zoom_ceiling: int | None = None,
) -> ExportReport
```

Export an overview file to a PMTiles archive, one zoom per level.

### Parameters

* `input`: Overview file from `overview()`.
* `output`: PMTiles archive to write.
* `layer_name`: MVT layer name.
* `tile_buffer`: Tile edge buffer in pixels, at most 256.
* `extent`: Tile resolution in MVT units.
* `tile_size_limit`: Maximum raw (uncompressed) tile size in bytes, a backstop behind `max_compressed_tile_size`; `"auto"` is 2 MiB, and `0` or `None` is no limit.
* `simple_clip_fastpath`: Use faster clipping for simple polygons. `False` gives byte-stable output.
* `partition_wave`: [Partitions in memory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves) at once, or `0` to size from CPUs and RAM.
* `feature_order`: Paint order within a tile: `"input"`, or a property name with an optional `:asc` or `:desc`.
* `min_zoom`: Coarsest zoom to declare when the coarsest levels are empty.
* `include_properties`: Keep only these properties in the tiles. The overview file keeps every column, and the `feature_order` column must stay. `[]` keeps none.
* `exclude_properties`: Drop these properties from the tiles. `include_properties` overrides it.
* `exclude_all_properties`: Drop every property, writing geometry-only tiles. `include_properties` overrides it.
* `feature_id`: Integer column to use as the MVT feature ID.
* `spill_dir`: [Spill directory](https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files) used when memory runs short.
* `name`: Human-readable tileset name, stored in the metadata.
* `description`: Description of the tileset, stored in the metadata.
* `attribution`: Attribution text or HTML stored in the archive metadata; MapLibre shows it in its attribution control. Each value is stored as given, and `""` is kept as `""`.
* `size_valve`: How to reduce a tile that exceeds its limit: `"merge"` merges the densest lines and polygons into a neighbor, which keeps its own attributes; `"drop"` removes features, leaving holes. Point tiles use feature dropping in both modes.
* `max_compressed_tile_size`: Maximum gzip-compressed tile size in bytes, like tippecanoe's `-M`; `"auto"` is 500 KiB, and `0` or `None` is no limit.
* `coverage_fill`: Experimental: under each tile's features, draw the area its child tiles cover, so a polygon coverage shows no holes at coarse zooms. Holds the finest level's geometry in memory. See `docs/OVERVIEW_TUNING.md`.
* `tile_range`: Export only the tiles in this tile-id range, `"LO..HI"`: one shard of a sharded build.
* `zoom_ceiling`: Export only the tiles at or below this zoom: the coarse job of a sharded build.

### Returns

[`ExportReport`](#exportreport): Tile counts for each zoom written, with encoding diagnostics.

### Raises

* `ConfigError`: An option out of range or refused, such as `tile_buffer` past 256, `extent=0`, a missing `spill_dir`, or metadata too large for the 16 MiB archive metadata.
* `ValueError`: A property selection the overview file cannot satisfy.
* `RuntimeError`: Not an overview file, or reading or writing failed.

### Example

```python
>>> report = export_pmtiles("overview.parquet", "out.pmtiles")
>>> report["max_zoom"]
10
>>> credited = export_pmtiles(
...     "overview.parquet",
...     "credited.pmtiles",
...     attribution="© Example contributors",
... )
>>> credited["zooms"] == report["zooms"]
True
```

## `pyramid`

```python
pyramid(
    bands: Sequence[str],
    output: str,
    *,
    generalize: bool = False,
    tile_size_limit: int | Literal['auto'] | None = "auto",
    feature_order: Literal['input'] | str = "input",
    work_dir: str | os.PathLike[str] | None = None,
    allow_missing_zooms: bool = False,
    name: str | None = None,
    description: str | None = None,
    attribution: str | None = None,
    max_compressed_tile_size: int | Literal['auto'] | None = "auto",
    size_valve: Literal['merge', 'drop'] = "merge",
    coverage_fill: bool = False,
    simple_clip_fastpath: bool = True,
    tile_buffer: int = 8,
    extent: int = 4096,
    partition_wave: int = 0,
    feature_id: str | None = None,
    include_properties: Sequence[str] | None = None,
    exclude_properties: Sequence[str] | None = None,
    exclude_all_properties: bool = False,
) -> PyramidReport
```

Build one PMTiles archive from per-zoom-band inputs.

Each band names a zoom range, an input, and an MVT layer. Bands of different layers may share zooms, giving several layers per tile; bands of one layer must not overlap. The output is overwritten if it exists.

### Parameters

* `bands`: Band specs as `"LO-HI:INPUT"` or `"LO-HI:INPUT:LAYER"`, such as `"0-7:countries.parquet:admin"`. `INPUT` is a GeoParquet source or a PMTiles archive for zooms `LO` to `HI`; `LAYER` defaults to the input's file stem. See the [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/) for the `LO-HI=INPUT=LAYER` form.
* `output`: PMTiles archive to write.
* `generalize`: Generalize each GeoParquet band, as `overview()` does. By default each band is tiled verbatim, keeping every feature, for input already made for its zooms.
* `tile_size_limit`: Maximum raw tile size in bytes for bands tiled here; `"auto"`, `0` and `None` are no limit, so every feature is kept.
* `feature_order`: Paint order within a tile for GeoParquet bands; see `export_pmtiles()`. A band without that column keeps input order, with a warning.
* `work_dir`: Directory for temporary per-band files, deleted after the build, or `None` for the system temporary directory.
* `allow_missing_zooms`: Accept a pre-tiled band with fewer zooms than declared. Missing zooms render empty.
* `name`: The output's tileset name. Unset, the PMTiles bands' name is kept when they agree and left out when they don't.
* `description`: Description for the output, inherited the same way.
* `attribution`: Attribution text or HTML for the output, `""` included. Unset, every PMTiles band's credit is kept, joined with `" | "`; GeoParquet bands carry none.
* `max_compressed_tile_size`: Maximum gzip-compressed tile size in bytes for bands tiled here; `"auto"`, `0` and `None` are no limit.
* `size_valve`: See `export_pmtiles()`.
* `coverage_fill`: See `export_pmtiles()`.
* `simple_clip_fastpath`: See `export_pmtiles()`.
* `tile_buffer`: See `export_pmtiles()`.
* `extent`: See `export_pmtiles()`.
* `partition_wave`: See `export_pmtiles()`.
* `feature_id`: See `export_pmtiles()`. Every GeoParquet band must have the column.
* `include_properties`: Keep only these properties in the tiles of GeoParquet bands. `[]` keeps none.
* `exclude_properties`: Drop these properties from the tiles of GeoParquet bands. `include_properties` overrides it.
* `exclude_all_properties`: Drop every property from the tiles of GeoParquet bands. `include_properties` overrides it.

### Returns

[`PyramidReport`](#pyramidreport): Tile counts per band, with merge diagnostics and the metadata written.

### Raises

* `ConfigError`: An option out of range or refused.
* `ValueError`: A malformed band spec, overlapping bands of one layer, or a missing band input.
* `RuntimeError`: A band failed to convert, export, or merge.

### Example

```python
>>> report = pyramid(
...     [
...         "8-9:buildings.parquet:coarse",
...         "10-12:buildings.parquet:fine",
...     ],
...     "pyramid.pmtiles",
... )
>>> [band["layer"] for band in report["bands"]]
['coarse', 'fine']
```

## `decode`

```python
decode(
    input: str,
    output: str,
    *,
    min_zoom: int | None = None,
    max_zoom: int | None = None,
    layer: str | None = None,
) -> DecodeReport
```

Decode a PMTiles archive of vector tiles back to GeoParquet.

Writes one row per feature per tile: the tiled representation, so features are simplified, clipped at tile edges, and repeated at every zoom and in every tile they cross. The columns are `zoom`, `layer`, `mvt_id`, the properties in name order, then `geometry` as lon/lat (`EPSG:4326`) WKB. The output is overwritten if it exists.

### Parameters

* `input`: PMTiles archive of vector tiles.
* `output`: GeoParquet file to write.
* `min_zoom`: Coarsest zoom to decode, or `None` for the archive's coarsest.
* `max_zoom`: Finest zoom to decode, or `None` for the archive's finest. Set both to one zoom to decode only that zoom.
* `layer`: Decode only this MVT layer, or `None` for every layer.

### Returns

[`DecodeReport`](#decodereport): Counts of tiles read and features written, with the layers and zooms found.

### Raises

* `ValueError`: `min_zoom` above `max_zoom`, an archive of raster tiles, or a property named like a provenance column.
* `RuntimeError`: Not a PMTiles archive, a damaged tile, or reading or writing failed.

### Example

```python
>>> report = decode(
...     "buildings.pmtiles", "decoded.parquet", min_zoom=10
... )
>>> report["layers"], report["zoom_range"]
(['overview'], (10, 10))
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
    tile_size_limit: int | Literal['auto'] | None = "auto",
    simple_clip_fastpath: bool = True,
    feature_order: Literal['input'] | str = "input",
    spill_dir: str | os.PathLike[str] | None = None,
    *,
    include_properties: Sequence[str] | None = None,
    exclude_properties: Sequence[str] | None = None,
    exclude_all_properties: bool = False,
    size_valve: Literal['merge', 'drop'] = "merge",
    max_compressed_tile_size: int | Literal['auto'] | None = "auto",
    coverage_fill: bool = False,
    gsds: Sequence[float] | None = None,
    bbox: tuple[float, float, float, float] | None = None,
    name: str | None = None,
    description: str | None = None,
    attribution: str | None = None,
    tile_buffer: int = 8,
    extent: int = 4096,
    partition_wave: int = 0,
    feature_id: str | None = None,
    keep_overview: str | os.PathLike[str] | None = None,
    tile_range: str | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    magnitude_ladder: str | None = None,
    ladder_step: int = 1,
    entry_zoom: str | None = None,
    sort_direction: Literal['desc', 'asc'] = "desc",
    class_rank_column: str | None = None,
    class_ranks: dict[str, float] | None = None,
    class_rank_unknown: float | None = None,
    no_auto_rank: bool = False,
    verbatim: bool = False,
    simplify_factor: float | None = None,
    collapse: bool = False,
    collapse_square: bool = False,
    no_collapse: bool = False,
    representation: str | None = None,
    cascade: bool = True,
    point_thinning: float | None = None,
    line_thinning: float | None = None,
    polygon_thinning: float | None = None,
    line_visibility: float | None = None,
    polygon_visibility: float | None = None,
    drop_rate: float = 1.65,
    drop_gamma: float = 1.5,
    density_drop: bool | None = None,
    drop_lines: bool = False,
    drop_polygons: bool = False,
    cluster: bool = False,
    accumulate_attributes: dict[str, Literal['sum', 'max', 'min', 'mean']] | None = None,
    coalesce_lines: bool | None = None,
    coalesce_snap: float = 1.0,
    coalesce_junction_angle: float = 0.0,
    coalesce_max_level_rows: int = 2_000_000,
    row_group_size: int = 10_000,
    row_group_size_policy: Literal['constant', 'zoom-scaled'] = "constant",
    full_column_stats: bool = False,
    streaming: bool = True,
    read_batch_size: int = 8192,
    filter: str | None = None,
    profile: Literal['auto', 'speed', 'bounded'] = "auto",
    in_flight_batches: int = 0,
    read_workers: int = 0,
    save_plan: str | os.PathLike[str] | None = None,
    plan: str | os.PathLike[str] | None = None,
) -> ConvertReport
```

Convert GeoParquet to PMTiles in one call, like `tylertoo tiles`.

Runs the same core facade as `tylertoo tiles`, with the same options and defaults, so the chain — the removed-after-export intermediate overview, its free-space preflight, the option rules, and the declared zoom range — matches the CLI. `overview()` followed by `export_pmtiles()` is the same chain in two steps. Sharded builds (`--shard`, `--plan-only`) stay CLI-only.

### Parameters

* `input`: See `overview()`.
* `output`: PMTiles archive to write.
* `min_zoom`: See `overview()`.
* `max_zoom`: See `overview()`.
* `layer_name`: MVT layer name, or `None` for the input's file stem.
* `tile_size_limit`: See `export_pmtiles()`. `"auto"` is no limit with `verbatim`.
* `simple_clip_fastpath`: See `export_pmtiles()`.
* `feature_order`: See `export_pmtiles()`.
* `spill_dir`: Scratch directory for the intermediate overview and the export's spill (the CLI's `--spill-dir`). The intermediate is at least input-sized; without this it goes to `$TMPDIR`, then the output's directory. Set it when `$TMPDIR` is small or a tmpfs. A `UserWarning` is raised when the chosen volume looks too small, whether or not the run then fails.
* `include_properties`: See `overview()`.
* `exclude_properties`: See `overview()`.
* `exclude_all_properties`: See `overview()`.
* `size_valve`: See `export_pmtiles()`.
* `max_compressed_tile_size`: See `export_pmtiles()`. `"auto"` is no limit with `verbatim`.
* `coverage_fill`: See `export_pmtiles()`.
* `gsds`: See `overview()`.
* `bbox`: See `overview()`.
* `name`: See `export_pmtiles()`.
* `description`: See `export_pmtiles()`.
* `attribution`: See `export_pmtiles()`.
* `tile_buffer`: See `export_pmtiles()`.
* `extent`: See `export_pmtiles()`.
* `partition_wave`: See `export_pmtiles()`.
* `feature_id`: See `export_pmtiles()`.
* `keep_overview`: Keep the intermediate overview file at this path instead of removing it. Its directory must exist.
* `tile_range`: See `export_pmtiles()`.
* `gsd_base`: See `overview()`.
* `sort_key`: See `overview()`.
* `magnitude_ladder`: See `overview()`.
* `ladder_step`: See `overview()`.
* `entry_zoom`: See `overview()`.
* `sort_direction`: See `overview()`.
* `class_rank_column`: See `overview()`.
* `class_ranks`: See `overview()`.
* `class_rank_unknown`: See `overview()`.
* `no_auto_rank`: See `overview()`.
* `verbatim`: See `overview()`. Also turns both tile size limits off unless you pass them.
* `simplify_factor`: See `overview()`.
* `collapse`: See `overview()`.
* `collapse_square`: See `overview()`.
* `no_collapse`: See `overview()`.
* `representation`: See `overview()`.
* `cascade`: See `overview()`.
* `point_thinning`: See `overview()`.
* `line_thinning`: See `overview()`.
* `polygon_thinning`: See `overview()`.
* `line_visibility`: See `overview()`.
* `polygon_visibility`: See `overview()`.
* `drop_rate`: See `overview()`.
* `drop_gamma`: See `overview()`.
* `density_drop`: See `overview()`.
* `drop_lines`: See `overview()`.
* `drop_polygons`: See `overview()`.
* `cluster`: See `overview()`.
* `accumulate_attributes`: See `overview()`.
* `coalesce_lines`: See `overview()`.
* `coalesce_snap`: See `overview()`.
* `coalesce_junction_angle`: See `overview()`.
* `coalesce_max_level_rows`: See `overview()`.
* `row_group_size`: See `overview()`.
* `row_group_size_policy`: See `overview()`.
* `full_column_stats`: See `overview()`.
* `streaming`: See `overview()`.
* `read_batch_size`: See `overview()`.
* `filter`: See `overview()`.
* `profile`: See `overview()`.
* `in_flight_batches`: See `overview()`.
* `read_workers`: See `overview()`.
* `save_plan`: See `overview()`.
* `plan`: See `overview()`.

### Returns

[`ConvertReport`](#convertreport): Both halves' reports, keyed `"convert"` and `"export"` like the CLI's `tiles --report` JSON.

### Raises

* `ConfigError`: Invalid or conflicting options, refused before any input is read.
* `ValueError`: A missing or mistyped column, or a property selection the input cannot satisfy.
* `MemoryError`: Too little memory for this input.
* `RuntimeError`: Conversion or export failed.

### Example

```python
>>> report = convert(
...     "buildings.parquet", "out.pmtiles", max_zoom=10
... )
>>> sorted(report)
['convert', 'export']
```

## Report types

### `OverviewReport`

Overview output counts, sizes, and input diagnostics.

```python
class OverviewReport(TypedDict):
    diagnostics: list[Diagnostic]
    source_losses: list[SourceLoss] | SpilledSourceLosses
    input_rows: int
    source_row_ranges: list[tuple[int, int]]
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
    crs_assumed_wgs84: bool
    out_of_range_exemplars: list[OutOfRangeExemplar]
    wrapped_features: int
    duration_secs: float
    remote_fetch: RemoteFetch | None
    effective_max_row_group_size: int | None
```

* `skipped_empty_levels`: Planned levels with nothing visible.
* `row_groups_read`: Row groups selected by `bbox` and `filter`.
* `antimeridian_suspect_features`: Features wider than 180° of longitude, usually a broken antimeridian crossing.
* `out_of_range_features`: Features dropped or clipped because their coordinates are outside the valid range. Longitude past ±180° but within ±540° wraps and is not counted.
* `unprojectable_features`: Features beyond ±85.05° latitude, which Web Mercator cannot tile.
* `wrapped_features`: Features past ±180° longitude but within ±540°, drawn wrapped across the antimeridian. Expected for 0..360 data; otherwise the declared CRS may be wrong. Not a loss.
* `crs_assumed_wgs84`: True when the input declared no CRS and lon/lat WGS84 was assumed. If the coordinates are not lon/lat degrees, the output is misplaced.
* `effective_max_row_group_size`: The row-group cap actually used, when it had to be raised above `row_group_size` to keep the file under Parquet's row-group limit. None otherwise.

### `Diagnostic`

One typed diagnostic aggregated by the core operation.

```python
class Diagnostic(TypedDict):
    diagnostics: NotRequired[list[Diagnostic]]
    exemplars: NotRequired[list[OutOfRangeExemplar]]
    context: NotRequired[dict[str, Any] | None]
    repair_kind: NotRequired[str]
    cap: NotRequired[int]
    name: NotRequired[str]
    data_type: NotRequired[str]
    reason: NotRequired[dict[str, Any]]
    machine: NotRequired[int | None]
    limit: NotRequired[int]
    input: NotRequired[str]
    total: NotRequired[int]
    levels: NotRequired[list[int]]
    gsd_min: NotRequired[float]
    gsd_max: NotRequired[float]
    input_features: NotRequired[int]
    first_written_gsd: NotRequired[float]
    first_written_zoom: NotRequired[int | None]
    kind: str
    count: int
```

### `SourceLoss`

An absent source row, with its optional exported MVT feature ID.

```python
class SourceLoss(TypedDict):
    id: int
    feature_id: int | None
    zoom: int | None
    cause: str
```

### `SpilledSourceLosses`

An exact loss report that exceeds the Python memory allowance.

`count` is the number of losses. `path` names a JSON Lines file with one SourceLoss per line, in zoom and source-row order. It uses `spill_dir` when supplied and survives the call. The caller owns the file and must delete it after reading. Small reports retain list[SourceLoss].

```python
class SpilledSourceLosses(TypedDict):
    count: int
    path: str
```

### `LevelReport`

Counts and sizes for one overview level.

```python
class LevelReport(TypedDict):
    level: int
    gsd: float
    zoom: int | None
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
    zoom: int | None
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
    diagnostics: list[Diagnostic]
    source_losses: list[SourceLoss] | SpilledSourceLosses
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
```

* `oversized_tiles`: Tiles that exceeded a size limit and triggered size reduction.
* `merged_features`: Tile features merged into a neighbor to fit a size limit (`size_valve="merge"`).
* `valve_dropped_features`: Tile features dropped to fit a size limit.
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
    merged_features: int
    valve_dropped_features: int
    max_valve_passes: int
    valve_passes: int
    fill_polygons: int
    fill_vertices: int
    fill_shed: int
```

### `SkippedPropertyColumn`

A property column omitted because MVT cannot encode its type.

```python
class SkippedPropertyColumn(TypedDict):
    name: str
    data_type: str
```

### `PyramidReport`

What `pyramid()` merged.

```python
class PyramidReport(TypedDict):
    diagnostics: list[Diagnostic]
    total_tiles: int
    bands: list[PyramidBandReport]
    skipped_tiles: int
    bands_without_bounds: list[str]
    metadata: PublicationMetadata
    metadata_conflicts: list[str]
    bands_without_metadata: list[str]
```

* `bands`: One entry per band, by zoom range, coarsest first.
* `skipped_tiles`: Tiles in a pre-tiled band at zooms outside its declared range, and so left out. Expected when one archive is split across several bands.
* `bands_without_bounds`: Bands whose archive declared no bounds, and so did not widen the merged archive's bounds.
* `metadata`: The `name`, `description` and `attribution` written: the keywords, over what the PMTiles bands carry.
* `metadata_conflicts`: Fields (`"name"`, `"description"`) the PMTiles bands disagreed on, and so left out.
* `bands_without_metadata`: PMTiles bands whose metadata is not valid JSON, and so passed on no credit.

### `PyramidBandReport`

Tiles one band contributed to a pyramid.

```python
class PyramidBandReport(TypedDict):
    layer: str
    min_zoom: int
    max_zoom: int
    tiles: int
```

### `PublicationMetadata`

An archive's `name`, `description` and `attribution`.

```python
class PublicationMetadata(TypedDict):
    name: str | None
    description: str | None
    attribution: str | None
```

* `name`: The tileset name, or `None` when the archive has none.
* `description`: The description, or `None`.
* `attribution`: The attribution text or HTML, or `None`.

### `DecodeReport`

What `decode()` read and wrote.

```python
class DecodeReport(TypedDict):
    tiles_read: int
    features_written: int
    features_skipped: int
    layers: list[str]
    zoom_range: tuple[int, int] | None
    elapsed_secs: float
```

* `tiles_read`: Tiles read, after the zoom filter.
* `features_written`: Feature rows written.
* `features_skipped`: Features whose geometry decoded to nothing, such as a polygon collapsed at the tile extent.
* `layers`: MVT layers read, after the layer filter, sorted.
* `zoom_range`: Coarsest and finest zoom written, or `None` when no feature matched the filters.
* `elapsed_secs`: Wall-clock seconds.

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

### `ConvertReport`

The combined report of a one-shot `convert()` run.

Both halves of the chain, keyed like the CLI's `tiles --report` JSON: the overview conversion and the PMTiles export.

```python
class ConvertReport(TypedDict):
    convert: OverviewReport
    export: ExportReport
```

## Errors

### `ConfigError`

```python
class ConfigError(ValueError)
```

An option value or combination refused before any input is read. The rules are the core library's, the same ones the CLI enforces.
