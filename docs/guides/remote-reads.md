# Remote reads

tylertoo tiles GeoParquet in object storage without downloading it whole. It
reads each file's footer, decides which row groups can matter, and fetches
only those by byte range. The
[Brazil tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
is a worked example: it reads a 20 km window out of three remote state files
and tiles it.

## Remote inputs

Pass an `s3://`, `https://`, or `gs://` URL wherever a local path goes:

```bash
tylertoo tiles https://example.com/fields.parquet fields.pmtiles
```

An `s3://` or `gs://` prefix ending in `/` lists every `.parquet` object below
it and tiles them as one dataset. A directory or glob of local files works the
same way.

A remote convert stages the column chunks it touches to a local spill file,
about one times the touched bytes. Both passes then read from disk instead of
the network. The run downloads its data about once. Set `--spill-dir` to a
volume with room for it. See [spill files](scaling.md#spill-files). Remote
inputs read with a single worker.

## What gets skipped

GeoParquet 1.1 covering statistics record each row group's bounding box in
the footer. tylertoo checks them before it reads a data page, so a regional
extract rules out most of a country file on footer metadata alone. On a
remote input, the skipped byte ranges never cross the network. Without
covering statistics, tylertoo reads every row group and applies the
per-feature filter to each. The output is the same, and the run loses only
the pruning.

**What makes a source prunable.** Pruning needs row groups that are spatially
sorted and covering statistics that describe them. In an unsorted collection
every row group holds features from across the whole extent. Every row
group's bounding box then overlaps every query, and the run downloads
everything.
Re-sort it with `gpio`, which writes Hilbert-ordered row groups and the
covering metadata:

```bash
gpio sort hilbert raw.parquet sorted.parquet --add-bbox \
  --row-group-size-mb 128
```

## Filters

- **`--bbox xmin,ymin,xmax,ymax`** keeps features whose bounding box
  intersects the box, in longitude and latitude degrees.
- **`--filter <expr>`**, alias `--where`, keeps features that match a
  SQL `WHERE` predicate over the property columns, such as `confidence > 0.8`
  or `crop_type IN ('soy', 'corn')`. It supports comparison operators, `IN`,
  `IS [NOT] NULL`, `AND`, `OR`, `NOT`, parentheses, quoted strings, numbers,
  quoted column names, and timestamp comparisons against date
  strings read as Coordinated Universal Time (UTC). Nulls follow SQL three-valued logic, so a row survives
  only when the predicate is true.

Both check row-group statistics first and skip any row group that cannot
match. They compose, for a spatial and attribute extract straight from the
source:

```bash
tylertoo tiles s3://bucket/fields/ brazil.pmtiles \
  --bbox=-74.1,-34.0,-34.7,5.4 \
  --filter "label = 'field' AND time >= '2025-01-01'"
```

Write `--bbox=` with an equals sign when the first coordinate is negative.

## Many files

**`--files-from <manifest>`** reads the files listed in a text file, one
local path or remote URL per line. tylertoo skips blank lines and `#`
comments, and local and remote entries can mix. Each line must name one `.parquet`
file, not a directory or glob. Line order is the dataset's row order, so the
same manifest gives the same output every run. With `--files-from`, give only
the output path as a positional argument:

```bash
tylertoo tiles --files-from parts.txt fields.pmtiles
```

**Hive layouts.** tylertoo walks a directory input recursively and lists
every key below a prefix, so a layout such as
`year=2024/region=north/part-00000.parquet` reads every part file. It skips
names starting with `.` or `_` (`_SUCCESS`, `.crc`, `_temporary/`). It does
not turn `key=value` path segments into columns, so
`--filter`, `--include-property`, and the tile properties see only columns
stored in the files. To use partition keys, write them into the files with
DuckDB and re-sort with `gpio`:

```bash
duckdb -c "LOAD spatial; COPY (
  SELECT * FROM read_parquet('dataset/**/*.parquet',
                             hive_partitioning = true)
) TO 'flat.parquet' (FORMAT parquet)"
gpio sort hilbert flat.parquet dataset.parquet --row-group-size-mb 128
```
