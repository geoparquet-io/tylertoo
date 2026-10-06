# Overview generalization tuning

`tylertoo overview` builds an
[overview file](tutorials/madagascar.md#3-build-and-validate-the-overview)
with precomputed generalizations of a GeoParquet dataset. Level `0` is the
coarsest; level `L-1` is the finest, or canonical, level. Two kinds of
settings control detail:

- Thinning and visibility decide which features survive.
- Simplification decides how many vertices each survivor keeps.

These factors multiply the level's ground sample distance (GSD): the smallest
ground distance, in meters, that it resolves. `--gsd-base` or `--gsd` sets
the GSD ladder. Use [Worked scenarios](#worked-scenarios) to find settings
for a specific problem. The
[CLI reference](reference/cli.md) lists every flag with its default.

---

## The GSD ladder: `--gsd-base` and `--gsd`

A zoom-range plan (`--min-zoom` and `--max-zoom`, the default) derives each
level's GSD from its Web Mercator zoom:

```
gsd(z) = 40075016.69 / gsd_base / 2^z          (meters, spec §5.2)
```

| Knob | Default | Units | Effect |
|------|---------|-------|--------|
| `--gsd-base F` | `1024.0` | dimensionless | base of the formula above |
| `--gsd G1,G2,…` | none | meters, strictly decreasing | explicit per-level GSDs that **override** the zoom range and `--gsd-base` |
| `--min-zoom` / `--max-zoom` | `0` / `6` (`0` / `14` on `tiles`) | Web Mercator zoom | coarsest and finest (canonical) level, at most **30** |

`--gsd-base` scales the whole zoom-range ladder:

- A larger value gives smaller GSDs: denser, more detailed, larger levels.
- A smaller value gives larger GSDs: sparser, less detailed, smaller levels.

The default `1024` follows the cogp-rs convention: about 4× a 256 px tile, so
sub-pixel features drop (spec §5.2, Q6). `--gsd` already gives absolute
meters, so `--gsd-base` has no effect alongside it.

The thinning, visibility, and simplification factors all multiply GSD.
Adjust them individually for one geometry class; adjust `--gsd-base` when
the whole map is too sparse or too dense.

The `geo:overviews` provenance in the footer records a non-default base as
`generalization.gsd_base`. A default run omits it, because the
`levels[].gsd` values in the footer already imply `1024`.

### `--max-zoom auto`

`--max-zoom auto` estimates the finest zoom from the input's own features, as
tippecanoe's `-zg` does. tylertoo reads the bounding boxes of a deterministic
sample of at most 200,000 rows, honoring `--bbox` and `--filter`. It picks the
zoom where one pixel of a 256 px tile resolves about half the smaller of two
measures, in Web Mercator meters:

- the median size of the features that have an extent
- the tenth-percentile spacing between nearby features

The result is clamped to `[--min-zoom, 16]` and logged with its measurements
at `info`. Treat it as a starting point. Python's `logging` does not receive
the Rust log; read the finest `zoom` in the report's `levels` instead.

An input with nothing to measure, such as one location or only empty
geometries, is an error, as with `-zg`. So is a `--min-zoom` above 16. With
`--gsd`, tylertoo ignores the value and estimates nothing. The divergence
table in `context/ARCHITECTURE.md` compares the formula with tippecanoe's.

### The zoom ceiling

tylertoo rejects any zoom above 30 at option validation, before it opens the
input. The limit marks what tile arithmetic can address, not what is useful.
Tile coordinates are 32-bit, so z31 is the hard limit, and the PMTiles
Hilbert tile id needs `4^z` of headroom in a `u64`. Thirty leaves one level of
margin. The same ceiling applies to `--min-zoom` and to a `--gsd` ladder fine
enough to imply a zoom above 30.

Cost grows about **4× per zoom**: z30 holds about 1.15e18 tiles at a GSD of
about 3.6e-5 m. A point touches one tile per zoom, so point data stays cheap.
Lines and polygons do not: one 1-degree linestring spans on the order of 3e6
tiles at z30. For non-point data, raise `--max-zoom` one level at a time and
watch the output size.

---

## Several inputs, one archive: `tylertoo pyramid`

A pyramid combines inputs in one archive, typically summaries at coarse
zooms and raw features at fine zooms. The client switches between them
without a second request.

```bash
tylertoo pyramid fire-2023.pmtiles \
  --band "0-5:cells_r5.parquet:aggregate" \
  --band "6-8:cells_r8.parquet:aggregate" \
  --band "9-14:points.parquet:features"
```

| Layer | Source | Zooms |
|-------|--------|-------|
| `aggregate` | r5 cells | z0–5 |
| `aggregate` | r8 cells | z6–8 |
| `features` | raw detections | z9–14 |

Bands may share a layer name: `aggregate` changes source at z6 here. Bands
in the same layer must have disjoint zoom ranges, or they would write the
same tile ids. tylertoo checks this before conversion.

Bands in different layers may share zooms, as with tippecanoe's `-L`.
Shared tiles combine the layers; tiles from only one band pass through
untouched. For example,
`--band 0-13:a.parquet:2024 --band 0-13:b.parquet:2025` builds one archive
with two independent layers over the same zooms.

Bands tile verbatim by default to preserve their input resolution. Pass
`--generalize` for raw features that need generalization across several zooms.

### Band sources and syntax

A band accepts GeoParquet or PMTiles via `--band LO-HI:PATH[:LAYER]`.
tylertoo detects the format from the contents, tiles GeoParquet over the
band's range, and merges an archive as-is. Mix formats to reuse a coarse
archive while rebuilding a fine band.

A GeoParquet band may be remote:
`--band 9-13:https://data.source.coop/…/features.parquet` streams with
byte-range requests, as `tiles` and `overview` do. Archive bands must be
local for directory and tile reads by offset. Stage remote `.pmtiles` bands
before use.

`LAYER` defaults to the input's file stem, or the
directory name for a glob or a directory. A path may contain colons
(`s3://…`, `https://…`, `C:\…`). tylertoo splits the layer off the **last**
colon only when the text after it has no `/`, `\`, or `:`. A URL scheme, a
port, and a `2024:06/` directory therefore stay part of the input. A
drive-relative path such as `C:data.parquet` also stays whole: a single ASCII
letter before the last colon counts as a drive letter.

For an input ending in a bare colon segment, such as the Hive directory
`admin:country_code=BR`, use `--band LO-HI=INPUT[=LAYER]`.
tylertoo splits the range at
the first `=` and the layer at the last. As before, it splits only when the
text after it has no `/`, `\`, or `:`. That keeps `--band 0-13=admin:country_code=BR/part.parquet`
whole. An input that itself ends in `=VALUE` reads as a layer, so give it an
explicit `=LAYER`.

For a pre-tiled archive the layer is a label. The layer name inside its tiles
is whatever its export wrote. When two bands share zooms, those names must
agree. Otherwise the combined tile holds two layers of one name, and a client
drops one.

### Gaps and partial archives

A gap between bands warns: the merged archive
advertises one continuous range, so a client requests the uncovered zooms and
gets nothing. An archive may hold more zooms than its band declares. One
archive splits across bands that way, for example two bands on the same
z0–13 archive declaring `0-5` and `6-13`. An archive that holds *fewer* zooms
than its band declares is an error, because those zooms would render empty.
Check that the band range matches the archive, or pass
`--allow-missing-zooms` for a deliberately sparse pyramid. A band whose range
shares no zoom with its archive is always an error.

### Export settings and disk use

`--max-tile-size` is unset by default for pyramid bands, preserving every
cell. When set,
the cap applies to each band's tiles before tylertoo combines them.
`--feature-order` applies to every GeoParquet band alike, as `--generalize`
does. A band that lacks the column exports in input order with a warning
naming its layer. An archive band keeps the order it had. tylertoo writes
bands coarsest first, whatever order you list them in.

Intermediates go to `--work-dir` (the system temp directory by default) and
are removed after success or failure. Budget roughly *levels × input size*
for each band's duplicating-mode overview. All finished band archives remain
on disk until the merge, which also needs temp space for its spool.

---

## Switching the ladder off entirely: `--verbatim`

Generalization works for features such as roads or buildings that need less
detail at coarse zooms. For cell aggregates or prebuilt levels, it can discard
data the input already summarizes. An H3 r6 cell holds the sum of its r7
children; dropping cells changes the totals:

```
$ tylertoo tiles cells.parquet out.pmtiles --min-zoom 0 --max-zoom 5
WARN omitting 1 empty level(s) [0] … none of the 3399 input feature(s)
     are visible at those scales (visibility gates / density budget)
  z2  (level 0):  757 features      ← 3,399 cells in, 757 drawn
  z3  (level 1): 1446 features
```

`--verbatim` turns the whole ladder off, so every level reproduces the input:

```
$ tylertoo tiles cells.parquet out.pmtiles --min-zoom 0 --max-zoom 5 --verbatim -f
  z0  (level 0): 3399 features
  z1  (level 1): 3399 features
  z2  (level 2): 3399 features
  …
```

Its output is byte-identical to setting eight ladder knobs by hand:

```bash
--no-density-drop --no-coalesce-lines --simplify-factor 0 \
  --point-thinning 0 --line-thinning 0 --polygon-thinning 0 \
  --polygon-visibility 0 --line-visibility 0
```

Use `--verbatim` for:

- A discrete global grid system (DGGS) or other cell aggregate.
- Levels already built upstream.
- One band of a [pyramid](#several-inputs-one-archive-tylertoo-pyramid)
  whose coarser zooms come from another input.

It leaves the mode, the level plan, the coordinate reference system (CRS),
the row-group layout, an explicit `--cluster`, and `--representation` alone.
A requested point or square band still replaces polygons with centroids or
placeholder squares.

Explicit settings override `--verbatim` defaults. For example,
`--verbatim --simplify-factor 0.5` keeps every feature but simplifies its
geometry. The report identifies the override.

It requires `--mode duplicating`. Partitioning writes each feature at exactly
one level. With thinning off, every feature would land in the coarsest level
and every finer level would be empty, so tylertoo rejects the combination.

On `tiles`, `--verbatim` also disables the per-tile size cap. An explicit
`--max-tile-size` restores it.

For two-step builds, `tylertoo export-pmtiles` still applies its default
500K cap after `tylertoo overview --verbatim`. Pass `--tile-size-limit 0`
(alias `--max-tile-size 0`) to export every feature.

### `0` is the off switch

Every thinning factor accepts `0`, meaning **no thinning**: each feature is
its own grid cell, so every feature that passes the gates survives. Negative
and non-finite factors are errors.

---

## Feature thinning: `--point/line/polygon-thinning`

Thinning keeps one winning feature per grid cell per level. The
[ranking](#ranking-which-feature-wins-a-cell) picks the winner. The cell size
for each geometry kind is:

```
cell_size = thinning_factor * gsd(level)          (in the CRS's units)
```

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--point-thinning` | `4.0` (`16.0` with `--cluster`) | × GSD (cell size) | **bigger = sparser** |
| `--line-thinning` | `1.0` | × GSD (cell size) | **bigger = sparser** |
| `--polygon-thinning` | `1.0` | × GSD (cell size) | **bigger = sparser** |

Larger factors give larger cells and fewer survivors. Lower
`--line-thinning` if coarse roads look too sparse.

Points use 4.0 because they clutter sooner than lines and polygons, which
use 1.0. In the Portland roads sweep, 1.0 kept roads more continuous at
coarse zooms with little loss of legibility
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)).
The factor multiplies the GSD, so it stacks with `--gsd-base`. Doubling
`--gsd-base` halves the GSD, which halves the cell size for the same factor.

---

## Visibility gates: `--line/polygon-visibility`

A line or polygon is eligible only if its bounding-box diagonal clears the
level's gate:

```
eligible  ⇔  bbox_diagonal >= visibility_factor * gsd(level)
```

Features below the gate reappear at finer levels as GSD shrinks. Points have
no gate.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--line-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |
| `--polygon-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |

A bigger factor raises the bar and drops more small features at coarse
levels. A smaller factor keeps more of them.

The polygon default matches the write stage's effective survival threshold
of about 2 × GSD: polygons also drop when their simplified geometry falls
below `--simplify-factor × GSD`. Stricter gates remove otherwise drawable
features. On Germany buildings,
2.0 instead of 4.0 gives 2.7–4.6× more features at z8–z12 and starts the
pyramid one zoom coarser, for 13% more file size and 7% more conversion time
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md),
Decision 6). Values *below* 2.0 change little on their own, because
simplification collapses nearly every extra feature at write time. The
exception is `--collapse`, which keeps them as representative points, as in
the [dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers).

The gate scales with zoom and `--gsd-base`. Going one zoom coarser doubles
the threshold. A feature can fail it even in an empty thinning cell.

### Empty coarse levels: the auto-clamp

When every feature fails a coarse level's gates, tylertoo omits the level
and renumbers the rest (spec §7.3). For buildings at `--min-zoom 0`, the
threshold `2 × gsd(z0)` is about 78 km. A typical warning is:

```
omitting 6 empty level(s) [0, 1, 2, 3, 4, 5] spanning GSD 39135.76–1222.99 m:
none of the 59032924 input feature(s) are visible at those scales (visibility
gates / density budget); the output pyramid starts at GSD 611.50 m (zoom 6).
To populate coarse levels, lower --polygon-visibility/--line-visibility, or
pass --collapse to keep sub-GSD polygons as representative points (see
docs/OVERVIEW_TUNING.md)
```

A `note:` line also appears under the CLI level table. The report's
`skipped_empty_levels` lists each omitted level's planned index, GSD, and
zoom, and the written `levels` array is the effective range. The same clamp
applies when a level empties during simplification: a feature's bbox can
clear the gate while its geometry collapses below the level tolerance.
PMTiles export of a clamped file starts at the coarsest written level's zoom.

The omitted levels contain no drawable features. To populate them, lower
the gates, raise `--gsd-base`, or use the
[dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)
(`--polygon-visibility 0 --collapse`). If **no** level has any rows, as with
an empty input or an empty `--bbox` selection, the conversion fails.

---

## Per-feature simplification: `--simplify-factor`

Simplification runs Ramer–Douglas–Peucker (RDP) on each *surviving* feature's
geometry with a world-space tolerance:

```
tolerance = simplify_factor * gsd(level)          (meters, then CRS-converted)
```

| Option | Default | Effect |
|--------|---------|--------|
| `--simplify-factor` | `1.0` | RDP tolerance in GSD multiples; larger = less detail |
| `--collapse` | off | Keep below-tolerance polygons as points |
| `--collapse-square` | off | Keep placeholder squares of about 1 × GSD ([details](#zoom-band-representation-and-placeholder-squares)) |
| `--representation LO-HI:KIND,…` | all `geom` | Choose `geom`, `point`, or `square` by zoom band |
| `--no-cascade` | off | Disable cascading simplification |

Lower `--simplify-factor` to keep more vertices; raise it for less detail
and smaller levels.

Simplification applies in duplicating mode only. The **canonical level always
stays verbatim**, whatever the factor (spec §2.4). `--simplify-factor 0`
disables simplification.

High factors also remove features whose bbox diagonal falls below the
tolerance. Keep the factor modest to shed vertices, and control feature
counts with thinning and visibility.

### Cascading simplification (default on): `--no-cascade`

In duplicating mode, each coarser level simplifies the next-finer level's
output, as tippecanoe does. This avoids repeatedly simplifying canonical
geometry, which dominates conversion time without cascading. A single
boolean-overlay pass repairs self-intersecting RDP candidates into valid
even-odd geometry.

Coarse-level coordinates differ slightly from non-cascaded output. Cascaded
vertices are still a subset of canonical vertices, and every step's output is
validity-checked or repaired. The geometric GSD ladder bounds the cumulative
deviation at about 2× the target level's tolerance instead of 1×. The footer
provenance records `generalization.cascade: true`. Pass `--no-cascade` to
reproduce non-cascaded output byte-for-byte.

#### Validity-check vertex cap

A simplification candidate with more than 2,048
total vertices skips the exact validity check and counts as valid. The check
is O(V²) in ring size, and on continental-scale rings at fine GSDs it stalled
conversion for tens of minutes per feature. A candidate that large means RDP
removed few vertices from already-valid input, the case least likely to
self-intersect. The overviews spec does not require valid geometry (§2).
Candidates at or below the cap get the full check and repair. tylertoo logs a
count of skipped checks at `info` when conversion ends.

---

## Country-scale dot fill for dense polygon layers

A dense layer of small buildings, parcels, or fields can render an empty
country view with the default settings. Sub-GSD polygons drop in two stages:

1. At assignment, a polygon below
   `--polygon-visibility × GSD` is ineligible.
2. At write time, an eligible polygon drops if its simplified geometry
   falls below the tolerance. At z4 that is about 2.4 km, larger than any
   building.

A 20 m building cannot render as a polygon at z0–z8. `--collapse` (opt-in,
spec Q4) keeps below-tolerance polygons as representative points. With a
zero visibility gate, coarse levels become a dot field bounded by thinning
and the density budget:

```bash
tylertoo overview buildings.parquet buildings_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse

# One-shot PMTiles build; 500K is the default cap (0 disables it).
tylertoo tiles buildings.parquet buildings.pmtiles \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse --max-tile-size 500K \
  --profile bounded
```

For country-scale builds:

- Keep the tile cap. Uncapped dot tiles can reach several MB (Germany z6:
  12 MB). The cap thins dots with a spatially even stride to about 500 KB.
- Use `--profile auto` to spill when needed, or `bounded` to force spilling.
  Coarse levels buffer more rows; forcing `speed` raised Germany's peak
  resident set size from 15 to 29 GB.

On Overture Germany buildings (59M footprints, z0–14), every level populates:
z0 holds 581 dots, z4 128,886, and z6 1,074,540
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md),
Decision 6). Levels z6–z13 land on the
[density budget](#density-budget-drop-rate-drop-gamma-no-density-drop) ladder
`N / 1.65^(14−z)`, with dense areas such as the Ruhr and Berlin visibly denser
and rural areas protected. The overview file grows 31% (11.9 → 15.6 GB), and
conversion takes 40% longer. On the Moldova field corpus the same recipe adds
**0.3%** to file size.

Add a small `circle` layer filtered to `["==", "$type", "Point"]`; `fill`
layers ignore points. The output's `geometry_types` lists the union, for
example `["Point","Polygon"]` (spec §7.5). Collapse is opt-in because it
changes geometry types (spec Q4), and is recorded in `generalization`
provenance.

Keep the default `--drop-rate` and `--drop-gamma`. The budget already caps
coarse levels and shapes dot density. Using `--drop-rate 1.3` increased
mid-zoom row counts by 13% without changing the coarse fill.

---

## Zoom-band representation and placeholder squares

Use `--representation` to choose geometry by zoom band, or
`--collapse-square` to retain polygons as placeholder squares.

### `--representation LO-HI:KIND,…`: the band selector

One build can use different representations across zoom bands:

```bash
# Dots at coarse zooms, polygons at fine zooms:
tylertoo tiles buildings.parquet buildings.pmtiles -f \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point,8-14:geom"

# Tippecanoe-style placeholder squares at coarse zooms instead:
tylertoo tiles buildings.parquet buildings.pmtiles -f \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:square"
```

The supported kinds are:

- `point`: every polygon becomes its representative point. This is the centroid,
  falling back to the bbox center and then the first vertex for degenerate
  rings. Polygons bypass the visibility gate and use the point grid
  (`--point-thinning`). Render them with a `circle` layer.
- `square`: normal simplification, with below-tolerance polygons replaced
  by area-dithered squares of about 1 × GSD (see `--collapse-square` below).
  Visible polygons stay unchanged, and fill styles work throughout.
  Polygons bypass the visibility gate but keep the polygon thinning grid.
- `geom`: normal geometry, also the default for unlisted zooms.
  Below-tolerance polygons follow the global disposition: drop,
  `--collapse`, or `--collapse-square`.

tylertoo validates the bands when conversion starts:

- They need duplicating mode with a zoom-range plan. A `--gsd` plan has no
  zooms to band on.
- Bands must not overlap.
- A non-`geom` band must end **before** `--max-zoom`, because the canonical
  level is always verbatim (spec §2.4).
- `point` bands must run contiguously from the coarsest planned zoom. The
  cascade carries the point through every coarser level, so tylertoo
  rejects a request for polygons coarser than a point band.

With cascading on, a feature entering a point band collapses at the band's
**finest** level, and every coarser band level reuses that point. Lines and
native points ignore every band kind. Bands work with clustering and
coalescing, and the density budget caps band levels like any other level.

The footer provenance records the bands, as
`generalization.representation: [{"zooms": [0,7], "repr": "point"}, …]`.

### `--collapse-square`: tippecanoe tiny-polygon squares as the global disposition

`--collapse-square` replaces below-tolerance polygons with `tol × tol`
squares, where `tol` is `simplify-factor × GSD`, converted to CRS units.
Like tippecanoe's tiny-polygon reduction, it keeps dense fields, buildings,
and parcels visible at coarse zooms with existing fill styles.
`geometry_types` stays `["Polygon"]`, so no spec-Q4 geometry-type opt-in
applies.

`--collapse-square` is opt-in, and the default disposition is still to drop. The
footer provenance records it as `generalization.collapse: "square"`, and
`--collapse` records `"point"`.

Two mechanisms share the threshold `T = side²`, where `side` is `tol` floored
at one tile unit of the level's zoom (see the divergences below).

#### Area accumulation and dithering

The accumulator covers polygons removed by the visibility gate, thinning,
or density budget. After assignment, each adds its area, clamped to `T`, to
its patch's running total in input order. A
patch is a 32 × GSD square, 1/32 of a 1024-px tile. Each time a patch's total
crosses `T`, the polygon that crossed it becomes the level's *carrier*.
tylertoo emits it as a `T`-area square at its representative point, with its
own attributes.

A polygon contributes at most one placeholder's area. Gate failures may be
larger than `T` because the gate is `--polygon-visibility` pixels wide;
thinning and budget losers can be any size. Each patch retains less than
`T` in unemitted area. This keeps 25 m fields visible as farmland at z0.
On a 368k-field sample, z1–z6
carry about 98% of the input area, against 1.5–7% with the dither alone. An
entry-zoom ladder (`--entry-zoom`) decides where its features first appear,
so the accumulator never counts them.

The dither covers polygons the level carries but that RDP shrinks
below `T` at write time. A polygon of area `A` survives as a square with
probability `A / T`, decided by a hash of its anchor coordinates. The two sets
are disjoint, so nothing counts twice.

Both mechanisms are deterministic. The accumulator traverses the pass-1
table in input order; the dither is a pure function of each feature. Output
is byte-identical across runs, engines (in-memory, streaming, pipelined),
and thread counts. With cascading, a square's anchor is its center. Coarser
levels reuse the hash draw with a smaller keep probability, so a square
removed at one level cannot reappear at a coarser one.

#### Differences from tippecanoe

See also `context/ARCHITECTURE.md`.

- **Per patch, not per tile.** Tippecanoe accumulates per tile. tylertoo
  accumulates per 32 × GSD patch, because overview levels have no tile scope.
  Squares land at the carriers' own positions inside the patch. A prepared
  file is in Hilbert order, so they cluster where the fields are.
- **Clamped, not skipped.** Tippecanoe accumulates only rings with area
  ≤ `tiny_polygon_size²` and keeps larger rings as geometry. tylertoo clamps
  each polygon's contribution to one placeholder instead: the gate or
  thinning already dropped a non-member here, so there is no geometry to keep.
- **Placement.** Tippecanoe places the placeholder at the ring's first vertex
  with side `tiny_polygon_size` (default 2 px). tylertoo places it at the
  polygon's representative point with side `simplify-factor × GSD`.
- **A floor of one tile unit.** Tippecanoe's placeholder side is a fixed count
  of tile units. tylertoo's is `simplify-factor × GSD`, **floored at one tile
  unit** at the level's zoom. One unit is `40,075,016.69 m / 2^zoom / 4096`,
  which for a `--min-zoom`/`--max-zoom` plan is `GSD × gsd-base / 4096`. At the
  default `--gsd-base 1024` that is a quarter of the GSD, so the floor applies
  when `--simplify-factor` is below 0.25. At `--gsd-base 8192`, even the
  default 1.0 floors to 2 × GSD.
  The tile encoder keeps a square of side `s < 1` unit only with probability
  `s²`. The floor lets the encoder draw every placeholder, reducing their
  count. The log names affected zooms. On those levels a patch's
  leftover area, under one `T`, becomes one more carrier with probability
  `leftover / T` instead of vanishing.
  The unit assumes the default extent of 4096, the only one the CLI exports
  at. A larger Python `export(extent=…)` draws floored squares over more than
  one unit, and a smaller one draws them under a unit, where they thin as
  described above.
- **No placeholder at `--simplify-factor 0`.** tylertoo skips the
  accumulator and logs it, as tippecanoe skips its reduction at
  `tiny_polygon_size` 0.
- **Duplicating mode only.** A carrier is a second appearance of a feature,
  which partitioning's feature-once contract cannot represent. In
  partitioning mode the levels are verbatim, so tylertoo accepts
  `--collapse-square` and logs it as inert.

Dense squares at mid zooms can exceed `--max-tile-size`, which removes
squares per tile. Raise the cap to retain more.
[`tylertoo stats`](#export-and-archive-commands-export-pmtiles-merge-stats)
reports sizes by zoom.

---

## Attribute-driven entry zoom: `--magnitude-ladder`, `--entry-zoom`

[Ranking](#ranking-which-feature-wins-a-cell) runs after the visibility gate,
so `--sort-key` cannot rescue small features the gate removed. For a
population-density choropleth, this can hide tiny urban tracts with high
values while keeping large rural tracts with low values.

An entry-zoom ladder assigns each distinct column value a zoom. Its
features appear from that zoom inward, exempt from visibility gates and
thinning, and never appear at coarser zooms.

```bash
# Derive it: distinct values ranked descending, one zoom apart from --min-zoom
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 6 \
  --magnitude-ladder density

# ...or place the rungs by hand. Rungs must fall inside the zoom range, and
# each names a value the column actually carries.
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 8 -f \
  --entry-zoom "density:5000=0,1000=3,200=6"
```

With five distinct values, `--min-zoom 0 --max-zoom 6`, and the default step,
the highest rank appears at z0; each lower rank joins one zoom later. The
ladder uses distinct-value ranks (SQL `DENSE_RANK`) to avoid concentrating
entry zooms in a narrow range when raw values cluster together.

A column may have more distinct values than the zoom range has room for.
Ranks that would fall past the finest zoom get no rung. Their features take
the ordinary gate and thinning, and tylertoo does not pin them to the finest
level. The run reports how many it left out.

`--ladder-step N` (default 1) sets the spacing. Nulls and values omitted
from an explicit spec get no rung; their features use ordinary gates and
thinning.

### Two mechanisms can still undo a ladder

Two later stages can remove admitted features:

- Simplification can collapse small geometry below tolerance.
  `--magnitude-ladder` and `--entry-zoom` therefore imply `--collapse` to
  retain representative points. `--collapse-square` overrides this.
- The density budget's size fallback can discard small, high-value survivors. Pair the
  ladder with `--no-density-drop`, or with `--sort-key` on the same column so
  the budget ranks the way the ladder does. tylertoo warns when a ladder runs
  with the budget on and no sort key.

With the budget off, each rank appears at its entry zoom and stays at finer
zooms:

```
zoom     rank 4  rank 3  rank 2  rank 1  rank 0   (0 = highest value)
z0            0       0       0       0     140
z1            0       0       0     140     140
z2            0       0     165     165     165
z3            0     192     176     176     176
z4          250     192     176     160     160
```

### Relation to tippecanoe

The ladder corresponds to tippecanoe's per-feature `tippecanoe.minzoom`
attribute. `--magnitude-ladder` also derives entry zooms from a column.

---

## Ranking: which feature wins a cell

The highest-priority feature wins each grid cell. Priority uses these tiers,
highest first (spec §3.5, Q1):

1. `--sort-key COL`: a numeric column, such as population or importance.
2. `--class-rank COL:VAL=RANK,…`: an explicit categorical map, such as
   `road_class:motorway=5,primary=4,residential=2`. Unlisted values rank
   below every listed one but above nulls.
3. **Automatic detection**, unless you pass `--no-auto-rank`. Overture roads
   (`class` or `road_class`) get a built-in motorway-to-service ranking, and
   Overture places rank by `confidence`.
4. **Size fallback**: the larger bbox diagonal wins, and a deterministic hash
   breaks ties, as in tippecanoe.

Ranking selects survivors; thinning sets their count. For roads, ranking
keeps highways visible at coarse zooms. The
`generalization.ranking` provenance in the footer records the tier used.

`--sort-key` and `--class-rank` are mutually exclusive.

Nulls, NaNs, and infinities rank below every real key. Their rows remain
eligible but lose cells contested by keyed features. The entry-zoom ladder
uses the same rule: non-finite values get no rung. `--accumulate-attribute`
skips NaNs but includes infinities (see
[Clustering](#clustering-cluster-accumulate-attribute)).

---

## Density budget: `--drop-rate`, `--drop-gamma`, `--no-density-drop`

Once thinning cells are smaller than typical feature spacing, almost every
feature wins a cell. Counts plateau near the input count, often from z9 up.
On Portland roads, this is 2–3× tippecanoe's count at z9–z11 and accounts
for much of duplicating mode's storage overhead.

The density budget caps survivors after thinning, decreasing geometrically
toward coarse levels. It drops the lowest-priority features in
[ranking](#ranking-which-feature-wins-a-cell) order:

```
budget(level) = N / drop_rate ^ (finest_level − level)      (N = input features)
keep(level)   = min(cell_winner_survivors(level), budget(level))
```

The canonical level keeps everything (spec §2.4). Levels below their budget
stay unchanged, typically including coarse zooms where thinning already
limits counts.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--drop-rate F` | `1.65` | ratio (>1) | **bigger = sparser mid zooms** |
| `--drop-gamma F` | `1.5` | exponent (≥1) | **bigger = more sparse-area protection** |
| `--no-density-drop` | off | flag | disables the budget |

### Drop rate

Each coarser level has `1/rate` of the next finer level's budget. Raise
`--drop-rate` for sparser mid zooms and smaller files. The default `1.65`
comes from Portland roads. It brings z9 to 1.21× and z10 to 1.03× tippecanoe's
counts and puts z11 at 0.67×. It leaves z8 and the coarse zooms near their
cell-winner counts
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)).

