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

## API walkthrough

### Mapping tippecanoe concepts to tylertoo

The concepts carry over; the flags and some defaults change. tylertoo applies
these to overview levels rather than to tiles at encode time, so a knob shapes a
stored, reusable level.

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-l` layer name | `--layer-name` | Set at export |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder; tylertoo anchors on the full canonical count, so the default differs |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell, leaving per-level totals unchanged |
| `-S` simplification | `--simplify-factor` | RDP, cascading by default |
| `--drop-fraction-as-needed` tile-size loop | `--tile-size-limit` | Single non-iterative drop pass, since levels are already budgeted |
| tiny-polygon reduction | `--collapse-square` | Per-feature area dither instead of a per-tile accumulator |
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
onto existing features, `-zg` guesses a maximum zoom from feature spacing, `-L`
names layers per input file, and `-e` writes a directory of tiles. tylertoo has
no equivalent to these; it writes one PMTiles archive or the overview file.

### Decoding tiles back

**`tylertoo decode`.** Turns a PMTiles archive back into GeoParquet, following
tippecanoe-decode's semantics. Nothing is deduplicated, so a feature appears once
per tile it touched, with `zoom`, `layer`, and `mvt_id` provenance columns for
filtering to one representation. Coordinates lift through tippecanoe's 32-bit
world-coordinate transform. Because tiling simplifies, clips, and drops
attributes, the output is the tiled geometry, not a route back to the source
file.
