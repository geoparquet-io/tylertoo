# Preparing input for tiling

Everything downstream of the input inherits its shape. A well-prepared file
tiles faster, holds memory lower, and downloads fewer bytes when it lives in
object storage. This topic covers the GeoParquet contract tylertoo expects and
the one-time `gpio` pass that satisfies it, so the preparation you do once pays
off at every zoom level that follows.

The Getting Started tutorial ran this preparation in a single command. Here is
what each part of it buys you.

## Design decisions

**tylertoo reads WGS84 or Web Mercator GeoParquet only.** Tiling is a Web
Mercator operation, so the converter accepts either lon/lat degrees
(`EPSG:4326`) or Web Mercator meters (`EPSG:3857`) and projects between the two
itself. It does not carry a general reprojection engine. A file in any other
CRS places its features at the wrong tile coordinates rather than failing
loudly, so reprojecting to `EPSG:4326` first is the difference between a correct
map and a subtly broken one.

**Streaming memory depends on row-group size.** The converter reads one row
group at a time, so peak memory tracks the largest row group in the file, not
the file's total size. Row groups far below the target multiply per-read
overhead and starve throughput. Row groups far above it raise the memory floor
for every run. The 64–256 MB band keeps both in check.

**Hilbert order lets each tile read few row groups.** When features sit in
spatial order, the handful that fall inside a given tile cluster into a few
adjacent row groups, and the bbox covering statistics let the reader skip the
rest at the footer. In an unsorted file the same tile's features scatter across
the whole layout, so pruning finds nothing to skip and every tile pays to scan
everything.

**Covering statistics enable bbox and filter pushdown.** GeoParquet 1.1 records
a per-row-group bbox, and Parquet records per-column min/max. Together they let
the footer decide which row groups a `--bbox` or `--filter` can rule out before
a single data page loads. On a remote file those ruled-out bytes are never
fetched, which is where regional extracts earn their speed.

**Preparation belongs to gpio not tylertoo.** The two tools split the work
cleanly. `gpio` owns format preparation — reprojecting, sorting, repacking row
groups — and tylertoo owns tiling. This keeps each tool focused, and the
Hilbert sort and row-group sizing that `gpio` applies are the same
optimizations the streaming reader depends on.

## API walkthrough

### Meeting the coordinate-system contract

**`EPSG:4326` or `EPSG:3857`.** These are the two projections the converter
reads. A file already in lon/lat WGS84, like the Brazil fields source, needs no
conversion.

**`gpio convert reproject <in> <out> -d EPSG:4326`.** The fix when a file
arrives in another CRS. `gpio inspect` reports the current CRS, so you know
whether this step applies before you run it.

```bash
# Only when a file is not already EPSG:4326 or EPSG:3857.
gpio convert reproject \
  fields-utm.parquet \
  fields-wgs84.parquet \
  -d EPSG:4326
```

### Checking a file before you tile it

**`gpio inspect <file>`.** Reports the CRS, the row-group count and average
size, and the spatial overlap ratio. Reading these three before a long run
tells you which of the preparation steps below the file needs.

```bash
# CRS, row-group layout, and spatial overlap ratio.
gpio inspect brazil-2025-fields.parquet
```

**`gpio check <file>`.** A pass/fail read of the same best-practice signals,
for scripting a gate into a pipeline rather than eyeballing the numbers.

```bash
# Non-zero exit if the file misses a best-practice signal.
gpio check brazil-2025-fields.parquet
```

### Ordering features by spatial locality

**`gpio sort hilbert <in> <out>`.** Reorders features along a Hilbert
space-filling curve so geographic neighbors land near each other on disk. It
preserves the CRS and writes bbox covering metadata as it goes, so the sorted
file is ready for pushdown. A high overlap ratio in `gpio inspect` is the signal
that a file needs this.

### Sizing row groups for streaming

**`--row-group-size-mb 128`.** Repacks features into row groups near the
streaming target in the same pass as the sort. This is the knob that sets the
converter's memory floor, so a value inside the 64–256 MB band keeps peak RSS
bounded without fragmenting reads. Pair it with `--overwrite` to replace an
existing output.

```bash
# Hilbert-sort and repack row groups in one pass; the result is
# the brazil-sorted.parquet every later example feeds to tylertoo.
gpio sort hilbert \
  brazil-2025-fields.parquet \
  brazil-sorted.parquet \
  --row-group-size-mb 128 \
  --overwrite
```
