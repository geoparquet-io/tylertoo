# Tiling remote and multi-file inputs

The fastest byte to process is the one you never read. tylertoo can tile a file
sitting in object storage without downloading it whole, extract one region from a
planet-scale input without scanning the rest, and treat a directory of partition
files as one dataset. This topic groups the features that share that theme:
pushdown, deciding what to skip before any data page loads.

The Brazil demo tiled 40.7 GiB straight from Source Cooperative and filtered it
while tiling. The features below are how a run like that touches only the bytes
it needs.

## Design decisions

**Remote inputs read via byte-range requests.** Given an `s3://`, `https://`, or
`gs://` URL, the reader fetches the Parquet footer first, then only the row
groups it decides to read. A remote convert never begins with a full download,
so time to first tile depends on the data you use, not the size of the object.

**Covering stats skip row groups at the footer.** GeoParquet 1.1 records a
per-row-group bounding box, and the reader consults it before touching data
pages. A regional extract from a country file therefore rules out most of the
file on footer metadata alone. Inputs without covering statistics still work, by
reading every row group and applying the exact per-feature filter, so the result
is identical and only the pruning is lost.

**Predicates push down before data pages load.** Both `--bbox` and `--filter`
evaluate against row-group statistics first. A row group that cannot contain a
match is skipped whole, and on a remote input its byte ranges never cross the
network. The filter runs during the pass-1 scan, so it composes with `--bbox`
rather than fighting it.

**files-from fixes dataset row order verbatim.** The manifest's line order is the
dataset's row order, preserved exactly. A convert over the same manifest produces
the same ordering every run, which is what makes multi-partition output
reproducible rather than dependent on directory listing order.

**Remote reads stage to local disk once.** A remote convert stages the bytes it
touches into a local spill file so the two passes re-read from disk instead of
re-fetching over the network. That mechanism, and where to put the spill, is
covered in [Keeping memory bounded](bounded-memory.md); here it is enough to know
a remote run downloads its data about once.

## API walkthrough

### Reading straight from object storage

**`s3://` / `https://` / `gs://` URLs.** A remote object as input, read by byte
range. The same command that tiles a local file tiles a remote one; only the
path changes.

```bash
# Tile straight from object storage — no full download.
tylertoo overview \
  s3://gpq-tiles-demo/brazil-2025-fields.parquet \
  brazil-ov.parquet \
  --min-zoom 0 --max-zoom 14
```

**`s3://…/` and `gs://…/` prefixes.** A trailing slash lists the `.parquet`
objects under the prefix and tiles them as one dataset, for a bucket laid out as
many partition objects.

```bash
# A trailing slash tiles every .parquet under the prefix as one dataset.
tylertoo overview \
  s3://gpq-tiles-demo/brazil-partitions/ \
  brazil-ov.parquet \
  --min-zoom 0 --max-zoom 14
```

### Extracting one region

**`--bbox xmin,ymin,xmax,ymax`.** Converts only features whose bounding box
intersects the box, in lon/lat degrees. Row groups outside the box are pruned at
the footer, so a city-sized window from a country file reads a fraction of the
data and, on remote input, downloads a fraction of the bytes. The tutorial's São
Paulo preview is this knob used to finish in seconds.

```bash
# Carve the São Paulo window straight out of the remote country file.
tylertoo overview \
  s3://gpq-tiles-demo/brazil-2025-fields.parquet \
  sp-preview-ov.parquet \
  --min-zoom 0 --max-zoom 12 \
  --bbox -48,-24,-46,-22
```

### Filtering by attribute

**`--filter <expr>`, aliased `--where`.** A SQL-WHERE predicate over the input's
property columns, such as `confidence > 0.8` or `crop_type IN ('soy', 'corn')`.
It supports the comparison operators, `IN`, `IS [NOT] NULL`, `AND`/`OR`/`NOT`,
parentheses, string and numeric literals, quoted column names, and timestamp
comparisons against date strings read as UTC. Nulls follow SQL three-valued
logic, so a row survives only when the predicate is true. Where column statistics
preclude a match, the row group is skipped at the footer like `--bbox`, and it
composes with `--bbox` for a combined spatial-and-attribute extract straight from
the source file.

```bash
# Attribute pushdown: only 2025 field predictions, filtered while tiling.
tylertoo overview \
  s3://gpq-tiles-demo/brazil-2025-fields.parquet \
  brazil-ov.parquet \
  --min-zoom 0 --max-zoom 14 \
  --filter "label = 'field' AND time >= '2025-01-01'"

# Composes with --bbox and richer predicates.
tylertoo overview brazil-sorted.parquet subset-ov.parquet \
  --bbox -48.0,-16.0,-47.0,-15.0 \
  --where "crop_type IN ('soy', 'corn') AND confidence >= 0.5"
```

### Combining many partition files

**`--files-from <manifest>`.** Converts the files listed in a manifest, one local
path or remote URL per line, with `#` comments and blank lines skipped. Each line
must be a single `.parquet` file, not a directory or glob, and local and remote
entries may be mixed. The line order defines the dataset row order, so the
manifest is both the input list and the ordering contract. Usage places the
manifest before the single positional output.

```bash
# manifest.txt: one .parquet path or URL per line, '#' comments allowed.
#   s3://gpq-tiles-demo/BR_north.parquet
#   s3://gpq-tiles-demo/BR_south.parquet
tylertoo overview \
  --files-from manifest.txt \
  brazil-ov.parquet \
  --min-zoom 0 --max-zoom 14
```

**Directory or glob input.** A partition set given as a positional path, for the
common case where the partitions sit together on disk and their listing order is
acceptable as the row order.
