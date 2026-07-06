# Layout benchmark: duplicating vs partitioning vs partitioning + geom_overview

**Date:** 2026-07-06. Prepared for the gpq-tiles / geoparquet-overviews
(Youssef Harby) alignment discussion.

The question under test: the two drafts differ in how a file stores its
coarse levels. gpq-tiles `geo:overviews` offers **duplicating** (each level
is a complete, self-contained simplified rendering; a reader fetches exactly
one level) and **partitioning** (every feature exactly once, verbatim
geometry, at its coarsest meaningful level; a reader fetches a cumulative
prefix). Youssef's `overviews` draft is a third point in the same space:
**partitioning plus a `geom_overview` column** (every feature exactly once
with exact geometry; coarse bands additionally carry a simplified copy in a
second geometry column; a reader fetches the prefix but reads the cheap
column). This benchmark measures all three on the same dataset and then
tests whether his reference viewer can render our files.

A fourth layout was prototyped and measured after the first three:
**column-per-zoom** — every feature exactly once (like partitioning),
but with one simplified-geometry column per zoom level instead of one
`geom_overview`. It is Youssef's design with the column count turned up
from 1 to N, and it changes the conclusions; see §5.

## The verdict, in plain terms

**The two modes are not competitors. They serve different consumers, and
the benchmark shows each winning exactly where its design says it should.**

**Duplicating mode is for renderers.** Every zoom level is a complete,
pre-built rendering: thinned, simplified for that zoom, aggregated
(clusters, coalesced strokes), packed together on disk. A map client or a
PMTiles exporter reads one level and is done. It is a tile pyramid stored
as a Parquet file. In our measurements it was the cheapest thing to read
for map display at every single zoom level.