tylertoo anchors its budget on the full canonical count `N`; tippecanoe's
`-rate` uses a per-tile basezoom count. Equivalent thinning therefore needs
a smaller rate here. At `2.5`, Portland's z9–z13 counts fall below
tippecanoe's.

### Spatial fairness

`--drop-gamma` protects sparse areas by distributing the budget across
`128 × GSD` super-cells. Each keeps its highest-priority features up to an
allocation `∝ population^(1/gamma)`. Water-filling reallocates unused
capacity so no cell gets more than it has:

- `gamma = 1` is a proportional cut: every neighborhood keeps the same
  fraction.
- `gamma > 1` is **sublinear**: dense neighborhoods keep proportionally fewer
  features, and sparse ones proportionally more.

This is tippecanoe's `-g` gamma dot-dropping ("reduce dots to the `1/gamma`
power in dense areas"), applied per super-cell. `--drop-gamma` does **not**
change per-level totals. It only redistributes *which* features survive
spatially, so it is independent of `--drop-rate`.

Points often stay below the budget because `--point-thinning` (default 4)
already removes many, as with New York City points of interest. Changing
`--drop-rate` then has little effect; use
[clustering](#clustering-cluster-accumulate-attribute). Lines and polygons
use thinning factor 1, so the budget usually affects them more.

**`--no-density-drop`** turns the budget off, leaving pure cell-winner
thinning and a footer without the budget block. It affects only the mid-zoom
plateau. The `geo:overviews` → `generalization.density_drop` provenance in
the footer records the mechanism and its parameters (`drop_rate`, `gamma`,
`supercell_gsd_factor`).

---

## Clustering: `--cluster`, `--accumulate-attribute`

`--cluster` (opt-in, duplicating mode only) counts points that lose their
thinning cell under the winner:

- Every output row gains `point_count` (`INT64 NOT NULL`), the number of
  source features it represents, following tippecanoe and supercluster.
  Canonical rows, lines, and polygons always carry 1.
- Winners keep their own geometry and attributes. Unlike supercluster's
  centroids, these deterministic anchors remain at real features.
- Clusters are computed per level. A point absorbed at z4 may win at z6
  with its own smaller cluster. At each level,
  `sum(point_count) == total source point count`: the clusters partition the
  dataset at every level's grid.

For dense points such as places or addresses, scale symbols by
`point_count` to show how many features each represents.

Clustering changes the `--point-thinning` default from 4.0 to 16.0.
Counts retain the discarded points' contribution, allowing a sparser grid.
16 × GSD gives about one dot per 16 display pixels; supercluster's default
radius is about 40 px. In the New York City sweep over factors 4, 16, and
48, each 4× step shifted the density ladder by two zooms. An explicit
`--point-thinning` overrides either default.

`--accumulate-attribute COL:OP` (repeatable, requires `--cluster`) replaces
the winner's numeric value with the cluster's `sum`, `max`, `min`, or `mean`:

```bash
tylertoo overview places.parquet places_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --cluster \
  --accumulate-attribute population:sum \
  --accumulate-attribute confidence:mean
```

### Aggregation and counts

- tylertoo computes aggregates **per level from source values**, never from
  coarser aggregates, so `mean` is exact at every level.
- Nulls do not contribute. A cluster whose members are all null keeps the
  winner's null. Non-accumulated columns keep the winner's own values.
- NaNs do not contribute or count toward the mean.
- **Infinities count.** `±inf` is an ordinary summand, so `value:max` over
  `{1.0, +inf}` is `+inf` at every level. That agrees with the canonical
  level, which carries the source value verbatim.
- Aggregation runs in `f64` and writes back in the column's original type. A
  `mean` over an integer column rounds to the nearest integer, so prefer float
  columns for `mean`.
- The column must exist and be numeric, or the conversion fails early.
- **The density budget** can defer a cell winner and leave its cell without a
  representative. Those features attach to the **nearest surviving point** at
  that level, so the counts still sum correctly.
- Partitioning mode is rejected: a row serves several zooms through prefix
  reads, but one stored count cannot describe every zoom's grid. Finer rows
  would also double-count points already absorbed by coarse winners.

The `geo:overviews` → `generalization.clustering` provenance in the footer
records clustering (`enabled`, `point_count_column`, `accumulated: [{column, op}]`).
`tylertoo validate` checks that the column exists as `INT64 NOT NULL` and that
canonical-level values are all 1.

---

## Line coalescing: `--no-coalesce-lines`, `--coalesce-junction-angle`, `--coalesce-snap`, `--coalesce-max-level-rows`

Short road or river segments can disappear below the visibility gate
(`--line-visibility × gsd`), leaving disconnected fragments after thinning.
Line coalescing, on by default, joins touching compatible segments into
stroke LineStrings before gates and thinning at each non-canonical
duplicating level. Pass `--no-coalesce-lines` to disable it.

- The gate tests a chain's extent, allowing short segments to survive
  together as one visible line.
- Chains never merge **across class values** when a class ranking is active,
  whether an explicit `--class-rank` or an automatically detected Overture
  `class` or `road_class`. With no class ranking, all lines are compatible.
- **Junctions**, where three or more compatible endpoints meet, end chains by
  default and preserve network topology. `--coalesce-junction-angle` can
  continue the most closely aligned pair through them.
- The merged feature keeps the **attributes of its highest-priority member**,
  in the same class-rank, size, and hash order as the cell-winner stage. The
  output gains a **`coalesced_count`** `INT32 NOT NULL` column: the number of
  source segments merged into the row. It is 1 for unmerged rows and for every
  row at the canonical level, which never coalesces. Tiles omit the column
  when it is 1 everywhere, since it then says nothing about the data. The
  overview file keeps it.
- Points and polygons stay untouched. MultiLineString rows pass through
  unmerged.

```bash
# Coalescing is on by default (auto class ranking groups by road class):
tylertoo overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14

# Opt out (no coalesced_count column):
tylertoo overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14 --no-coalesce-lines
```

### `--coalesce-junction-angle` (default 0 = off, degrees)

Chains stop at junctions by default, which rendered better in the Portland
sweep. An angle allows the straightest pair to continue if its deviation
is within the limit, then repeats for the next pair. Both through-streets
can continue across a four-way crossing. Larger angles give fewer, longer
strokes but can merge real turns and smear attributes across crossings.
At 30°, Portland's z0–z1 gains large arterial strokes.

### `--coalesce-snap` (default 1.0, GSD multiples)

Exact endpoint matches always chain, including shared Overture and
OpenStreetMap nodes. Snapping also joins chain ends within `factor × gsd`.
Raise the factor to bridge larger gaps, at the risk of joining nearby
parallel lines. `0` leaves exact matches only.

### `--coalesce-max-level-rows` (default 2,000,000): memory guard

Chaining holds all candidate line geometries in memory, including fragments
otherwise dropped. Above either limit below, tylertoo skips coalescing and
warns which limit was exceeded. The overview keeps `coalesced_count` (all 1)
and provenance; tiles omit the all-1 column. Large levels tend to be near
canonical, where individual segments are visible and coalescing matters
less. This allocation is `O(lines)` even in streaming mode.

The guard checks both rows and modeled bytes. For 2M lines, the model
estimates 168 MiB for two-point roads but about 15 GiB for 500-vertex
contours:

| Limit | Default | What it counts |
|------|---------|----------------|
| candidate lines | `--coalesce-max-level-rows` (2,000,000) | line features in the input |
| retained geometry | `--coalesce-max-level-rows × 512 B` (1.024 GB, about 977 MiB) | a fixed model: a 56 B slot plus 16 B per vertex, per line |

The byte model depends only on vertex counts, independent of the machine,
allocator, or `geo` version. Allocator headers, `Vec` slack, and side vectors
add about 100–120 B per line: the default 1 GB modeled limit means about
1.2 GiB resident. Short lines have more overhead relative to geometry;
2M two-point segments measured about 400 MiB resident against 168 MiB modeled.

The byte limit binds first when lines average more than about 28 vertices
(`512 = 56 + 16 × 28.5`). Depending on line count, about 57–64M vertices
cross it: 2M lines at 29 vertices, 1M at 61, or 250k at 253.

- *Not affected:* road networks split at intersections. Overture
  transportation segments average about 8 vertices (about 190 B modeled), so
  the row limb binds for them.
- *Affected:* **unsplit** line data, once there are enough lines to cross
  about 1 GB modeled. Examples include OpenStreetMap ways kept whole, rivers,
  streams, administrative boundaries, coastlines, contour lines, and GPS
  tracks.

Raise `--coalesce-max-level-rows` to scale both limits: 4,000,000 permits
4M lines or 2.048 GB modeled. Allow about 1.2× modeled bytes for resident
geometry, plus the larger chain-stage peak below, or disable coalescing.

Both limbs are pure functions of the input, so the verdict and the output are
identical across machines, engines (`--no-streaming` or not), and
`--read-batch-size` values. Pass 1 enforces the limbs *while it collects*:
the moment the running totals cross, it frees what it has buffered and falls
back to counting. On a 6M-line synthetic input, that cut the macOS peak
memory footprint from 4890 to 1157 MiB with byte-identical output.

#### Chain-stage memory

The guard does not bound chaining itself. Pass 1 chains the whole line
scratch for every planned level to obtain the row counts needed by the plan.
Pass 2 reuses those tables.

The levels run in parallel, in waves sized against the `--profile` memory
budget: 60% of available RAM under `auto` and `bounded`, unbounded under
`speed`. At most `budget / (2 × modeled line bytes)` levels run at once, and
never fewer than one. A tighter budget costs wall time, never output. Peak RSS
on Linux (`/usr/bin/time -v`) for 1.5M 7-vertex lines in chains of ten
(240 MiB modeled):

| run | unbounded budget | capped budget |
|---|---|---|
| `tiles`, z0–z14 | 4667 MiB, 19 s | 3525 MiB, 19 s (8 GB RAM, 9 levels per wave) |
| `overview` defaults (6 of 7 levels empty) | 2386 MiB, 2.4 s | 1373 MiB, 5.6 s (2.5 GB RAM, 2 levels per wave) |
| `tiles --plan-only` | 4973 MiB, 4.6 s | 1845 MiB, 9.6 s (2.5 GB RAM, 2 levels per wave) |

With `--no-coalesce-lines`, the same `tiles` z0–z14 run peaks at 935 MiB.
Pass 1 must chain levels before it can know they are empty. For the lowest
peak, set `TYLERTOO_AUTO_MEM_LIMIT_BYTES` low enough for one level per wave,
or disable coalescing.

### Interactions

- **`--line-visibility` and `--line-thinning`** act on *chains*. The gate
  tests the merged extent, and one **chain**, not one segment, survives per
  thinning cell. Expect coarse levels to show **fewer rows but much more line
  length** than a run without coalescing.
- **Class ranking** does double duty: it picks which chain wins a cell and
  defines the compatibility groups. `--no-auto-rank`, or a numeric
  `--sort-key`, makes all lines compatible. That suits single-class datasets
  such as rivers and is usually wrong for mixed road networks.
- **The density budget** applies to **chains**. After the gate and thinning,
  each level keeps at most `num_lines / drop_rate^(finest − level)` chains,
  with the same ladder, floor, and spatial-fairness gamma as the point and
  polygon budget, cutting the lowest-priority chains first. Without it, the
  reclaimed fragments would re-inflate the mid-zoom counts the budget caps.
  `--no-density-drop` disables the chain budget too. Points and polygons keep
  the row-level budget.
- **Partitioning mode: inert.** Partitioning places each feature exactly once
  with verbatim geometry. A merged chain is a new geometry replacing several
  source rows, which that contract cannot represent. Partitioning conversions
  run without coalescing (no `coalesced_count` column, no provenance, an
  `info` log line). An explicit `--coalesce-lines` with
  `--mode partitioning` is an error.

The `geo:overviews` → `generalization.coalescing` provenance in the footer
records the complete knob set (`enabled`, `snap_tolerance_gsd_factor`,
`junction_angle`, `max_level_rows`, and `coalesced_count_column`, per spec §13.4).
`tylertoo validate` checks that the column exists as `INT32 NOT NULL`, that
every value is at least 1, and that canonical-level values are all 1.

---

## Reserved column names

tylertoo reserves names for its appended overview columns, ignoring case.
A colliding source name gains `_` suffixes until free:
`level` becomes `level_`, `LEVEL` becomes `LEVEL_`, and `level_` becomes
`level__`. The reserved column is authoritative.

| Reserved name | Reserved when | Reaches MVT? |
|---|---|---|
| `level` | always | **no**: dropped from tile properties |
| `point_count` | `--cluster` | yes |
| `coalesced_count` | line coalescing (on by default) | yes, unless 1 on every row (then omitted from tiles, but kept in the overview file) |

tylertoo logs a rename at `warn`:

```
input column "level" collides with the reserved overview column "level";
renaming the input column to "level_" in the output
```

Export restores source names when free. tylertoo's `level` never reaches
vector tile (MVT) properties, so a source `level` is restored in tiles:

```
overview.parquet:   level_ (your data)  +  level (tylertoo's)
out.pmtiles:        level  (your data)
```

The same holds for a standalone `export-pmtiles` run on an earlier overview
file. The footer records the rename (`geo:overviews.generalization.renamed_columns`,
output name → source name), so the export does not need the converting run's
state.

Source `point_count` and `coalesced_count` columns keep their renamed names
when the reserved columns reach MVT. If tylertoo omits an all-1
`coalesced_count`, the source column regains its original name.

By-name options follow the rename automatically (`--sort-key level`,
`class_ranking.column`, `--accumulate-attribute`, `--filter`), so you never
spell the renamed name yourself.

---

## Output feature order: `--feature-order`

MVT does not define draw order, but renderers paint features in tile order
unless a style overrides it. Changing that order can change the map.

The default is input row order: source order restricted to the rows each
overview level kept. A nested-polygon fixture at z12–z14 has zero
within-tile inversions against source order.

Tippecanoe's order is unspecified and varies by tile and zoom, sometimes
reversing within one input. Set `--feature-order` when paint order matters:

```bash
# Source row order (the default)
tylertoo tiles in.parquet out.pmtiles --feature-order input

# Sort within each tile by a property: high `level` painted last (on top)
tylertoo tiles in.parquet out.pmtiles --feature-order level -f

# ...or first (underneath)
tylertoo tiles in.parquet out.pmtiles --feature-order level:desc -f
```

Both `tiles` and `export-pmtiles` accept it.

For a nested-polygon choropleth, sort small high-value shapes above larger
low-value shapes. `--feature-order level` sets the archive order equivalent
to a style's `"fill-sort-key": ["get", "level"]`.

### Sorting rules

- **Ties keep input order.** The within-tile sort is stable over the
  `(tile, row)` order, so two runs of the same build produce byte-identical
  tiles.
- **Ordering is per tile.** Nothing moves across tile boundaries. The option
  changes the sequence within a tile and nothing else.
- **Numeric types compare as numbers.** A column read as an integer in one
  row group and a double in another is one ordering class, not two.
- **A feature missing the property sorts first**, painted underneath, rather
  than dropping. A `NaN` sorts the same way: no style can rank it, so it joins
  the unrankable features underneath instead of taking an arbitrary place
  among the numbers.
- **Sorting is independent of the oversized-tile valve.** With
  `--max-tile-size` in force, `select_kept_members` decides which features
  survive: largest first, or a uniform spatial stride on point-dominated
  tiles. `--feature-order` then orders the survivors.
- **The name is the key the tile advertises**, not the schema column name.
  Export restores a source column renamed to clear a reserved name (see
  [Reserved column names](#reserved-column-names)), so sort by the *source*
  name. `--feature-order level` is right for an input with its own `level`
  column, even though the overview file stores it as `level_`. A column stays
  renamed when tiles publish the reserved name, as they publish `point_count`
  under `--cluster`. Name such a column as it appears in the tile.
- An unpublished column produces a warning listing available property names.
  All features remain unranked and keep input order.

`--feature-order input` is the default and costs nothing. Naming a column adds
one stable sort per tile over features already in memory.

---

## Stable feature ids: `--feature-id`

Every MVT feature carries an `id`, defaulting to its position in one tile
at one zoom. This tile-local id can change across tiles and zooms.

`--feature-id COLUMN` writes the column's value as the feature id instead, on
every tile and zoom the feature appears in. It matches tippecanoe's
`--use-attribute-for-id`:

```bash
tylertoo tiles buildings.parquet buildings.pmtiles --feature-id building_id
tylertoo export-pmtiles overview.parquet out.pmtiles --feature-id building_id
```

Stable ids let MapLibre feature state and external joins follow a feature
across tiles and zooms without `promoteId`. For example:

```javascript
map.setFeatureState({ source, sourceLayer, id }, { hover: true });
```

`promoteId` still works on any property, with or without `--feature-id`.

### Stable id requirements

- **Integer columns only**: `Int8` to `Int64`, `UInt8` to `UInt64`, or an
  unscaled `DECIMAL(p,0)`. MVT ids are unsigned 64-bit integers. Tippecanoe
  also parses numeric strings and integral floats, and tylertoo rejects them.
  A string id, such as an Overture GERS id, has no lossless integer form. Cast
  or hash it to an integer column before tiling, with `gpio` or DuckDB (for
  example `hash(id)::UBIGINT`), and keep the original string as a property if
  clients need it.
- Every row must hold a value in `0..=2^64-1`. Nulls, negatives, and
  out-of-range decimals fail export. Unlike tippecanoe, tylertoo does not
  wrap `-5` to `18446744073709551611`. It checks every overview level before
  writing tiles. Errors name the overview row and level, since conversion
  reorders source rows.
- **The column moves rather than copies.** It becomes the id and leaves the
  tile properties and `vector_layers`, whatever `--include-property` and
  `--exclude-property` say at export. To keep it as a property too, carry it
  under a second name in the source. On `tiles`, the convert step must keep
  the column, so an `--exclude-property` naming it is an error there.
- **It cannot be the `--feature-order` column**, because that sort reads the
  tile properties the id left. Nor can it be an `--accumulate-attribute`
  column, whose clustered value is a sum or mean of several ids.
- **Aggregated features keep one member's id.** A cluster (`--cluster`)
  carries its representative point's id. A coalesced line chain carries its
  highest-priority member's id. A tiny-polygon placeholder carries its own
  polygon's id. tylertoo does not check ids for uniqueness. Duplicate values
  give features that share an id, and feature state then applies to all
  of them at once.

`pyramid` does not take `--feature-id`, and neither does the Python
`convert()` one-shot. Use `overview()` and then
`export_pmtiles(..., feature_id=...)` instead.

---

## Tile geometry knobs: `--tile-buffer`, `extent`

Both settings apply at export.

### Tile buffer

`--tile-buffer` (default 8, maximum 256) extends geometry past tile edges
to render seams continuously. Units are pixels of a nominal 256-pixel tile,
matching tippecanoe's `--buffer` (default 5).

At 256, a tile already includes features from all eight neighbors. Larger
buffers would reach nonadjacent tiles and risk O(features × tiles) export,
so tylertoo rejects them before conversion or export. Sharded builds read
two pivot tiles (512 px) beyond their range, covering this margin. See
[Scaling](guides/scaling.md#sharded-builds).

### Tile extent

`extent` (default 4096) sets tile-local coordinate resolution through
Python's `export_pmtiles(extent=...)`; there is no CLI flag. It must be
positive. At `0`, geometry quantizes to the origin, lines and polygons
degenerate, and consumers divide by zero. Both export and `tylertoo decode`
reject zero extents.

The MVT spec recommends a power of two, which decoders assume for coordinate
precision. Other positive values are accepted with a warning.

---

## Export and archive commands: `export-pmtiles`, `merge`, `stats`

### Tile size cap

`export-pmtiles --tile-size-limit`, also spelled
`--max-tile-size` as on `tiles`, caps each tile's encoded MVT size. The
default of 500K matches tippecanoe, and `0` disables the cap. A tile over the
cap sheds features for that tile only, in one non-iterative pass. It drops
the largest first for polygons and lines, and keeps a uniform spatial stride
for point tiles.

### Polygon clipping

By default, a polygon whose rings are already
simple skips the `i_overlay` boundary-bridge fallback when tylertoo clips it
to a tile. Fine-zoom polygon export runs faster and renders the same, but a
simple ring comes out rotated to a different start vertex. Pass
`--no-simple-clip-fastpath` when you need byte-stable tile output.

### Declared minimum zoom

`export-pmtiles --min-zoom` sets the minimum zoom
the archive declares in its metadata (`vector_layers[].minzoom`), even when
the overview file lacks its coarsest levels. `overview` omits a level that
generalizes to nothing, so a file built for z0–13 can start at z2. This flag
records the requested z0 anyway. The PMTiles header's minimum zoom does not
widen: it always reports the shallowest zoom that holds a tile, as
`go-pmtiles verify` requires. A renderer that reads the header gets z2, and
the empty zooms would render nothing either way. The value must not be finer
than the coarsest level present. Unset, it is the coarsest level's zoom.
`tiles` and Python's `convert()` pass their own `--min-zoom` here.

### Property selection at export

`export-pmtiles --include-property`,
`--exclude-property`, and `--exclude-all-properties` act on the tiles only.
See [Property selection](#property-selection-include-property-exclude-property-exclude-all-properties).

### Encode tallies

`--report`, and the return value of
Python's `export_pmtiles()`, carry per-zoom tile and feature counts and the
oversized-tile tally. They also carry two encode tallies, in total and per
zoom:

- `encode_dropped_features` counts tile members with nothing to encode: empty
  geometries or empty GeometryCollections. A non-zero value means clipping
  lost content. A warning names the total, and the summary line repeats it.
- `encode_quantized_features` counts tile members whose geometry collapsed at
  the tile extent: zero-area polygon rings or lines of fewer than two points,
  typically clip slivers at a buffered tile edge. Ordinary data produces
  these, and they never warn.

### Sharded export

`--tile-range LO..HI` takes two tile ids at the same
pivot zoom and emits every descendant of those tiles at every deeper zoom. The
ids under one tile are contiguous on the Hilbert curve, so the restriction is
an exact interval test at each zoom. Tiles coarser than the pivot are not
emitted. `--zoom-ceiling Z` emits only the zooms at or below `Z`, the coarse
half. It is a ceiling, not a `--max-zoom`, because it chooses which zooms to
emit, while `--min-zoom` only widens what the metadata declares. The overview
file still holds every level. [Scaling](guides/scaling.md#sharded-builds)
covers both in context.

### `tylertoo merge`

`merge` takes two or more inputs that hold disjoint tile ids
and agree on tile type and tile compression. The merged archive's bounds,
zoom range, and `vector_layers` are the unions of the inputs'. Layers sharing
an id collapse into one entry spanning their combined zooms, with the union
of their fields. A shard's ids need not form a contiguous slice of the id
space, because subtrees at different depths interleave on the Hilbert curve.
tylertoo therefore checks disjointness per tile id, not per range.
`--work-dir` holds the spool file of merged tile data until merge assembles
the archive. Set it when the output is large and `/tmp` is a small tmpfs.
`--report` writes per-zoom tile counts for checking a sharded build. For
overlapping archives, such as different data at different zooms or several
layers over the same zooms, use [`tylertoo pyramid`](#several-inputs-one-archive-tylertoo-pyramid).

### `tylertoo stats`

For each zoom, `stats` prints the tile count and the total,
mean, p50, p99, and maximum tile size. `--largest N` adds the largest tiles by
z/x/y, and `--json` prints the same numbers machine-readably:

```bash
tylertoo stats buildings.pmtiles --largest 5
tylertoo stats buildings.pmtiles --json
```

Sizes are stored (compressed) bytes per addressed tile, read from directory
entries without reading or decompressing tiles. Run-length
members and deduplicated tiles each count at the full length of their shared
body. The `total` column is therefore the bytes a client would fetch, and it
can exceed the archive's size on disk. Percentiles are nearest-rank: pN is
the smallest size such that at least ⌈N/100 × tiles⌉ of the zoom's tiles are
no larger. The largest tiles sort by size, descending, with ties broken by
ascending PMTiles tile id. Memory and time scale with the archive's directory
entries, not its tile count, because runs aggregate with their multiplicity
rather than expanding.

---

## File layout knobs: `--row-group-size`, `--full-column-stats`

These settings change Parquet layout, footer size, and bbox pruning.
Geometry and attributes remain byte-identical.

### `--row-group-size` (default 10000): per-level row-group sizing

`--row-group-size` caps rows per group separately for each level:

- A level at or below the cap uses one group, fetched whole for a coarse view.
- Larger levels use `ceil(features / row-group-size)` roughly equal groups,
  allowing bbox statistics to prune fine-level reads to a viewport.

Each level always ends on a row-group boundary, and no row group mixes two
levels (spec §4.2), whatever the knob says.

Smaller groups tighten bbox pruning but enlarge the footer; larger groups
do the reverse. The default 10000 balances both. String and geometry
statistics are suppressed by default, keeping footers small even with
hundreds of groups. Lower the cap for tiny viewports over high-latency storage.

Parquet's `i16` group ordinal limits files to 32,768 groups. Before pass 2
opens the output, tylertoo projects group counts from pass 1's winners and
checks a 32,000-group ceiling, leaving headroom for the estimate taken before
simplification. Above it, the cap rises to the smallest clean value that
fits. A warning logs both caps; the summary and `--report` field
`effective_max_row_group_size` record the applied value.

A raised cap needs more memory because the writer buffers a whole group.
To reduce group counts, plan fewer levels with `--min-zoom` and
`--max-zoom`, or use `--row-group-size-policy zoom-scaled` for larger coarse
groups.

### `--row-group-size-policy` (default `constant`): per-level cap scaling

This sets how the `--row-group-size` cap applies across levels:

- **`constant`** (default): every level uses the same cap.
- **`zoom-scaled`**: the cap doubles for each zoom step *below* the finest
  level, `cap = row-group-size << (max_zoom − level_zoom)`. Coarse bands,
  which a wide viewport reads mostly whole anyway, collapse into fewer and
  larger row groups and fewer remote requests. The finest level keeps tight
  per-row-group bbox pruning.

Use `zoom-scaled` when a deep pyramid's coarse levels fragment into many tiny
row groups and bloat the footer. Otherwise leave it `constant`. Decision 5 in
[`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)
gives the evidence.

### `--full-column-stats` (default off): statistics suppression

The writer suppresses per-group min/max statistics on WKB geometry and
string or binary properties, such as Overture's 26-character ULID `id`.
The overview read protocol uses bbox covering and `level` statistics,
which always remain enabled (spec §4.4).

Suppressed statistics can dominate the Thrift footer, which every remote
query reads in full. On Moldova polygons (631k features, ULID ids), full
statistics make an 8.84 MB footer, larger than most viewport reads.
Suppression reduces it to under 1 MB.

Pass `--full-column-stats` to keep statistics on all columns. Do this only if
remote clients push predicates on property columns, such as `WHERE id = …`
or `WHERE class = 'motorway'`, and want row-group skipping on them. You trade
a bigger footer for that pushdown.

---

## Memory / streaming knobs: `--no-streaming`, `--read-batch-size`

These settings control conversion memory without changing output content.
[How streaming bounds memory](guides/scaling.md#how-streaming-bounds-memory)
explains the two-pass pipeline and its pass-1 floor of up to 42 bytes per row.
See also the [file layout knobs](#file-layout-knobs-row-group-size-full-column-stats).

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--read-batch-size N` | `8192` | rows per read batch (max 1048576) | **bigger = slightly faster, more memory** |
| `--no-streaming` | off | flag | use the one-pass in-memory pipeline |

### Read batch size

`--read-batch-size` bounds transient memory in both passes. The default
keeps each batch's working set in the tens of MB even for vertex-heavy
polygons. Try 1024 for limited RAM or huge multipolygons; try 65536 only
when profiling shows batch overhead and RAM is available.

Pass 1 divides each batch across rayon threads in chunks of
`read_batch_size / threads`, clamped to 256–1024 rows. Smaller batches
reduce parallelism but never disable it.

### In-memory conversion

`--no-streaming` holds the whole table and all decoded geometry,
using `O(dataset)` memory. It decodes geometry once rather than re-decoding
winners in pass 2, so small inputs may run slightly faster. Large inputs
run slower and need far more RAM. This is the equivalence-tested reference
implementation and a fallback, with no output-quality advantage.

During pass 2, the winner table holds 1 byte per feature, about 0.6 MB for a
632k-feature file.

The pass-1 feature table is held from pass 1's scan through level
assignment. It costs 33 bytes per input row, 41 with a sort key (explicit or
detected from the schema), and 42 with a sort key and an entry-zoom
ladder. The transient scan vectors raise the scan-time peak to about 42–80
bytes per row. The table is the coarse job's memory floor on a
[sharded build](guides/scaling.md#sizing-the-coarse-jobs-memory) and reaches
tens of GiB at a billion rows. Before pass 1, tylertoo checks it against the
footer row counts (#543). It warns when rows × 42 B × 3.3 exceeds the memory
figure. It fails only when the floor alone exceeds a hard cgroup limit and no
`--bbox` or `--filter` is active. `TYLERTOO_SKIP_MEMORY_PREFLIGHT=1` turns the
error into a warning.

---

## Regional extract: `--bbox`

`--bbox xmin,ymin,xmax,ymax` converts only features whose bounding box
intersects the region, in longitude and latitude degrees (EPSG:4326).
[Remote reads](guides/remote-reads.md#what-gets-skipped) explains how covering
statistics let it skip row groups, and what makes a source prunable.

```bash
# Tile Antananarivo from a Madagascar-wide file
tylertoo overview madagascar.parquet antananarivo.parquet \
  --bbox 47.4,-19.0,47.6,-18.8 \
  --min-zoom 0 --max-zoom 14
```

Use degrees even for EPSG:3857 input; the converter reprojects the box.
Without covering statistics, every row group is read before the same
per-feature test. Compare `ConvertReport.row_groups_read` with
`row_groups_total` to check pruning.

---

## Attribute filter: `--filter` / `--where`

`--filter <EXPR>` (alias `--where`) selects features using a SQL
`WHERE`-style predicate on input properties. It combines with `--bbox` on
both `overview` and `tiles`.

```bash
# Only high-confidence field boundaries, straight from the source file
tylertoo tiles fields.parquet fields.pmtiles \
  --filter "confidence > 0.8" --max-zoom 14

# Composes with --bbox and richer predicates
tylertoo overview brazil.parquet subset.parquet \
  --bbox=-48.0,-16.0,-47.0,-15.0 \
  --where "crop_type IN ('soy', 'corn') AND confidence >= 0.5"
```

### Expression syntax

A built-in recursive-descent parser supports:

- Comparisons: `=` (or `==`), `!=` (or `<>`), `<`, `<=`, `>`, `>=`, with the
  column on the left: `confidence > 0.8`, `country = 'BRA'`, `active = true`.
- Membership: `col IN (v1, v2, ...)` and `col NOT IN (...)`.
- Null tests: `col IS NULL` and `col IS NOT NULL`.
- Boolean composition: `AND`, `OR`, `NOT`, and parentheses. `OR` binds
  loosest, then `AND`, then `NOT`.
- Literals: numbers (`0.8`, `-2`, `1e6`), single-quoted strings (`'it''s'`
  escapes a quote), and `TRUE`/`FALSE`. Keywords are case-insensitive.
- Columns: bare identifiers, or `"double quoted"` for names with spaces.
  Supported column types are numeric (signed or unsigned integer, float), string, boolean, and
  timestamp.
- Timestamp columns compare against single-quoted datetime strings:
  `time >= '2025-01-01'`, `time < '2025-06-01 12:30:00'`, or full RFC 3339
  with an offset. A literal without a timezone reads as UTC, because Arrow
  timestamps store a UTC instant whatever the column's display timezone. A
  malformed datetime fails at startup, not mid-run. Statistics pushdown works
  when the file stores timestamps as INT64, as modern writers do. Legacy
  INT96 timestamps, from older Spark, expose no statistics, so those
  predicates filter exactly but prune nothing.

### Nulls and non-finite values

Filters use SQL three-valued logic:

- A comparison or `IN` over a `NULL` is `UNKNOWN`.
- `AND`, `OR`, and `NOT` combine with Kleene logic.
- A row stays only when the whole predicate is `TRUE`.

So `confidence > 0.8` drops null-confidence rows, and so does
`NOT (confidence > 0.8)`. Use `IS NULL` and `IS NOT NULL` to test nulls
explicitly.

NaN comparisons are `UNKNOWN`, including `!=` and `NOT IN`; infinities
compare normally. For example, `NaN != 5` is `TRUE` in IEEE-754 but
`UNKNOWN` here. NaN is present rather than null: `IS NULL` does not match
it, and `IS NOT NULL` does. To select these rows by value, clean the column
upstream, for example with `gpio`, or filter on another column.

### Pruning and pipeline order

Like `--bbox`, the filter checks row-group statistics before it reads data.
It then evaluates each row exactly in pass 1, so the output is identical
whether pruning fired or not. See
[Remote reads](guides/remote-reads.md#filters). `AND` intersects the prunable
sets and `OR` unions them. `NOT (...)` subtrees and columns without usable
statistics keep the row group.

The filter runs before level assignment, ranking, density budgets,
clustering, and coalescing. Rejected features never enter the pipeline.
`ConvertReport.input_features` counts only survivors. Compare
`row_groups_read` with `row_groups_total` to see whether pruning fired.
Sorting the input by a filtered column, or lowering `--row-group-size`,
tightens per-row-group statistics and prunes more.

---

## Property selection: `--include-property` / `--exclude-property` / `--exclude-all-properties`

These flags select output properties, like tippecanoe's `-y`, `-x`, and
`-X`. On `overview` and `tiles`, excluded columns are never decoded or
written to the overview. Smaller tile properties leave room for more
features under `--max-tile-size`. Geometry always stays.

```bash
# Tiles carry only confidence and metrics:area
tylertoo tiles fields.parquet fields.pmtiles --max-zoom 13 -f \
  --include-property confidence --include-property metrics:area

# Everything except the id and the timestamp
tylertoo overview fields.parquet ov.parquet --exclude-property id \
  --exclude-property "determination:datetime"

# Geometry only
tylertoo tiles fields.parquet outlines.pmtiles --exclude-all-properties
```

| Knob | Default | Semantics |
|------|---------|-----------|
| `--include-property COL` (repeatable) | all | keep only these. Naming a column the input lacks is an error |
| `--exclude-property COL` (repeatable) | none | drop these. An unknown name only warns. An include list overrides it |
| `--exclude-all-properties` | off | geometry-only output. An include list overrides it |

An include list overrides all exclusions, matching tippecanoe, where `-y`
implies `-X`. For example,
`--exclude-all-properties --include-property foo` keeps `foo`, and
`--include-property a --include-property b --exclude-property b` keeps both.
Without an include list, `--exclude-all-properties` keeps nothing and
`--exclude-property` drops what it names.

### Columns required by other options

Columns used by these options must stay included:
`--sort-key`, `--class-rank`, `--magnitude-ladder`, `--entry-zoom`,
`--accumulate-attribute`, and `--filter`. Excluding one is an error naming
the option that needs it.

`export-pmtiles` applies the same flags to published tile property names,
leaving the overview untouched. The `--feature-order` column must remain;
`tiles` checks this before conversion too. At export, an explicit
`--include-property coalesced_count` retains even an all-1 column. Include
lists still override exclusions; an included property the file cannot
export is an error.

### Column types

tylertoo exports struct, list, and map columns as JSON
strings, tippecanoe's convention for nested attributes. So
`--include-property names` on an Overture file keeps the whole `names` struct
as one string property. A column with no MVT encoding, such as a binary
column, drops with one warning per column and appears in the export report's
`skipped_property_columns`. tylertoo rejects it by name if `--include-property`
asks for it, and an `--exclude-property` naming it silences the warning. The
full type table is in [preparing input](tutorials/madagascar.md#1-prepare-the-input).

---

## Performance profiles: `--profile`, `--in-flight-batches`, `--read-workers`

These settings control speed and memory. Output is byte-identical across
profiles, batch concurrency, read-worker counts, and thread counts, as with
the [memory and streaming knobs](#memory-streaming-knobs-no-streaming-read-batch-size).

The Scaling guide covers [memory profiles](guides/scaling.md#memory-profiles),
[read concurrency](guides/scaling.md#read-concurrency), and
[spill files](guides/scaling.md#spill-files) in depth, including container
memory limits and `--spill-dir`.

Pass 2 reads Parquet once and pipelines reads, decoding, and simplification
across all cores. The profile decides where output rows wait before writing.

| Option | Default | Effect |
|--------|---------|--------|
| `--profile speed\|bounded\|auto` | `auto` | Choose RAM buffering or disk spilling |
| `--in-flight-batches N\|auto` | cores, clamped to 4–16 | More batches = more overlap and memory |
| `--read-workers N\|auto` | cores/4, at most 4 | More pass-2 readers = more throughput and resident batches |

- **`speed`** buffers each level's rows in RAM. It runs in the least wall
  time, with no temp I/O, but peak RAM grows with total *output* size.
- **`bounded`** spills each level's rows to temporary Arrow IPC files and
  streams them back at write time, capping peak RAM whatever the output size.
- **`auto`** spills when the estimated buffered output exceeds 0.6 of
  available RAM and keeps rows in RAM otherwise.

### Automatic memory estimate

`auto` estimates `buffered rows × per-row cost` from pass 1's average
encoded-geometry size. Geometry dominates buffered rows and varies about
400× across datasets. The estimate is conservative, favoring spilling.
If pass 1 scanned nothing, it uses about 8 KiB per duplicating row or
16 KiB per partitioning row. Partitioning always spills above 2M buffered
rows. The log reports the average, estimate, budget, and decision.
`TYLERTOO_AUTO_MEM_LIMIT_BYTES` overrides the detected available RAM.

That estimate counts geometry only, so a run that starts in RAM keeps
measuring for the whole pass (#626). It sums the Arrow bytes of every
buffered batch, geometry plus every kept property column, with a 256-byte
floor per row. After each input batch, it adds the bytes already buffered to
the rows still to come at the rate so far. If that total passes the budget,
`auto` moves the buffered rows to spill files once and spills for the rest of
the pass. Row order and output bytes do not change. The switch happens no
later than the moment the buffered bytes reach the budget. The log and
`TYLERTOO_PROFILE_JSON` (`pass2.sink`) report the measured bytes per row and
whether it switched. `speed` and `bounded` never switch.

### Pass-1 winner grids

Level assignment builds one winner grid per coarse level. Concurrent grids
can set peak RSS on large inputs with simple geometry: 5.9 GiB on
germany-segments and about 24 GiB at Brazil scale (see `[rss]` logs).

`bounded` and `auto` estimate grid memory against the same budget and split
levels into waves when needed, down to one level per wave. Each level still
uses all cores. Parallel reduction adds transient memory outside the estimate: about
2 MiB per thread across a wave, at least 1 MiB per level, freed after each
reduce. `[assign] winner grids …` logs any split. With enough RAM, one wave
runs all levels; `speed` always uses unbounded parallel grids.

### Batch concurrency

`--in-flight-batches` sets each pass's channel depth. Raise it to overlap
reads and computation when a few expensive geometries stall the pipeline.
Each extra batch holds `read_batch_size` more rows; passes do not overlap.
`--read-batch-size` sets the rows per batch, and `--in-flight-batches` sets
how many batches coexist. Pass 2's readers add `--read-workers` × their queue
depth on top. Under `bounded`, each level's spill writer adds up to three
more batches: two queued and one in the encoder.

### Read workers

`--read-workers` decodes disjoint runs of Parquet row groups concurrently
in pass 2. An ordered merge restores the single-reader batch sequence,
keeping output byte-identical.
`crates/cli/tests/thread_count_determinism.rs` checks `1` against `2` and
`4`. tylertoo honors an explicit value up to **2× the machine's cores**, at
least 4. Above that the CLI rejects it, and the library clamps it with a
warning, because each worker is a thread plus its own read-ahead queue. A
worker buffers its run ahead of the merge. A row group larger than its queue
makes the worker wait mid-run, and the reads serialize again. A
`[convert] pass 2 read:` debug line reports it. A single-row-group file
always reads sequentially.

For multi-GB partitioning inputs, `speed` can exhaust RAM: buffered output
may approach input size. `auto` selects `bounded` for large partitioning
and over-budget runs. Watch peak RSS if you explicitly choose `speed`.

---

## Reusing a plan: `--save-plan`, `--plan`

Pass 1 scans the input and assigns rows to levels, including density
budgets. It often takes most of the conversion time. Save its winner table
to repeat builds with different write settings without repeating assignment.

`--save-plan PATH` saves the result; `--plan PATH` replays it. Both work on
`overview` and `tiles`, require streaming, and are mutually exclusive.
Neither is exposed in Python.

```bash
# Once: scan, assign, and keep the plan.
tylertoo overview roads.parquet roads.parquet.overview \
  --min-zoom 0 --max-zoom 14 --save-plan roads.plan

# Again, tuning only the write side; pass 1 and the assignment are skipped.
tylertoo overview roads.parquet roads-tuned.overview \
  --min-zoom 0 --max-zoom 14 --plan roads.plan \
  --profile bounded --row-group-size 50000
```

The plan contains a header with magic bytes and a checksum, followed by an
Arrow IPC file. It stores one winner byte per row plus small per-level
tables: a 28 MB, 24k-feature polygon input produces a 49 KB plan. With line
coalescing, it also stores collected WKB geometries, increasing its size.

### Plan validation

`--plan` verifies the payload's xxh3-64 hash before decoding. Truncated,
edited, or damaged plans produce an error naming the path:

```
--plan /mnt/shared/roads.plan: is corrupt: the payload hashes to
1f3c...  but the header records 90ab.... The file was truncated, edited, or
damaged in transit — re-create it with --save-plan.
```

Other errors also name `--plan` and the path:

- a file that is not a plan
- a foreign format version
- an impossible level count or cluster stride
- an unknown geometry-kind code
- row-indexed sections of disagreeing length

Every run also checks the plan's row domain against the row groups selected
by `--bbox` and `--filter`.

A fingerprint records the tylertoo version, thinning flags, and input
identities. Mismatches name the changed field:

```
--plan: saved plan does not match this run: input "roads.parquet" mtime was
"1790242802390408452" when the plan was saved but is "1790244114398193000"
now. Re-run without --plan (add --save-plan to write a fresh one).
```

### Input identity

The fingerprint records these fields for every input part:

| Field | Local file | Remote object |
|---|---|---|
| Path / URL | Yes | Yes |
| Byte size | Yes (`stat`) | Yes (Content-Length) |
| Row count | Yes (footer) | Yes (footer) |
| Row-group count and pruned selection | Yes | Yes |
| mtime | Yes | No |
| Content hash / ETag | No | No |

Row counts are essential because the winner table is indexed by row
position. Replacing an input with fewer or more rows could assign incorrect levels
or index out of bounds; footer count checks reject both. Remote parts
produce a warning listing which identity fields are checked.

The fingerprint checks staleness, not content integrity. `cp -p` and
`rsync -a` preserve mtime and size; remote objects can be replaced with the
same size and row count.

The fingerprint deliberately leaves out these flags, so one plan replays
across them: `--profile`, `--row-group-size`, `--row-group-size-policy`,
`--full-column-stats`, `--read-batch-size`, `--in-flight-batches`,
`--spill-dir`, and `--cogp-compat`. It covers everything that changes *which*
features land at *which* level, and output with `--plan` is byte-identical
to the run that saved it.

### Shared plans for sharded builds

Shards need one shared plan because assignment depends on the whole input:

- The density budget water-fills a 128 × GSD super-cell budget over *every*
  candidate of a level.
- The level walk carries a running kept count from coarse to fine.
- `--magnitude-ladder` dense-ranks the *global* distinct values of its column.
- The automatic class ranking picks its column from a global vocabulary scan.

Recomputing assignment on each shard would produce inconsistent levels, so
`tiles --shard` requires `--plan`. A shard reads only planned row groups
whose bbox reaches its tile range. Validation allows this subset, and the
plan's row tables are remapped to the shorter stream before pass 2. See
[Scaling](guides/scaling.md#sharded-builds).

### Paths and output replacement

Both plan paths are checked before scanning. `--save-plan` needs an existing
writable parent and, if present, a writable file. The plan is written after
assignment; overwriting an existing plan is logged. `--plan` must be
readable and have valid magic bytes. Future format versions are identified
in the error.

Every subcommand that writes a file
(`overview`, `tiles`, `export-pmtiles`, `decode`, `pyramid`, `merge`, and
`shard-plan`) refuses an existing output unless given `-f`/`--force`. The
GeoParquet writers, `overview` and `decode`, build their output in a uniquely
named sibling (`OUTPUT.<random>.partial`) and rename it over `OUTPUT` only
after writing the footer. A run killed part-way therefore leaves a previous
output intact. A failed run removes the sibling. After a killed run, delete
it by hand.

---

## Worked scenarios

### Detail and visibility

- Too few coarse features: lower `--point-thinning`, `--line-thinning`, or
  `--polygon-thinning`; lower `--line-visibility` or
  `--polygon-visibility`; or raise `--gsd-base`.
- Disconnected coarse roads: lower `--line-thinning` or
  `--line-visibility`, keep
  [coalescing](#line-coalescing-no-coalesce-lines-coalesce-junction-angle-coalesce-snap-coalesce-max-level-rows)
  on, or raise `--gsd-base`.
- Jagged or over-smoothed geometry: lower `--simplify-factor`, for example
  from 1.0 to 0.5.
- Residential roads displace highways: use `--class-rank road_class:…` or
  automatic ranking (leave `--no-auto-rank` unset).
- Coarse levels are too large or slow: raise thinning factors or
  `--simplify-factor`, or lower `--gsd-base`.
- Small buildings vanish early: lower `--polygon-visibility` or use
  `--collapse` for points.
- Empty country view of buildings or parcels: use
  `--polygon-visibility 0 --collapse` and a circle layer
  ([dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)), or
  `--collapse-square` for polygons.
- Whole map is too sparse or dense: adjust `--gsd-base` (up = denser,
  down = sparser).
- Too many mid-zoom features (about z9–z12), or large duplicating files:
  raise `--drop-rate`. `--no-density-drop` disables the budget entirely.
- Sparse rural areas disappear under the budget: raise `--drop-gamma`.
- Dense points look misleadingly sparse: use `--cluster` and scale symbols
  by `point_count`.
- Need cluster totals or averages: add `--accumulate-attribute col:sum` or
  `col:mean` with `--cluster`.
- Cell aggregates or prebuilt levels lose features: use `--verbatim`.

### Remote reads

- Huge footer on every query: leave `--full-column-stats` unset to suppress
  string and geometry statistics.
- Need row-group skipping on property predicates: enable
  `--full-column-stats`, accepting a larger footer.
- Tiny viewports fetch too much: lower `--row-group-size` for tighter bbox
  pruning.

### Speed and memory

- Conversion exhausts RAM or swaps: lower `--read-batch-size`, keep
  streaming enabled, and see [Scaling](guides/scaling.md).
- Cores remain idle: raise `--in-flight-batches` for more read/compute overlap.
- `--profile speed` exhausts RAM: use `bounded` to spill each level.
  The default `auto` spills when the estimate exceeds its budget.
- `memory allocation of N bytes failed` on a musl binary at v0.7.1 or
  earlier, despite available RAM: use `--profile bounded` or upgrade.
  Later binaries ship mimalloc to avoid musl `mallocng` fragmentation
  ([#480](https://github.com/geoparquet-io/tylertoo/issues/480)).

[`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)
holds the corpus sweeps behind the defaults, including `--line-thinning` ×
`--simplify-factor` on Portland roads and the `--drop-rate` calibration
against tippecanoe.
