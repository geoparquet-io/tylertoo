# How tylertoo relates to tippecanoe

tylertoo takes its name from the campaign slogan "Tippecanoe and Tyler Too,"
and the debt is real: [tippecanoe](https://github.com/felt/tippecanoe) is the
reference implementation the tiling algorithms are measured against. Many
readers arrive already knowing it and want to know how the two line up. This
topic answers that as a factual comparison, not a migration guide. It maps the
concepts you know onto tylertoo, states what each tool does that the other does
not, and stays neutral about which to use, because the two solve overlapping but
different problems.

## Design decisions

**tylertoo reads GeoParquet directly, structure and all.** tippecanoe reads
GeoJSON, line-delimited GeoJSON, FlatGeobuf, and point CSV. A GeoParquet dataset
tiled through tippecanoe first converts to one of those, usually GeoJSON, which
rewrites a compact columnar file into larger text. tylertoo reads the GeoParquet
as it is, and the throughput difference compounds from several sources rather
than the skipped conversion alone. It decodes only the geometry column a tile
needs instead of parsing whole text features. It uses the file's Hilbert
ordering and bbox covering statistics to read only the row groups a tile or a
`--bbox`/`--filter` touches, including byte ranges on remote input, where a
GeoJSON stream carries no spatial index to skip or seek on. Both tools are
compiled native code, so the leverage is the data path, not the language. The
demo page carries the measured numbers.

**Overviews embed levels inside the input format.** tippecanoe generalizes in
tile space, per tile, at encode time, and writes the result into tiles. tylertoo
generalizes in world space, per level, and stores those levels in a GeoParquet
file. This is the core format difference: a tylertoo level is a reusable,
exact, SQL-queryable row band, where a tippecanoe tile is a rendered endpoint.
The overview archive that results has no established equivalent.

**Quality-ladder knobs mirror tippecanoe concepts.** Feature dropping, buffers,
layer naming, zoom ranges, and simplification all have direct tylertoo
counterparts, because tippecanoe defined the vocabulary. The numeric defaults
differ where the mechanism is anchored differently, and those divergences are
documented rather than incidental.

**Parity sets the performance bar.** The goal for output quality is to match
tippecanoe on a shared corpus, and the pipeline is validated against tippecanoe
output as it changes. Where tylertoo diverges, it is a deliberate, recorded
choice in `context/ARCHITECTURE.md`, not drift.

**Decode returns tiled geometry not source data.** Both tools can turn tiles
back into features, and in both the result is the tiled representation —
simplified, clipped, and duplicated across tiles — never the original source.
tylertoo's decoder follows tippecanoe-decode's model deliberately.

## The measured comparison

"Faster than tippecanoe" needs a number, a version and a caveat list, so
there is a harness: `benchmarks/e2e/` pins **tippecanoe 2.79.0** by tag and
commit, builds it from source, converts the same GeoParquet to tippecanoe's
best input format, and holds zoom range, layer name, tile buffer and per-tile
byte cap equal on both sides. Full method, the flag-by-flag parity mapping and
the asymmetries that flags cannot close are in `benchmarks/e2e/README.md`;
measured numbers are in `benchmarks/e2e/RESULTS.md`.

Two things a skeptical reader should know before reading any ratio.

**At defaults the two tools do not draw the same map.** tylertoo's density
budget thins features at coarse and mid zooms. tippecanoe drops only points,
so on a contiguous polygon coverage it emits every feature at every zoom. On a
17,465-polygon admin-boundary dataset, tylertoo's default z8 tile set carries
865 of those polygons and tippecanoe's carries all 17,465. If your layer is a
coverage rather than a sample — admin boundaries, parcels, a choropleth — that
is the behaviour to change (`--verbatim`, or raise `--gsd-base` and lower the
thinning factors; see [Tuning what appears at each zoom](tuning-zoom.md)), and
it is the configuration any honest speed comparison has to use.

**The GeoParquet read is not where most of the speed comes from.** On that
same dataset the GeoParquet→FlatGeobuf conversion a tippecanoe user must run
is about 1% of tippecanoe's end-to-end wall. The native columnar read saves a
43 MB intermediate file and is what makes `--bbox`/`--filter` row-group
pushdown and remote byte-range reads possible at all, but the tiling engine
is carrying the ratio.

With those stated, on that dataset (Apple M3 Pro, 12 cores, z0–z14, median of
3 warm runs):

| comparison | tylertoo | tippecanoe 2.79.0 | ratio |
|---|---|---|---|
| Same features at every zoom (`--verbatim --simplify-factor 1.0`) vs tippecanoe reading FlatGeobuf | 2.67 s | 20.05 s | **≈7.5×** |
| Each tool at its own defaults, end to end from GeoParquet | 1.61 s | 20.24 s | **≈10–12×** |
| Output archive, quality-matched | 118 MB | 151 MB | 0.78× |
| Peak RSS, quality-matched run | 760 MB | 413 MB | 1.8× (tylertoo heavier) |

The quality-matched row lands within two tiles of tippecanoe's count at every
zoom, which is how we know it is like-for-like. The defaults row is a range
because two runs an hour apart on the same idle laptop differed by 20%. The
memory row is a loss, not a win, and `--partition-wave` is the knob.

The corpus is three in-repo fixtures, the largest 28 MB; nothing here says how
either tool behaves at planet scale, on points, or on cold cache.
`benchmarks/e2e/RESULTS.md` lists what is untested.

## API walkthrough

### Mapping tippecanoe concepts to tylertoo

The concepts carry over; the flags and some defaults change. tylertoo applies
these to overview levels rather than to tiles at encode time, so a knob shapes a
stored, reusable level.

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-l` layer name | `--layer-name` | Set at export |
| `-L` one layer per input | `pyramid --band LO-HI:INPUT:LAYER …` with bands sharing a zoom range | Each band is its own ladder; tiles at shared zooms carry every layer |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-y` / `-x` / `-X` property selection | `--include-property` / `--exclude-property` / `--exclude-all-properties` | Applied at scan time on `overview` / `tiles`, at export on `export-pmtiles`. Same precedence: an include list wins and `-x` / `-X` are then ignored |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder; tylertoo anchors on the full canonical count, so the default differs |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell, leaving per-level totals unchanged |
| `-S` simplification | `--simplify-factor` | RDP, cascading by default |
| `--drop-fraction-as-needed` tile-size loop | `--tile-size-limit` | Single non-iterative drop pass, since levels are already budgeted |
| tiny-polygon reduction | `--collapse-square` | Area accumulator per 32×GSD patch of the level (tile-less), plus a per-feature dither for write-time collapses |
| cluster centroid | `--cluster` | Winner keeps its own geometry and absorbs losers into `point_count` |
| `--coalesce` family | coalescing (on by default) | Chains same-class segments before gates and thinning |

### What only tylertoo does

**Reads GeoParquet directly.** The columnar source is the input, with no GeoJSON
conversion. Remote objects read by byte range, and `--bbox` and `--filter` push
down to skip row groups at the footer, so a run can carve a filtered slice out of
a planet-scale remote collection while tiling it.

**Writes an embedded overview file.** The world-space levels live in a valid
GeoParquet file you can query with DuckDB, re-export more than once, and validate
against the `geo:overviews` spec. tippecanoe's output is the tileset; there is no
intermediate you can open as data.

### What tippecanoe does that tylertoo does not

**Reads more input formats.** GeoJSON, line-delimited GeoJSON, FlatGeobuf, and
point CSV, plus GeoJSON on standard input. tylertoo reads GeoParquet in
`EPSG:4326` or `EPSG:3857` and nothing else, on the expectation that `gpio`
converts other formats first.

**Ships tileset tooling.** `tile-join` merges tilesets and joins CSV attributes
onto existing features, `-zg` guesses a maximum zoom from feature spacing, and
`-e` writes a directory of tiles. tylertoo has no equivalent to these; it
writes one PMTiles archive or the overview file. (`-L`, one layer per input
file, is covered by `pyramid` bands that share a zoom range.)

### Decoding tiles back

**`tylertoo decode`.** Turns a PMTiles archive back into GeoParquet, following
tippecanoe-decode's semantics. Nothing is deduplicated, so a feature appears once
per tile it touched, with `zoom`, `layer`, and `mvt_id` provenance columns for
filtering to one representation. Coordinates lift through tippecanoe's 32-bit
world-coordinate transform. Because tiling simplifies, clips, and drops
attributes, the output is the tiled geometry, not a route back to the source
file.