**Partitioning mode is for data.** Every feature is stored exactly once,
with its exact geometry, untouched. The file *is* the dataset — every SQL
engine, every `read_parquet()` call, every join gives correct answers with
zero special knowledge. That is the core promise of GeoParquet, and
partitioning-family layouts (ours, and Youssef's with `geom_overview`) are
the only ones that keep it.

**The downside of duplicating is NOT performance — read performance is
where it wins.** The downsides are:

1. **Wrong answers from normal tools.** This is the serious one. A plain
   `SELECT count(*)` on our duplicating file returns **9,136,026 rows.
   The dataset has 5,279,151.** `avg(height)` is silently skewed the same
   way. Every consumer that doesn't know to add `WHERE level = <canonical>`
   gets wrong numbers, with no warning. A duplicating file is not "the
   dataset plus overviews"; it is "a rendering product that contains the
   dataset if you know where to look."
2. **+60 % file size** (1,067 MB vs 665 MB here), and it grows with the
   number of levels materialized.
3. **Analytics scans pay for the duplicates** (21.7 MB / 10.7 s for the
   naive full-file aggregate vs 6.7 MB / 2.3 s on gpo's layout).
4. Bigger footers, slower writes, and any future update story must keep
   up to 8 copies of each feature consistent instead of one.

**The downside of partitioning is a capability list, not just a speed
gap.** Three things cannot live in a partitioning file, structurally
(§12.5 / §13.5 of our spec; same logic applies to Youssef's layout):
per-zoom simplification (each feature stores at most one reduced copy),
faithful cheap PMTiles export (coarse tiles need zoom-tuned geometry that
isn't there), and N:1 aggregation like clustering and coalescing (one
stored row cannot carry per-zoom counts or merged geometries without
either lying to SQL or being wrong at every zoom but one).

**A fourth layout, prototyped after the first three, combines most of
both:** column-per-zoom (§5) stores every feature once — so SQL is
correct — but carries one simplified-geometry column per zoom, so a map
reads exactly one zoom-tuned column. Measured: it matches duplicating's
read cost at every overview zoom, answers naive SQL exactly right, and
costs 882 MB (between partitioning's 665 and duplicating's 1,067 —
only geometry is copied per zoom, never ids or attributes). Its two
open costs: full-resolution street reads still pay partitioning's
scatter (~3× duplicating, page-index pruning unmeasured), and
coalescing still has no home outside duplicating.

**So the coherent shape for a merged spec is:** partitioning +
`geom_overview` as the base mode — it keeps the GeoParquet promise —
column-per-zoom as its natural extension where read latency matters
(same layout, more columns), and duplicating as an explicit,
loudly-documented opt-in for render-serving workloads (PMTiles export,
aggregation-heavy cartography), where breaking the naive-SQL promise is
the accepted price of pre-baked levels.

## Setup

| | |
|---|---|
| Dataset | 5,279,151 Overture building polygons, central Germany (bbox 7.8, 49.5, 10.2, 51.6), carved from the bigbench Germany extract |
| Input file | `buildings-de-central.parquet`, 591 MB, gpio-optimized (Hilbert sort, bbox covering, zstd) — comparable to the 5.65 M-building file in Youssef's published README numbers |
| gpq-tiles | 0.6.0 (workspace @ 319d147), `overview --mode {duplicating,partitioning} --min-zoom 0 --max-zoom 14`, defaults otherwise |
| gpo | yharby/geoparquet-overviews @ a269b3b, `gpo convert` defaults (3 bands, 16 MB row groups, zstd-15, bbox + native GEOMETRY types + page index) |
| Remote store | `s3://gpq-tiles-bench/layoutbench/` (us-east-2), benchmarked from a residential connection, cold reads, 3-run medians |
| Scripts | `bench_layout_reads.py` (viewer-pattern range reads), `bench_layout_sql.py` (DuckDB over S3); results in `layout_read_results.json` / `layout_sql_results.json` |

Known confounds, kept because they are each converter's real defaults:
gpo compresses at zstd-15 (ours default lower — some of its size win is
compression, not layout); gpo cuts 16 MB row groups vs our zoom-scaled
~1.9 MB medians (fewer/fatter row groups: cheaper footers and fewer
requests, coarser pruning); ours ships 15 levels z0–14 (8 non-empty)
vs gpo's 3 bands.

## 1. Conversion

| | wall | peak RSS | output | rows out |
|---|---|---|---|---|
| gpq-tiles duplicating | **32 s** | **1.3 GB** | 1,067 MB (1.80× input) | 9,136,026 (5.28 M exact + 3.86 M simplified copies) |
| gpq-tiles partitioning | **23 s** | **1.3 GB** | 665 MB (1.12×) | 5,279,151 |
| gpo | 139 s | 7.0 GB | 567 MB (0.96×) | 5,279,151 (+ geom_overview on coarse bands) |
| column-per-zoom (prototype) | 10.6 s pivot from the dup file (DuckDB) | 9.5 GB (pivot, unoptimized) | 882 MB (1.49×) | 5,279,151 (+ 7 zoom-geometry columns) |

The Rust converter is ~4–6× faster and uses ~5× less memory than the
reference Python converter (a converter-implementation fact, not a layout
fact — but it matters for whose implementation carries a merged spec).

**Storage is the clearest layout result:** duplicating pays +60 % file size
over partitioning on polygon data at z14 canonical. gpo's geom_overview
column costs almost nothing (the simplified coarse copies compress to a few
MB) while partitioning-without-it saves nothing more — i.e. *the overview
column is nearly free; the duplication is not.*

Footers: dup 985 KB (920 row groups), part 511 KB (533), gpo 119 KB (89).

## 2. Viewer-pattern reads (range requests against S3, cold, median of 3)

Read model identical for all three (footer, level-for-zoom, bbox-stat
row-group pruning, fetch only the rendering geometry column chunks, 8-way
parallel). Duplicating reads one level's own slice; the other two read the
cumulative prefix. gpo coarse bands read `geom_overview`; everything else
reads exact `geometry`. Page-index (sub-row-group) pruning was NOT modeled
for anyone — see caveats.

| view | dup | part | gpo | column-per-zoom |
|---|---|---|---|---|
| world z8 | 3 req / 0.99 MB / 1.4 s | 4 req / 0.56 MB / 1.7 s | 34 req / 4.7 MB / 2.3 s | 3 req / **0.59 MB** / **1.3 s** |
| regional z11 | 7 req / **4.1 MB** / 2.4 s | 12 req / 11.5 MB / 2.3 s | 18 req / 8.9 MB / 2.1 s | 8 req / 4.7 MB / 2.3 s |
| street z14 | 7 req / **4.5 MB** / 2.1 s | 17 req / 14.0 MB / 2.6 s | 7 req / 16.1 MB / 2.3 s | 12 req / 13.7 MB / 2.4 s |

(dup/part/gpo walls re-measured in the same session as the prototype;
bytes reproduced within noise of the first run.)

Readings:

- **Duplicating is the cheapest remote-read layout at every zoom.**
  Self-contained levels mean no cumulative reads, and the canonical band
  stays spatially packed, so street-level windows touch few, small row
  groups.
- **Pure partitioning (no overview column) pays exact-geometry prices at
  coarse zooms** and cumulative-prefix prices at fine zooms (3.1× dup's
  bytes at street). Its coarse renderings are, however, full-fidelity
  verbatim geometry — it fetches more and paints more detail.
- **geom_overview fixes partitioning's coarse zooms** (world/regional
  competitive) but gpo's street reads are inflated by the 16 MB final-band
  row groups (5 × ~3.2 MB chunks for a 0.02° window). His viewer's
  page-index pruning would claw a large part of that back; equivalently,
  zoom-scaled row-group sizing (our #202 result) would too. This row is a
  row-group-sizing artifact more than a layout verdict.
- gpo's 34-request world view is its 32-row-group band 0; request count is
  a coarse-band row-group-count choice (`--coarse-row-groups`), not
  intrinsic.

## 3. SQL-pattern reads (DuckDB over S3, cold, median of 3)

| query | dup | part | gpo | column-per-zoom |
|---|---|---|---|---|
| `SELECT count(*), avg(height)` naive | **WRONG: 9,136,026 / 5.13** — 21.7 MB / 11.0 s | 5,279,151 / 4.85 — 10.8 MB / 6.7 s | 5,279,151 / 4.85 — **6.7 MB / 2.3 s** | 5,279,151 / 4.85 — 8.1 MB / 7.5 s |
| same, correct (dup needs `WHERE level = 7`) | 10.9 MB / 12.9 s | n/a (naive is correct) | n/a (naive is correct) | n/a (naive is correct) |
| street window, full-res count + geometry | 9 req / **4.1 MB** / 2.0 s | 25 req / 12.7 MB / 1.9 s | 8 req / 16.3 MB / 2.2 s | 18 req / 12.6 MB / 2.0 s |
| regional window, full-res | 98 req / **46 MB** / 2.9 s | 147 req / 76 MB / 3.3 s | 41 req / 102 MB / 4.8 s | 129 req / 71 MB / 3.0 s |

Readings:

- **The duplicating footgun is real and measurable:** a plain
  `SELECT count(*)` returns 73 % too many rows and a skewed average unless
  the user knows to filter to the canonical level. Partitioning-family
  files answer naive SQL correctly with zero reader knowledge — this is
  the core GeoParquet-compatibility promise, and only they keep it.
- **Full-resolution window queries invert the ranking again:** dup's
  spatially-packed canonical band is 1.7–2.2× cheaper in bytes than
  either partitioning layout, whose full-res features are scattered
  across every band the window touches (locality dilution). Note the
  wall clocks stay comparable — on this connection latency is dominated
  by round trips, not bytes.
- gpo's regional window reads 102 MB — fat final-band row groups again.

## 4. Viewer compatibility (Youssef's TypeScript viewer, our files)

Fork: local clone @ a269b3b, branch `gpq-tiles-compat`.

**Unpatched:** the viewer looks up the literal `overviews` footer key,
doesn't find ours (`geo:overviews`), and falls back to plain-GeoParquet
mode — our file still renders (graceful degradation works exactly as both
specs intend), but with no pyramid: the initial view cost **27 MB /
24 requests / 23 s** to paint.

**The patch is small and entirely contained in `metadata.ts`:**
50 lines changed in one source file (+ tests). It (a) also accepts the
`geo:overviews` key, (b) maps `zoom` → `max_zoom` and synthesizes level
ordinals, (c) adds a `duplicating` mode whose levels read their own
row-group slice instead of the cumulative prefix. All 192 of his existing
tests still pass, plus 6 new dialect tests; typecheck clean.

**Patched, against S3 with a real headless browser:**

| file | first view | notes |
|---|---|---|
| gpo (control) | 4.4 MB, painted 64,031 features | unmodified behavior |
| ours part | **0.58 MB / 5 req / 2.2 s**, 81 features @ z7 | full 8-level ladder recognized, exact geometry per level |
| ours dup | **0.99 MB / 5 req / 2.0 s** @ z7; 3.5 MB total after zooming to z11 (39,104 features from 4 row groups) | slice reads work; screenshot verified |

The two formats are structurally the same convention: band-major row
groups + per-level `row_group_end` + `gsd`. The differences a reader must
handle are the key name, two field-name spellings, one mode flag, and
his `geom_overview` column — about an afternoon of work, most of it tests.

## 5. Column-per-zoom prototype (measured, not hypothetical)

The idea: keep every feature as exactly one row (like partitioning), but
instead of ONE `geom_overview` column, write one simplified-geometry
column per zoom — `geom_z7` … `geom_z13`, plus the exact `geometry` for
z14. A map client picks the single column tuned for its zoom and reads
nothing else; Parquet's columnar projection makes the other columns
free. It is Youssef's design with the column count turned up from 1 to N
— rows-of-copies replaced by columns-of-copies.

Built by pivoting the duplicating file (which already holds every
per-zoom geometry, as rows) with a DuckDB `GROUP BY id`: 10.6 s,
`buildings-de-central.cpz.parquet`, 882 MB, 516 row groups. Rows are
ordered band-major (coarsest-visible feature first, preserving the
Hilbert order within bands), which makes each zoom column's values
pack into a contiguous row-group prefix: `geom_z8`'s data lives
entirely in row group 0, `geom_z11`'s in row groups 0–8, `geom_z13`'s
in 0–312. **The footer's per-chunk null statistics alone tell a reader
which row groups hold a zoom's data — no level metadata is strictly
required.** (A `geo:overviews` v0.3.0-proto key with a zoom→column
mapping is written anyway.)

What the measurements show (tables in §2/§3 above):

- **Map reads: matches duplicating at every overview zoom.** World
  0.59 MB (cheapest of all four), regional 4.7 vs dup's 4.1 MB.
  The zoom column is self-contained — no cumulative prefix reads.
- **SQL: exactly correct, naively.** count 5,279,151, avg 4.847,
  8.1 MB scan — no `level` filter to know about, nothing to get wrong.
- **Size: 882 MB** — between partitioning (665) and duplicating
  (1,067), because only geometry is copied per zoom; ids, attributes,
  and bbox stay single.
- **Still open:** full-resolution reads (street window, full-res SQL)
  track partitioning, ~3× duplicating — the exact-geometry column is
  spread across the zoom bands rather than packed in one canonical
  band. Page-index pruning should recover much of this; unmeasured.
  And N:1 aggregation: per-zoom `point_count` columns likely give
  clustering a coherent home here, but coalescing's merged strokes
  still don't fit any single-row-per-feature layout.
- Two ergonomic notes: the zoom columns are ordinary top-level columns,
  so hyparquet (his viewer's reader, which cannot project struct
  fields) and `geo.columns` declarations both work — the flat-columns
  variant dodges the objections a struct would raise. The one new
  ergonomic cost is that `SELECT *` drags all eight geometry columns;
  consumers should project, which is standard Parquet advice anyway.

### What do all the NULLs cost? Almost exactly nothing.

Column-per-zoom means most values in most zoom columns are NULL (a
building invisible at z8 has NULL in `geom_z8`). Parquet stores a run
of NULLs as a run-length-encoded bitmap, so an entire all-NULL column
chunk costs ~36 bytes. Measured on the prototype:

| column | real data | NULL overhead (whole file) |
|---|---|---|
| geom_z7–z10 | 0.9 MB | 74 KB |
| geom_z11 | 5.5 MB | 18 KB |
| geom_z12 | 33.7 MB | 17 KB |
| geom_z13 | 186.0 MB | 7 KB |
| geometry (exact) | 390.5 MB | 0 |

**Total NULL overhead: ~116 KB in an 882 MB file — 0.013 %.** The size
premium is entirely real geometry, and it is dominated by the finest
overview level: z13 alone is 186 MB of the 227 MB of overview data.
That exposes a cheap tuning knob: stop the ladder one level earlier and
let z13 render the exact geometry (only ~2× over-detailed at that
zoom), and the premium drops from 227 MB to ~41 MB — a 6 % file-size
premium for duplicating-grade reads at z7–z12.

### PMTiles generation from a column-per-zoom file

Essentially unimpacted — arguably improved. The exporter needs, for
each tile at zoom z, geometry already generalized for zoom z. In the
duplicating file that is "level z's row slice"; in the column-per-zoom
file it is "column `geom_z`" — same content, different axis. Export is
a streaming full-file pass, so the read-locality differences that matter
for viewport queries don't matter here. Our `export.rs` already
abstracts the source behind a reader; mapping level→column instead of
level→row-slice is a modest change.

What the export loses from the stored file is coalescing (and any
aggregation richer than winner + count). The right home for those is
the exporter itself: tippecanoe's `--coalesce` and clustering are
tile-generation-time operations, not source-format features — the
merged stroke exists in the tile, which is a rendering, and nowhere
else. That keeps the GeoParquet file honest (every row a real feature)
while the PMTiles output keeps full cartographic polish. The cost is
CPU at export time instead of at convert time, paid once per export.

### Do we need aggregation in the format at all?

Mostly no — and Cloud-Optimized GeoTIFF is the precedent for where to
draw the line. COG overviews aggregate aggressively (every overview
pixel is an average/mode/nearest of four below), but COG stores no
provenance: nobody can ask which pixels produced an average, the
resampling method is a write-time choice declared in metadata, and the
overview is understood to be a rendering product. Adopting the same
stance here — **a zoom column is "what to draw at this zoom," not "this
row's geometry, simplified"** — settles the three cases:

- **Density dropping / thinning: solved.** NULL at that zoom. Free
  (see above).
- **Clustering: keep, in reduced form — it needs no relationship
  machinery.** Duplicating mode already stores no relationships: one
  surviving "winner" point absorbs its neighbors and only a count is
  recorded. Column-per-zoom translation: winner keeps its point in
  `geom_z5`, a small `count_z5` column says "represents ~120 points
  here," absorbed rows are NULL there. One integer column per zoom,
  opt-in. Worth keeping because thinning *destroys* density honesty (a
  z5 POI map looks the same for 500 or 50,000 places) and a client
  cannot reconstruct counts from points it never downloaded.
- **Coalescing: drop from the storage format.** Its correctness benefit
  is smaller than it looks — class-ranked assignment puts all motorway
  segments at the coarse level where they render continuously by
  adjacency, no merging needed; what coalescing buys is weight and
  polish. And it is the only feature that genuinely cannot fit a
  one-row-per-feature file (a merged stroke belongs to 200 rows at
  once). A feature that is both skippable and the only source of
  architectural complexity should be skipped — it moves to the PMTiles
  exporter, per above.

## Can partitioning reach read parity with duplicating?

Short answer: **close, but not equal — and the remaining gap has a
structural floor.** Where the deficit comes from, and what closes it:

Duplicating won the read benchmark for three separable reasons.

**1. Geometry weight at coarse zooms — CLOSED by `geom_overview`.**
Our pure partitioning ships exact vertices at every zoom; that is most of
its regional-view deficit (11.5 MB vs dup 4.1 MB). Youssef's overview
column fixes this (8.9 MB), at near-zero storage cost. Already done in
his layout.

**2. Row-group granularity — CLOSED by sizing + page pruning.**
gpo's 16 MB row groups made its street reads fat (16.1 MB vs dup 4.5 MB):
a 0.02° window drags in five ~3 MB chunks. Two known fixes compose:
zoom-scaled row-group sizing (our #202 sweep: small groups where windows
are small) and page-index pruning (his Profile A + viewer already do
sub-row-group reads; our benchmark model didn't simulate them). Neither
is speculative; both are implemented somewhere today.

**3. Locality dilution — BOUNDED, not closable.** This is the floor.
At any zoom, duplicating's visible features sit in ONE Hilbert-packed
band: one locality island, minimal overfetch. In a partitioned file the
same features are spread across every band in the prefix (up to 8 here),
so a viewport always touches k islands instead of 1, and each island
contributes its own partial-row-group / partial-page overfetch, its own
requests. Page pruning shrinks each island's waste toward one page
(~128 KB); level `extent` fields let a reader skip bands that miss the
viewport entirely; but k islands never become 1. Back-of-envelope with
fixes 1+2 applied: street-view reads land roughly 1.2–1.5× duplicating's
bytes rather than today's 3×. Parity-adjacent for real UX (walls here
were request-latency-bound anyway: 2.2 s vs 3.4 s at street), but not
parity.

**What can never reach parity in row-partitioning:** per-zoom
simplification fidelity (each feature stores at most ONE reduced copy, so
any zoom except its band's target renders either cruder or heavier than
duplicating's zoom-tuned copy) and aggregation (clusters/strokes).

**The one design that gets genuinely all the way there is
column-per-zoom — and it is now measured, not speculative (§5):**
generalize `geom_overview` to one overview column per level. That is
duplication of *geometry into columns* instead of *features into rows* —
a reader fetches exactly the one zoom-tuned column it needs, so the
read profile matches duplicating level-for-level at overview zooms
(measured: world 0.59 MB, regional 4.7 MB), while rows stay unique and
naive SQL stays correct (measured: exact counts). Storage lands at
882 MB — 33 % over partitioning, 17 % under duplicating — because only
geometry is copied. It trades the footgun for file size — the right
trade, since size is visible and wrong answers are not. Remaining gaps:
full-resolution reads keep partitioning's scatter (page pruning
unmeasured), and coalescing still fits only duplicating.

## What this means for a merged spec

1. **Base mode: partitioning + `geom_overview`** (Youssef's shape) with
   our zoom-scaled row-group sizing and page-index pruning — keeps the
   GeoParquet promise, near-free storage, parity-adjacent map reads.
2. **Its natural extension: column-per-zoom** — the same layout with N
   overview columns instead of 1, when read latency or per-zoom
   simplification quality justify the storage. Prototyped and measured
   (§5): duplicating-grade overview reads + exact naive SQL at 882 MB.
   The base mode is the N=1 special case, so this is one design with a
   knob, not two modes.
3. **Opt-in mode: duplicating**, for PMTiles export and
   aggregation-heavy cartography (coalescing lives only here), with the
   naive-SQL hazard documented in the spec itself, not a footnote.
4. **Reader convergence is cheap.** One afternoon of dialect handling
   made his viewer render both of our modes. A merged spec that settles
   key name (`overviews` vs `geo:overviews`), `zoom` vs `max_zoom`, and
   level ordinals removes even that.

## Reproduce

```bash
# subset (from bigbench Germany extract, needs spatial ext)
duckdb -c "INSTALL spatial; LOAD spatial; COPY (SELECT id, \
  building_class, height, geometry FROM read_parquet(\
  'corpus/data/bigbench/gpio/overture-germany-buildings.parquet') \
  WHERE bbox.xmin BETWEEN 7.8 AND 10.2 AND bbox.ymin BETWEEN \
  49.5 AND 51.6) TO 'buildings-de-central.raw.parquet' \
  (FORMAT PARQUET, COMPRESSION ZSTD);"
uv run --project ~/Documents/dev/geoparquet-io gpio sort hilbert \
  buildings-de-central.raw.parquet buildings-de-central.parquet \
  --add-bbox --geoparquet-version 1.1 --compression zstd --overwrite

# conversions
target/release/gpq-tiles overview --mode duplicating \
  --min-zoom 0 --max-zoom 14 buildings-de-central.parquet \
  buildings-de-central.dup.parquet
target/release/gpq-tiles overview --mode partitioning \
  --min-zoom 0 --max-zoom 14 buildings-de-central.parquet \
  buildings-de-central.part.parquet
uvx --from "git+https://github.com/yharby/geoparquet-overviews\
#subdirectory=converter" gpo convert \
  buildings-de-central.parquet buildings-de-central.gpo.parquet

# column-per-zoom prototype (pivots the dup file's per-level rows
# into per-zoom columns; run from corpus/data/layoutbench/)
duckdb < benchmarks/layout/make_cpz.sql

# benchmarks (uploads assumed at s3://gpq-tiles-bench/layoutbench/)
uv run --with pyarrow --with requests \
  python3 benchmarks/layout/bench_layout_reads.py
uv run python3 benchmarks/layout/bench_layout_sql.py
```
