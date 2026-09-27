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

**A file that cannot be tiled says so.** Two kinds of input tile to nothing, and
the converter counts both rather than reporting a silent success. Coordinates
that reach beyond the declared CRS's range — projected meters stored under
CRS84 metadata is the usual cause — are counted as dropped, and when the values
are large enough to be another CRS's units the warning names the `gpio convert
reproject` fix. Features with perfectly valid lon/lat that sit outside the Web
Mercator tiling domain (`|lat| > 85.05°`, an Arctic or Antarctic extract) are
counted separately: Web Mercator does not reach the poles, so there is nothing
to reproject and those features simply cannot be tiled. Both counts appear in
the run summary and in the conversion report
(`out_of_range_features`, `unprojectable_features`), and when the two together
account for 99% or more of the input, the conversion fails instead of writing an
empty archive. The out-of-range count also names its first few features, by row
and offending coordinate (`lon 180.548 (row 1041)`; the report's
`out_of_range_exemplars`), so a dateline cell or a stray projected point can be
looked up directly. The row is the row in the input file, counting any row
groups `--bbox` or `--filter` skipped; a directory or glob input also names the
part.

**Very large geometries shrink the read batch.** Arrow decodes a WKB column into
an array whose offsets top out at about 2 GiB per batch. When the input's rows
are large enough that a default 8192-row batch would pass that (rows averaging
over about 128 KB), the converter reads fewer rows per batch and logs a warning
saying so; the output is unchanged. Only geometries averaging over 2 GiB each
cannot be read at all, and the conversion then fails naming its row group.

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

**Every property column is encoded or warned about, never dropped silently.**
MVT values are strings, numbers and booleans, so each Arrow column type is
mapped onto one of those. Nested columns become JSON strings, which is what
tippecanoe does with nested GeoJSON attributes and what keeps an Overture
`names` struct or `sources` list on the feature instead of vanishing. A
column with no mapping at all is dropped, but the export says so once per
column (a `WARN` line naming the type), lists it in the export report under
`skipped_property_columns`, and refuses an `--include-property` that names
it with a message that says why.

| Arrow type | In the tile | `vector_layers` type |
|------------|-------------|----------------------|
| `Utf8`, `LargeUtf8` | string | `String` |
| `Boolean` | boolean | `Boolean` |
| `Int8`…`Int64`, `UInt8`…`UInt64` | integer | `Number` |
| `Float32`, `Float64` | float / double | `Number` |
| `Decimal128`, `Decimal256` | integer when the scale is ≤ 0 and the value fits 64 bits, else double | `Number` |
| `Date32`, `Date64`, `Time32`, `Time64`, `Timestamp` (naive or UTC) | ISO 8601 string | `String` |
| `Struct`, `List`, `LargeList`, `FixedSizeList`, `Map` | JSON string, one per row (`{"primary":"…","common":{"fr":"…"}}`, `["osm","meta"]`) | `String` |
| `Dictionary<_, T>` | as `T` | as `T` |
| `Binary`, `LargeBinary`, `FixedSizeBinary`, a `Timestamp` with a named zone | **dropped, warned once, counted in the report** | not advertised |

Inside a JSON string the leaves follow the same rules (a nested timestamp is
an ISO 8601 string, a nested decimal a number); a nested leaf with no mapping
renders as JSON `null`, and a null struct or list row carries no property at
all, like any other null. A `Map` with string keys becomes a JSON object;
with any other key type it becomes an array of `[key, value]` pairs. A
column you would rather see in another shape — a binary column as hex, a
struct flattened into `names_primary` — is a one-line `gpio` or DuckDB
projection before tiling, applied once to the input rather than at every
export.

## API walkthrough

### Meeting the coordinate-system contract

**`EPSG:4326` or `EPSG:3857`.** These are the two projections the converter
reads. A file already in lon/lat WGS84, like the Brazil fields source, needs no
conversion.

**`gpio convert reproject <in> <out> -d EPSG:4326`.** The fix when a file
arrives in another CRS. `gpio inspect` reports the current CRS, so you know
whether this step applies before you run it.

### Checking a file before you tile it

**`gpio inspect <file>`.** Reports the CRS, the row-group count and average
size, and the spatial overlap ratio. Reading these three before a long run
tells you which of the preparation steps below the file needs.

**`gpio check <file>`.** A pass/fail read of the same best-practice signals,
for scripting a gate into a pipeline rather than eyeballing the numbers.

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
