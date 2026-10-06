# Remote reads

tylertoo reads GeoParquet from object storage by byte range. It checks each
file's footer and fetches only the row groups that may match your filters. The
[Brazil tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
tiles a 20 km window from three remote state files.

## Remote inputs

Use an `s3://`, `https://`, or `gs://` URL in place of a local path:

```bash
tylertoo tiles https://example.com/fields.parquet fields.pmtiles
```

An `s3://` or `gs://` prefix ending in `/` includes every `.parquet` object
below it as one dataset. Local directories and globs work the same way.

Remote conversions use a single reader worker and stage the required column
chunks in a local spill file. Both passes read from disk, so data is downloaded
about once. The spill file needs roughly as much space as the fetched chunks;
use `--spill-dir` to choose a volume with enough room. See
[spill files](scaling.md#spill-files).

## What gets skipped

GeoParquet 1.1 covering statistics store each row group's bounding box in the
footer. tylertoo uses them to skip row groups outside your query before
downloading their data pages. Without covering statistics, it reads every
row group and filters individual features; the output is unchanged.

Spatial sorting makes pruning effective. In an unsorted dataset, each row
group can span the whole extent, so even a small query may download every
group. Use `gpio` to write Hilbert-ordered row groups and covering metadata:

```bash
gpio sort hilbert raw.parquet sorted.parquet --add-bbox \
  --row-group-size-mb 128
```

## Filters

- `--bbox xmin,ymin,xmax,ymax` keeps features whose bounding boxes intersect
  the query box. Coordinates are longitude and latitude in degrees.
- `--filter <expr>` (alias `--where`) applies a SQL `WHERE` predicate to
  property columns, such as `confidence > 0.8` or
  `crop_type IN ('soy', 'corn')`.

Attribute filters support comparison operators, `IN`, `IS [NOT] NULL`,
`AND`, `OR`, `NOT`, parentheses, quoted strings, numbers, quoted column names,
and timestamp comparisons against date strings interpreted as Coordinated
Universal Time (UTC). Nulls follow SQL three-valued logic: a row is kept only
when the predicate is true.

Both filters check row-group statistics to skip groups that cannot match.
Combine them to select features by location and properties:

```bash
tylertoo tiles s3://bucket/fields/ brazil.pmtiles \
  --bbox=-74.1,-34.0,-34.7,5.4 \
  --filter "label = 'field' AND time >= '2025-01-01'"
```

Write `--bbox=` with an equals sign when the first coordinate is negative.

## Many files

`--files-from <manifest>` accepts a text file with one local path or remote
URL per line. You can mix local and remote files; blank lines and `#` comments
are skipped. Each entry must name a single `.parquet` file. The manifest's
line order sets the dataset's row order, giving reproducible output. Supply
only the output path as a positional argument:

```bash
tylertoo tiles --files-from parts.txt fields.pmtiles
```

tylertoo reads Hive layouts such as
`year=2024/region=north/part-00000.parquet` by recursively walking directories
or listing keys below a remote prefix. Names starting with `.` or `_` are
skipped, including `_SUCCESS`, `.crc`, and `_temporary/`.

Partition keys in `key=value` paths are not added as columns. Filters,
`--include-property`, and tile properties use only columns stored in the
files. To use partition keys, write them into the files with DuckDB, then
re-sort with `gpio`:

```bash
duckdb -c "LOAD spatial; COPY (
  SELECT * FROM read_parquet('dataset/**/*.parquet',
                             hive_partitioning = true)
) TO 'flat.parquet' (FORMAT parquet)"
gpio sort hilbert flat.parquet dataset.parquet --row-group-size-mb 128
```
