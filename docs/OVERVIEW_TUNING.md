# Overview generalization tuning

`tylertoo overview` turns a GeoParquet file into a multi-resolution
[overview file](tutorials/madagascar.md#3-build-and-validate-the-overview):
several precomputed generalizations of the dataset at increasing detail.
Level `0` is the coarsest, and level `L-1` is the finest, or canonical, level.
Two families of knobs control how much detail each coarse level sheds:

- **Thinning and visibility** decide which whole *features* survive at a level.
- **Simplification** decides how many *vertices* each surviving feature keeps.

Every knob is a multiple of the level's ground sample distance (GSD): the
smallest ground distance, in meters, that the level resolves. `--gsd-base` or
`--gsd` sets the GSD ladder. This page gives each knob's effect, default,
units, and direction, and how the knobs interact. For a symptom-first index,
start at [Worked scenarios](#worked-scenarios). The
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

`--gsd-base` is the **master detail knob** for a zoom-range plan. It scales
the whole ladder at once:

- **A larger `--gsd-base`** gives smaller GSDs at every zoom, so coarse levels
  come out denser, more detailed, and larger.
- **A smaller `--gsd-base`** gives larger GSDs, so coarse levels come out
  sparser, cruder, and cheaper.

The default `1024` follows the cogp-rs convention: about 4× a 256 px tile, so
sub-pixel features drop (spec §5.2, Q6). `--gsd` already gives absolute
meters, so `--gsd-base` has no effect alongside it.

Every other knob on this page is a multiple of the level GSD, so changing
`--gsd-base` moves them all together. Use a per-family knob to rebalance one
geometry class, and `--gsd-base` when the whole map is too sparse or too
dense.

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

tylertoo clamps the result to `[--min-zoom, 16]`. tylertoo logs the chosen zoom and
its measurements at `info`. Treat it as a starting point, the way tippecanoe
users treat `-zg`. In Python the log goes to Rust's logger, which Python's
`logging` does not receive. Read the chosen zoom from the finest `zoom` in
the report's `levels` instead.

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

A **pyramid** serves one map from different inputs at different zooms, in one
archive. A typical pyramid holds a pre-aggregated summary at coarse zooms and
the raw features at fine ones. A client switches inputs at the handover with
no second request.

```bash
tylertoo pyramid fire-2023.pmtiles \
  --band "0-5:cells_r5.parquet:aggregate" \
  --band "6-8:cells_r8.parquet:aggregate" \
  --band "9-14:points.parquet:features"
```

| layer | source | zooms |
|-------|--------|-------|
| `aggregate` | r5 cells | z0–5 |
| `aggregate` | r8 cells | z6–8 |
| `features` | raw detections | z9–14 |

Two bands may share a layer name, as `aggregate` does here. To a client it is
one layer whose content changes at z6. Two bands of the **same** layer must not
share a zoom, because they would write the same tile ids. tylertoo checks this
before it tiles anything, so a bad plan costs an error, not a conversion.

Bands in **different** layers may share zooms, as with tippecanoe's `-L`. At a
shared zoom, a tile that every band wrote carries every band's layer, and a
tile that one band wrote passes through untouched. So
`--band 0-13:a.parquet:2024 --band 0-13:b.parquet:2025` builds one archive
with two independent layers over the same zooms.

**Bands tile verbatim by default.** A band's input already has the right
resolution for its zooms, and the generalization ladder would undo that. Pass
`--generalize` when a band holds raw features over several zooms and needs
the ladder.

**A band is GeoParquet or a PMTiles archive.** Both use
`--band LO-HI:PATH[:LAYER]`, and tylertoo detects the kind from the file
contents, not the extension. It tiles a GeoParquet source over the band's
range and merges an archive as-is. You can mix the two: reuse last week's
coarse archive and re-tile only the fine band.

**A GeoParquet band may be remote.**
`--band 9-13:https://data.source.coop/…/features.parquet` streams with
byte-range requests, as `tiles` and `overview` do. A band archive must be
local, because the merge reads its directories and tiles by offset. tylertoo
refuses a remote `.pmtiles` band and says to stage it.

**Layer names and colons.** `LAYER` defaults to the input's file stem, or the
directory name for a glob or a directory. A path may contain colons
(`s3://…`, `https://…`, `C:\…`). tylertoo splits the layer off the **last**
colon only when the text after it has no `/`, `\`, or `:`. A URL scheme, a
port, and a `2024:06/` directory therefore stay part of the input. A
drive-relative path such as `C:data.parquet` also stays whole: a single ASCII
letter before the last colon counts as a drive letter.

The colon rule cannot express an input that *ends* in a bare colon segment,
such as the Hive directory `admin:country_code=BR`. Spell those bands with
`=` instead: **`--band LO-HI=INPUT[=LAYER]`**. tylertoo splits the range at
the first `=` and the layer at the last. As before, it splits only when the
text after it has no `/`, `\`, or `:`. That keeps `--band 0-13=admin:country_code=BR/part.parquet`
whole. An input that itself ends in `=VALUE` reads as a layer, so give it an
explicit `=LAYER`.

For a pre-tiled archive the layer is a label. The layer name inside its tiles
is whatever its export wrote. When two bands share zooms, those names must
agree. Otherwise the combined tile holds two layers of one name, and a client
drops one.

**Gaps and partial archives.** A gap between bands warns: the merged archive
advertises one continuous range, so a client requests the uncovered zooms and
gets nothing. An archive may hold more zooms than its band declares. One
archive splits across bands that way, for example two bands on the same
z0–13 archive declaring `0-5` and `6-13`. An archive that holds *fewer* zooms
than its band declares is an error, because those zooms would render empty.
The cause is almost always a band range that disagrees with the archive. Pass
`--allow-missing-zooms` for a deliberately sparse pyramid. A band whose range
shares no zoom with its archive is always an error.

**Per-band export settings.** `--max-tile-size` is unset by default for
pyramid bands, so a band keeps every cell it exists to draw. When you set it,
the cap applies to each band's tiles before tylertoo combines them.
`--feature-order` applies to every GeoParquet band alike, as `--generalize`
does. A band that lacks the column exports in input order with a warning
naming its layer. An archive band keeps the order it had. tylertoo writes
bands coarsest first, whatever order you list them in.

**Disk.** Intermediates go to `--work-dir`, by default the system temp
directory, and tylertoo removes them whether the build succeeds or fails.
Budget for them. Each band tiles in duplicating mode, so its intermediate
overview holds roughly *levels × input size*. Every band's finished archive
sits on disk before the merge, and the merge spools through the temp
directory.

---

## Switching the ladder off entirely: `--verbatim`

The rest of this page describes generalization: thinning, visibility gates,
simplification, the density budget, and coalescing. That suits inputs whose
coarse levels should be coarser drawings of the same features, such as a road
network or a building footprint layer.

It fails an input that already has the right resolution for the requested
zooms. An H3 r6 cell is not a simplified r7 cell. It is their parent, and its
count is their sum. The gates ask "is this feature big enough to see" when the
question is "what do these cells sum to". A coarse level then shows some
cells and silently omits the rest:

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

Use it in three cases:

- The input is a discrete global grid system (DGGS) or other cell aggregate.
- An upstream tool already built the levels.
- The input is **one band of a
  [pyramid](#several-inputs-one-archive-tylertoo-pyramid)** whose coarser
  zooms come from another input.

It leaves the mode, the level plan, the coordinate reference system (CRS),
the row-group layout, an explicit `--cluster`, and `--representation` alone.
A point or square band still replaces polygons with centroids or placeholder
squares. That is the one generalizing knob left on, and it stays inert unless
you ask for it.

`--verbatim` supplies **defaults** and overrides nothing. Any knob you set
explicitly wins, so `--verbatim --simplify-factor 0.5` keeps every feature
but still simplifies geometry. The run reports the override instead of
claiming verbatim output.

It requires `--mode duplicating`. Partitioning writes each feature at exactly
one level. With thinning off, every feature would land in the coarsest level
and every finer level would be empty, so tylertoo rejects the combination.

On `tiles`, `--verbatim` also disables the per-tile size cap: a valve that
sheds features to fit a byte budget is not verbatim either. An explicit
`--max-tile-size` puts a cap back.

⚠️ **The two-step form does not inherit that.** `tylertoo overview --verbatim`
followed by `tylertoo export-pmtiles` still applies the export's 500K cap and
sheds features from oversized tiles. The flag acts at convert, and the export
reads a file, not your flags. Pass `--tile-size-limit 0` (alias
`--max-tile-size 0`) to `export-pmtiles` when every feature must reach a tile.

### `0` is the off switch

Every thinning factor accepts `0`, meaning **no thinning**: each feature is
its own grid cell, so every feature that passes the gates survives. Negative
and non-finite factors are errors.

---

## Feature thinning: `--point/line/polygon-thinning`

Thinning keeps **one winning feature per grid cell** per level, and the
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

A bigger factor means bigger cells, fewer cells, fewer survivors, and a
**sparser** map. A smaller factor gives a **denser** one.

⚠️ **The direction is counter-intuitive.** A *bigger* thinning number makes
the map *sparser*, because it multiplies the cell size. If coarse roads look
too empty, **lower** `--line-thinning`.

The defaults depend on the geometry class. Points thin hardest, at 4.0,
because they clutter first. Lines and polygons thin least, at 1.0. In the
Portland roads sweep, a line factor of 1.0 kept road networks visibly more
continuous at coarse zooms at little cost in legibility
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)).
The factor multiplies the GSD, so it stacks with `--gsd-base`. Doubling
`--gsd-base` halves the GSD, which halves the cell size for the same factor.

---

## Visibility gates: `--line/polygon-visibility`

A line or polygon is **eligible** at a level only if its bounding-box diagonal
clears the gate:

```
eligible  ⇔  bbox_diagonal >= visibility_factor * gsd(level)
```

tylertoo **drops** a feature below the gate at that level. It reappears at
finer levels once the GSD shrinks below its size. Points have no gate.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--line-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |
| `--polygon-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |

A bigger factor raises the bar and drops more small features at coarse
levels. A smaller factor keeps more of them.

The polygon default matches what the write stage keeps anyway. The write stage
drops any polygon whose *simplified* geometry falls below the level tolerance
(`--simplify-factor × GSD`), an effective survival bar of about 2 × GSD on
real shapes. A stricter gate only starves coarse zooms. On Germany buildings,
2.0 instead of 4.0 gives 2.7–4.6× more features at z8–z12 and starts the
pyramid one zoom coarser, for 13% more file size and 7% more conversion time
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md),
Decision 6). Values *below* 2.0 change little on their own, because
simplification collapses nearly every extra feature at write time. The
exception is `--collapse`, which keeps them as representative points, as in
the [dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers).

⚠️ **The gate scales with the GSD**, so it moves with `--gsd-base` and with
zoom. A polygon visible at one level can fail the gate one level coarser only
because the GSD, and with it the gate, doubled. This is a *hard drop*,
distinct from thinning's *one-per-cell* competition: a feature can fail the
gate even when its cell is otherwise empty.

### Empty coarse levels: the auto-clamp

When the gates cull **every** feature at a coarse level, tylertoo omits that
level and renumbers the rest (spec §7.3) instead of failing. Small features at
world zooms do this routinely. With country-scale buildings and
`--min-zoom 0`, for example, no building's bbox clears `2 × gsd(z0)`, about
78 km. The run prints a `WARN` like:

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

Those features have no drawable form at those scales, so the clamp loses no data. To populate
coarse levels anyway, lower the gates, raise `--gsd-base`, or, for dense
small-polygon layers, use the
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

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--simplify-factor` | `1.0` | × GSD (RDP tolerance) | **bigger = cruder + lighter** |
| `--collapse` | off | flag | a below-gate polygon becomes a representative point instead of dropping |
| `--collapse-square` | off | flag | dropped polygons stand in as placeholder squares of about 1 × GSD ([details](#zoom-band-representation-and-placeholder-squares)) |
| `--representation` | none (all `geom`) | `LO-HI:KIND,…` | per-zoom-band representation: `geom`, `point`, or `square` |
| `--no-cascade` | off (cascading **on**) | flag | disables cascading simplification |

- **A lower `--simplify-factor`** keeps more vertices: crisper but heavier
  coarse levels.
- **A higher factor** keeps fewer vertices: cruder, blockier, lighter levels.

Simplification applies in duplicating mode only. The **canonical level always
stays verbatim**, whatever the factor (spec §2.4). `--simplify-factor 0`
disables simplification.

⚠️ **High factors also thin.** tylertoo drops a line or polygon whose bbox
diagonal falls below the tolerance instead of smoothing it. At the extremes,
`--simplify-factor` removes features as well as vertices. To shed only
vertices, keep the factor modest and set density with the thinning and
visibility knobs.

### Cascading simplification (default on): `--no-cascade`

In duplicating mode, each coarser level simplifies the **next-finer level's
already-simplified output**, as tippecanoe does, not the canonical geometry.
Without cascading, every level a feature appears on re-simplifies it from full
resolution, and that repeated work dominates duplicating-mode conversion
time. Cascading also repairs a self-intersecting RDP candidate into its valid
even-odd interpretation in one boolean-overlay pass.

Coarse-level coordinates differ slightly from non-cascaded output. Cascaded
vertices are still a subset of canonical vertices, and every step's output is
validity-checked or repaired. The geometric GSD ladder bounds the cumulative
deviation at about 2× the target level's tolerance instead of 1×. The footer
provenance records `generalization.cascade: true`. Pass `--no-cascade` to
reproduce non-cascaded output byte-for-byte.

**Validity-check vertex cap.** A simplification candidate with more than 2,048
total vertices skips the exact validity check and counts as valid. The check
is O(V²) in ring size, and on continental-scale rings at fine GSDs it stalled
conversion for tens of minutes per feature. A candidate that large means RDP
removed few vertices from already-valid input, the case least likely to
self-intersect. The overviews spec does not require valid geometry (§2).
Candidates at or below the cap get the full check and repair. tylertoo logs a
count of skipped checks at `info` when conversion ends.

---

## Country-scale dot fill for dense polygon layers

A dense layer of *small* polygons, such as buildings, parcels, or field
boundaries, renders an **empty country view** with type-preserving defaults,
however you tune the gates. Two independent mechanisms cull sub-GSD polygons
at coarse levels:

1. The **visibility gate** at assignment: a polygon below
   `--polygon-visibility × GSD` is ineligible.
2. The **write-time collapse** at simplification: tylertoo drops even an
   eligible polygon when its simplified geometry falls below the level
   tolerance. At z4 the tolerance is about 2.4 km, and no building survives
   that.

No level from z0 to z8 can draw a 20 m building *as a polygon*, and the fix
is to draw it as something else. **`--collapse`** does that (spec Q4, opt-in):
a below-tolerance polygon becomes a **representative point** instead of
vanishing. Combined with a zero gate, it turns the coarse levels into a **dot
field** that cell winners and the budget bound:

```bash
tylertoo overview buildings.parquet buildings_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse

# One-shot to PMTiles. The 500K tile cap is the default, shown here
# for clarity; --max-tile-size 0 turns it off.
tylertoo tiles buildings.parquet buildings.pmtiles \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse --max-tile-size 500K \
  --profile bounded
```

Two settings in the `tiles` call matter at country scale:

- **The tile cap.** Uncapped coarse dot tiles on a country-scale layer reach
  several MB (Germany z6: 12 MB), far past renderer norms. The cap thins them
  to about 500 KB with a spatially even stride of the dots.
- **The memory profile.** On multi-GB inputs the recipe buffers much larger
  coarse levels. The default `--profile auto` estimates this and spills when
  needed (Germany's peak resident set size rises from 15 to 29 GB if forced to `speed`).
  `--profile bounded` forces spilling.

On Overture Germany buildings (59M footprints, z0–14), every level populates:
z0 holds 581 dots, z4 128,886, and z6 1,074,540
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md),
Decision 6). Levels z6–z13 land on the
[density budget](#density-budget-drop-rate-drop-gamma-no-density-drop) ladder
`N / 1.65^(14−z)`, with dense areas such as the Ruhr and Berlin visibly denser
and rural areas protected. The overview file grows 31% (11.9 → 15.6 GB), and
conversion takes 40% longer. On the Moldova field corpus the same recipe adds
**0.3%** to file size.

Two caveats:

- **Style the points.** A `fill` layer ignores Point features, so a fill-only
  style renders the same empty country view you started with. Add a small
  `circle` layer filtered to `["==", "$type", "Point"]`.
- **The geometry type changes mid-zoom.** The output's `geometry_types` lists
  the union, for example `["Point","Polygon"]` (spec §7.5). Collapse is opt-in
  so that no renderer meets points by surprise (spec Q4). Files record the
  collapse in the `generalization` provenance.

`--drop-rate` and `--drop-gamma` need **no** retuning for this recipe. The
existing budget ladder caps the coarse levels and shapes the dot density. A
higher `--drop-rate 1.3` only inflated mid-zoom row counts by 13% without
changing the coarse fill.

---

## Zoom-band representation and placeholder squares

The dot-fill recipe changes the representation of below-tolerance polygons
**at every zoom**. Two further knobs give per-zoom-band control and a
**type-preserving** alternative to points.

### `--representation LO-HI:KIND,…`: the band selector

One run builds one archive with a different representation per zoom band, with
no two-archive merge:

```bash
# Dots zoomed out, full polygons zoomed in, in ONE PMTiles:
tylertoo tiles buildings.parquet buildings.pmtiles -f \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point,8-14:geom"

# Tippecanoe-style placeholder squares at coarse zooms instead:
tylertoo tiles buildings.parquet buildings.pmtiles -f \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:square"
```

`KIND` per band:

- **`point`**: every polygonal feature in the band becomes its
  **representative point**, whatever its size. The point is the centroid,
  falling back to the bbox center and then the first vertex for degenerate
  rings. In-band polygons **bypass the visibility gate**, since a dot is
  always visible, and thin on the **point grid** (`--point-thinning`). The
  band renders as a dot field with real coverage. Style it with a `circle`
  layer, because fill layers ignore points.
- **`square`**: normal simplification, except that a **below-tolerance**
  polygon becomes an area-dithered **placeholder square** of about 1 × GSD
  instead of dropping (see `--collapse-square` below). Visible polygons stay
  untouched, and the level stays all-`Polygon`, so plain fill styles keep
  working. In-band polygons bypass the visibility gate, because the dither
  must see the tiny ones, but keep the polygon thinning grid.
- **`geom`**: the normal path, and the default for zooms no band lists.
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

`--collapse-square` is the third **below-tolerance disposition**, after the
default drop and `--collapse` to a point. A polygon a level cannot show as
itself stands in as a `tol × tol` square, where `tol` is
`simplify-factor × GSD`, CRS-converted. This is tippecanoe's **tiny-polygon
reduction**, the primary reference's way to keep dense small-polygon layers
(fields, buildings, parcels) visible at coarse zooms with **no style
changes**. Squares are polygons, `geometry_types` stays `["Polygon"]`, and no
spec-Q4 geometry-type opt-in applies.

`--collapse-square` is opt-in, and the default disposition is still to drop. The
footer provenance records it as `generalization.collapse: "square"`, and
`--collapse` records `"point"`.

Two mechanisms share the threshold `T = side²`, where `side` is `tol` floored
at one tile unit of the level's zoom (see the divergences below).

**The accumulator** covers every polygon the level does *not* carry: those
that failed the visibility gate, lost their thinning cell, or fell to the
density budget. After assignment, each such polygon adds its area,
**clamped to `T`**, to a running total for its **patch**, in input order. A
patch is a 32 × GSD square, 1/32 of a 1024-px tile. Each time a patch's total
crosses `T`, the polygon that crossed it becomes the level's *carrier*.
tylertoo emits it as a `T`-area square at its representative point, with its
own attributes.

A polygon contributes at most one placeholder of area. A gate-failed polygon
is often bigger than `T`, because the gate is `--polygon-visibility` pixels
wide, and a thinning or budget loser can be any size. A patch keeps less than
one `T` unemitted. The accumulator is what makes a country of 25 m fields
read as farmland at z0 instead of vanishing. On a 368k-field sample, z1–z6
carry about 98% of the input area, against 1.5–7% with the dither alone. An
entry-zoom ladder (`--entry-zoom`) decides where its features first appear,
so the accumulator never counts them.

**The dither** covers polygons the level *does* carry but that RDP shrinks
below `T` at write time. A polygon of area `A` survives as a square with
probability `A / T`, decided by a hash of its anchor coordinates. The two sets
are disjoint, so nothing counts twice.

Both mechanisms are **deterministic**. The accumulator runs once over the
pass-1 feature table in input order, so every engine (in-memory, streaming,
pipelined) reads the same carrier set. The dither is a pure function of the
feature. The same input produces byte-identical output across runs, engines,
and thread counts. Under cascading, a kept square's anchor is its own center.
Coarser levels re-dither it against the same hash draw with a shrinking keep
probability, so survival is monotone from fine to coarse.

Divergences from tippecanoe (see `context/ARCHITECTURE.md`):

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
  `s²`. With the floor, the encoder draws every placeholder and there are fewer of them, and
  the log names the zooms where the floor applied. On those levels a patch's
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

At mid zooms a dense square carpet can exceed `--max-tile-size`. The valve
then sheds squares for that tile, as it would any feature. Raise the cap or
accept the thinner carpet. [`tylertoo stats`](#export-and-archive-commands-export-pmtiles-merge-stats)
shows how close each zoom runs to the cap.

---

## Attribute-driven entry zoom: `--magnitude-ladder`, `--entry-zoom`

[Ranking](#ranking-which-feature-wins-a-cell) decides *which of the features
competing for a cell wins*. It runs **after** the visibility gate has dropped
the small features. That order is backwards whenever a dataset's most
important features are also its physically smallest.

A population-density choropleth is the everyday case. The dense urban tracts
with the highest values are tiny next to the sparse rural ones. A coarse
level therefore keeps the large low-value polygons and hides the small
high-value ones, the opposite of what the map should show zoomed out.
`--sort-key` helps where features survive to compete, but cannot reach a
feature the gate removed first.

A **ladder** answers a different question: not "which of these wins a cell"
but "how early may this feature appear at all". Each distinct value of a
column gets an *entry zoom*. A feature appears from its entry zoom inward and
at no coarser zoom, **exempt from the visibility gate and from thinning**
throughout.

```bash
# Derive it: distinct values ranked descending, one zoom apart from --min-zoom
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 6 \
  --magnitude-ladder density

# ...or place the rungs by hand. Rungs must fall inside the zoom range, and
# each names a value the column actually carries.
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 8 -f \
  --entry-zoom "density:5000=0,1000=3,200=6"
```

With five distinct values over `--min-zoom 0 --max-zoom 6` and the default
step, the derived ladder gives a clean staircase of feature counts. The
strongest rank alone appears at z0, and each weaker rank joins one zoom later.
Without a ladder the strongest values do not appear until the gate stops
removing them. That is typically at the finest zooms, because they are the
layer's smallest features.

The ladder ranks **distinct values** (SQL `DENSE_RANK`), not the values
themselves, which keeps it scale-free. Real values usually occupy a narrow
part of their nominal scale. A linear map from raw value to zoom then strands
every rung in the upper zooms.

A column may have more distinct values than the zoom range has room for.
Ranks that would fall past the finest zoom get no rung. Their features take
the ordinary gate and thinning, and tylertoo does not pin them to the finest
level. The run reports how many it left out.

`--ladder-step N` (default 1) widens the spacing. Some values get no rung:
a null, or a value an explicit spec leaves out. Those features get no entry
zoom and take the ordinary gate. A partly populated column therefore degrades to the ordinary
behavior instead of hiding rows.

### Two mechanisms can still undo a ladder

The ladder governs **admission**. Two later stages can remove a feature it
admitted, and both matter in practice:

- **Simplification.** A feature admitted to a coarse level still drops there
  if its geometry simplifies below that level's tolerance, which is usual for
  small-but-strong features. `--magnitude-ladder` and `--entry-zoom`
  therefore **imply `--collapse`**, so such a feature survives as a
  representative point. `--collapse-square` overrides that.
- **The density budget** caps each level and sheds its lowest-priority
  survivors *by size*, the same inversion the ladder corrects. Pair the
  ladder with `--no-density-drop`, or with `--sort-key` on the same column so
  the budget ranks the way the ladder does. tylertoo warns when a ladder runs
  with the budget on and no sort key.

With the budget off, the staircase is exact. Each rank appears at its own zoom
and at no coarser one, and every rank already admitted stays:

```
zoom     rank 4  rank 3  rank 2  rank 1  rank 0   (0 = highest value)
z0            0       0       0       0     140
z1            0       0       0     140     140
z2            0       0     165     165     165
z3            0     192     176     176     176
z4          250     192     176     160     160
```

### Relation to tippecanoe

The ladder is tippecanoe's per-feature `tippecanoe.minzoom`, the one
attribute-driven thinning lever it offers. Tippecanoe takes the minzoom as an
input attribute. `--magnitude-ladder` also *derives* one from a column, which
callers otherwise compute by hand before tiling.

---

## Ranking: which feature wins a cell

When several features compete for one grid cell, the highest-priority feature
**wins**. The priority tiers are, highest first (spec §3.5, Q1):

1. `--sort-key COL`: a numeric column, such as population or importance.
2. `--class-rank COL:VAL=RANK,…`: an explicit categorical map, such as
   `road_class:motorway=5,primary=4,residential=2`. Unlisted values rank
   below every listed one but above nulls.
3. **Automatic detection**, unless you pass `--no-auto-rank`. Overture roads
   (`class` or `road_class`) get a built-in motorway-to-service ranking, and
   Overture places rank by `confidence`.
4. **Size fallback**: the larger bbox diagonal wins, and a deterministic hash
   breaks ties, as in tippecanoe.

Ranking changes *which* features survive, not *how many*. Thinning sets the
count. For road networks, a good ranking keeps highways visible at coarse
zooms instead of a random scatter of residential streets. The
`generalization.ranking` provenance in the footer records the tier used.

`--sort-key` and `--class-rank` are mutually exclusive.

**Unrankable values.** A null ranks below every real key. A feature with no
key still appears and only loses any cell it contests with a keyed feature. A
NaN or infinity in a numeric column ranks the same way, because float
columns usually spell nodata that way and neither is a comparable priority.
tylertoo never drops such a row for it. The row competes as a keyless feature.
The entry-zoom ladder reads its column by the same rule, so a non-finite value
is no rung. `--accumulate-attribute` aggregates rather than ranks and is one
notch looser: it skips a NaN too, but sums an infinity as a real value (see
[Clustering](#clustering-cluster-accumulate-attribute)).

---

## Density budget: `--drop-rate`, `--drop-gamma`, `--no-density-drop`

Cell-winner thinning stops binding once its grid cell is smaller than the
typical feature spacing. From roughly z9 up, *every* feature wins its own
cell, so per-level counts plateau at about the whole dataset. On Portland
roads that plateau is 2–3× tippecanoe's feature count at z9–z11. It clutters
the map and drives most of duplicating mode's storage overhead.

The **density budget** handles this the way tippecanoe does. After
cell-winner thinning, tylertoo caps each level at a feature **budget** that
decays geometrically toward coarse zooms. It drops the lowest-priority
survivors, in [ranking](#ranking-which-feature-wins-a-cell) order, until the
level meets its budget.

```
budget(level) = N / drop_rate ^ (finest_level − level)      (N = input features)
keep(level)   = min(cell_winner_survivors(level), budget(level))
```

The finest (canonical) level keeps everything (spec §2.4). The budget is a
*ceiling*. A level already sparser than its budget stays untouched, and that
includes every coarse zoom, where cell-winner thinning did the work. The
budget therefore bites only the mid-zoom plateau.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--drop-rate F` | `1.65` | ratio (>1) | **bigger = sparser mid zooms** |
| `--drop-gamma F` | `1.5` | exponent (≥1) | **bigger = more sparse-area protection** |
| `--no-density-drop` | off | flag | disables the budget |

**`--drop-rate`** sets the strength. Each coarser level keeps `1/rate` of the
next finer one. A bigger rate makes coarse levels shed harder, for sparser
mid zooms and smaller files. A smaller rate is gentler. The default `1.65`
comes from Portland roads. It brings z9 to 1.21× and z10 to 1.03× tippecanoe's
counts and puts z11 at 0.67×. It leaves z8 and the coarse zooms near their
cell-winner counts
([`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)).

> **Why 1.65, not tippecanoe's 2.5?** tylertoo's budget anchors on the
> *full canonical count* `N`, since every feature appears at the finest level.
> Tippecanoe's `-rate` is relative to a per-tile basezoom count. An equivalent
> per-level thinning therefore lands at a smaller numeric rate. A rate of
> `2.5` here over-thins, putting Portland z9–z13 below tippecanoe.

**Spatial fairness (`--drop-gamma`).** A global rank-ordered cut would empty
sparse rural areas to keep dense cities under budget. Instead, tylertoo shares
the per-level budget across coarse **super-cells**, neighborhoods of
`128 × GSD`. Each super-cell keeps its top-priority features up to an
allocation `∝ population^(1/gamma)`, water-filled so no cell gets more than it
has:

- `gamma = 1` is a proportional cut: every neighborhood keeps the same
  fraction.
- `gamma > 1` is **sublinear**: dense neighborhoods keep proportionally fewer
  features, and sparse ones proportionally more.

This is tippecanoe's `-g` gamma dot-dropping ("reduce dots to the `1/gamma`
power in dense areas"), applied per super-cell. `--drop-gamma` does **not**
change per-level totals. It only redistributes *which* features survive
spatially, so it is independent of `--drop-rate`.

⚠️ **Points may not feel the budget.** The budget applies to points, lines,
and polygons alike, but `--point-thinning` (default 4) already thins points
hard. On a large point dataset, such as New York City points of interest, the
cell-winner point counts often sit *below* the budget at every zoom.
`--drop-rate` then seldom binds. Address point over-retention with
[clustering](#clustering-cluster-accumulate-attribute) instead. Lines and
polygons, at thinning factor 1, are where the budget does the most work.

**`--no-density-drop`** turns the budget off, leaving pure cell-winner
thinning and a footer without the budget block. It affects only the mid-zoom
plateau. The `geo:overviews` → `generalization.density_drop` provenance in
the footer records the mechanism and its parameters (`drop_rate`, `gamma`,
`supercell_gsd_factor`).

---

## Clustering: `--cluster`, `--accumulate-attribute`

By default, a point that loses its thinning-grid cell does not appear at that
level. The survivor says nothing about how many features it stands for.
**`--cluster`** (opt-in, duplicating mode only) makes the survivor **absorb**
its cell's losers instead:

- Every output row gains a **`point_count`** `INT64 NOT NULL` column: the number
  of source features the row represents at its level, following the
  tippecanoe and supercluster convention. Every value at the canonical level
  is 1. Lines and polygons always carry 1, because clustering applies only to
  points.
- The winner keeps its **own geometry and attribute values**. A cluster stays
  anchored on a real feature instead of moving to a centroid. This diverges
  from supercluster's re-centering on purpose: it is deterministic, and the
  anchor stays a real place.
- Absorption is **per level**. A point absorbed at z4 may itself win at z6
  with its own smaller cluster. At each level,
  `sum(point_count) == total source point count`: the clusters partition the
  dataset at every level's grid.

Use it for graduated-dot rendering of dense point data such as points of
interest or addresses. The client scales the symbol by `point_count` instead
of drawing a misleadingly sparse field of constant-size dots.

**Clustering changes the `--point-thinning` default from 4.0 to 16.0.**
Without clustering, a coarse grid *discards* data, so the default stays
dense. With clustering, `point_count` summarizes the losers, so a sparser
grid loses nothing and gives the familiar graduated-cluster look.
The supercluster default radius is about 40 px, and 16 × GSD gives about one
dot per 16 display pixels. In the New York City sweep over factors 4, 16, and
48, each 4× step shifted the whole density ladder by two zooms. Pass
`--point-thinning` explicitly to override in either mode.

**`--accumulate-attribute COL:OP`** (repeatable, requires `--cluster`)
aggregates a numeric column across each cluster. The winner's value of `COL`
becomes the `OP` over itself and everything it absorbed at that level. The
ops are `sum`, `max`, `min`, and `mean`:

```bash
tylertoo overview places.parquet places_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --cluster \
  --accumulate-attribute population:sum \
  --accumulate-attribute confidence:mean
```

Notes:

- tylertoo computes aggregates **per level from source values**, never from
  coarser aggregates, so `mean` is exact at every level.
- Nulls do not contribute. A cluster whose members are all null keeps the
  winner's null. Non-accumulated columns keep the winner's own values.
- A **NaN** does not contribute either. It is far more often nodata than a
  value, and one NaN would poison every aggregate it touched. It does not
  count as a contributor, so `mean` is the mean of the real values.
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
- **tylertoo rejects partitioning mode.** A partitioning row serves many zooms
  through prefix reads but exists at one level, so one stored `point_count`
  cannot reflect every zoom's grid. Absorbed features would also reappear as
  their own rows at finer levels while still counted in coarser winners,
  double-counting every prefix sum.

The `geo:overviews` → `generalization.clustering` provenance in the footer
records clustering (`enabled`, `point_count_column`, `accumulated: [{column, op}]`).
`tylertoo validate` checks that the column exists as `INT64 NOT NULL` and that
canonical-level values are all 1.

---

## Line coalescing: `--no-coalesce-lines`, `--coalesce-junction-angle`, `--coalesce-snap`, `--coalesce-max-level-rows`

At coarse levels a line network, such as roads or rivers, can degrade into
scattered dashes. The visibility gate (`--line-visibility × gsd`) drops every
segment whose bbox diagonal falls below it, and cell-winner thinning keeps
disconnected fragments of the rest. Selection works, but *continuity* breaks.

Line coalescing is therefore **on by default**. Pass **`--no-coalesce-lines`**
to opt out. At each non-canonical duplicating level, it chains touching,
compatible segments into single "stroke" LineStrings **before** the gate and
thinning run:

- A chain of individually sub-visibility segments survives as **one long
  visible artery**, because the gate tests the chain's extent, not each
  fragment's. This ordering is the whole point.
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

By default, chains stop at junctions. In the Portland sweep, that strict
degree-2 chaining rendered better than junction continuation, which
over-merges. Set an angle to opt in. At each junction, tylertoo merges the
pair of lines that continues straightest, if its deviation is at most this
angle. It repeats for the next pair, so a four-way crossing continues **both**
through-streets. A bigger angle bends chains further through junctions,
giving fewer and longer strokes. At 30°, Portland's z0–z1 gain giant arterial
strokes. The cost is merging through real turns and smearing attributes
across crossings.

### `--coalesce-snap` (default 1.0, GSD multiples)

Endpoints that touch exactly always chain, and Overture and OpenStreetMap
segments share exact node coordinates. The snap pass also joins chain ends
within `factor × gsd` of each other, since two endpoints closer than one
ground sample look the same at that level. A bigger factor bridges larger
digitization gaps but risks fusing the ends of nearby parallel lines. `0`
disables the snap pass, leaving exact matches only.

### `--coalesce-max-level-rows` (default 2,000,000): memory guard

Chaining needs a level's candidate line geometries in memory at once. Every
non-canonical level's candidate set is **all** lines, because dropped
fragments must stay reclaimable. Over the ceiling, tylertoo skips coalescing
and warns, naming the limb that tripped. The file still carries the
`coalesced_count` column, all 1, and the provenance block, so the overview
schema stays stable. Tiles then omit the column, as for any all-1 counter.
Levels that large sit near canonical, where segments are individually visible
and coalescing matters least. This is the streaming pipeline's one deliberate
`O(lines)` residual allocation.

**The ceiling has two limbs**, because a row count does not bound memory. By
the model below, 2M two-point road segments retain 168 MiB, and 2M
500-vertex contour lines about 15 GiB. tylertoo skips coalescing when
**either** limb trips:

| limb | default | what it counts |
|------|---------|----------------|
| candidate lines | `--coalesce-max-level-rows` (2,000,000) | line features in the input |
| retained geometry | `--coalesce-max-level-rows × 512 B` (1.024 GB, about 977 MiB) | a fixed model: a 56 B slot plus 16 B per vertex, per line |

The byte limb is a **model**, not a measurement. It is a pure function of the
vertex counts, so the verdict never depends on the machine, the allocator, or
the `geo` version. Real resident memory runs higher, because allocator
headers, `Vec` growth slack, and side vectors add about 100–120 B per line.
When the byte limb binds, the default ceiling is about 1 GB modeled but
**about 1.2 GiB resident**. Short lines have the worst ratio: 2M two-point
segments model at 168 MiB but measured about 400 MiB resident.

**Which inputs the byte limb skips.** 512 B per line is 56 + 16 × 28.5, so the
byte limb trips *before* the row limb when lines average **more than about 28
vertices**. That means fewer than 2M lines that together carry more than
about 57–64 million vertices: 2M lines at 29 vertices, 1M at 61, or 250k at
253.

- *Not affected:* road networks split at intersections. Overture
  transportation segments average about 8 vertices (about 190 B modeled), so
  the row limb binds for them.
- *Affected:* **unsplit** line data, once there are enough lines to cross
  about 1 GB modeled. Examples include OpenStreetMap ways kept whole, rivers,
  streams, administrative boundaries, coastlines, contour lines, and GPS
  tracks.

To coalesce such an input anyway, raise `--coalesce-max-level-rows`. It scales
both limbs together: 4,000,000 allows 4M lines or 2.048 GB modeled. The cost
is that much more resident line geometry, about 1.2× the modeled figure,
**plus** the chain-stage peak below, which is a large multiple of it. Size the
machine first, or leave coalescing off for those layers.

Both limbs are pure functions of the input, so the verdict and the output are
identical across machines, engines (`--no-streaming` or not), and
`--read-batch-size` values. Pass 1 enforces the limbs *while it collects*:
the moment the running totals cross, it frees what it has buffered and falls
back to counting. On a 6M-line synthetic input, that cut the macOS peak
memory footprint from 4890 to 1157 MiB with byte-identical output.

**What the guard does not bound** is the chain stage itself, on inputs inside
the ceiling. Every overview level runs it over the whole line scratch. Pass 1
runs it for every planned level, because a level's row count is its chain
count, which the level plan needs before pass 2. Pass 2 reuses the tables
pass 1 built.

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

The same input with `--no-coalesce-lines` peaks at 935 MiB (`tiles`,
z0–z14). Pass 1 cannot know which levels are empty until it has run them.
A run whose coarse levels come out empty therefore still pays for chaining
them. To
force the lowest peak, set `TYLERTOO_AUTO_MEM_LIMIT_BYTES` low, for one level
per wave, or pass `--no-coalesce-lines`.

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

tylertoo appends its own columns to the intermediate overview GeoParquet and
reserves their names. Name comparison ignores case. tylertoo **renames** a
colliding source column by appending `_` until the name is free:
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

**PMTiles output gives the source name back where it is free.** tylertoo's own
`level` never reaches vector tile (MVT) properties, so tiles publish a
source `level` as `level`. The rename stays an implementation detail of the
intermediate GeoParquet, the only place the collision is real:

```
overview.parquet:   level_ (your data)  +  level (tylertoo's)
out.pmtiles:        level  (your data)
```

The same holds for a standalone `export-pmtiles` run on an earlier overview
file. The footer records the rename (`geo:overviews.generalization.renamed_columns`,
output name → source name), so the export does not need the converting run's
state.

Restoration is conditional. `point_count` and `coalesced_count` *are* real MVT
properties in the modes that append them. A source column moved aside from one
of those keeps its renamed name. Merging two columns into one tile property
would be worse than the wrong name. Only free names come back. A
`coalesced_count` omitted from the tiles (1 on every row) counts as free, so a
source `coalesced_count` then publishes under its own name.

By-name options follow the rename automatically (`--sort-key level`,
`class_ranking.column`, `--accumulate-attribute`, `--filter`), so you never
spell the renamed name yourself.

---

## Output feature order: `--feature-order`

MVT does not define draw order, but renderers paint features in the order the
tile lists them. The order tylertoo writes **is** the paint order for any style
that does not override it. A style that relies on it renders differently
against a differently ordered archive of the same data.

**tylertoo's default is input row order**: the overview file's row order,
which is the source file's row order restricted to the rows each level kept.
On a nested-polygon fixture across z12–z14, the within-tile sequence has zero
inversions against source row order at every zoom.

**Do not assume tippecanoe matches it.** Tippecanoe's order is incidental, not
specified. It varies by zoom and by tile, and on one input it can point the
opposite way at some zooms and the same way at others. If your style depends
on paint order, pin it:

```bash
# Source row order (the default)
tylertoo tiles in.parquet out.pmtiles --feature-order input

# Sort within each tile by a property: high `level` painted last (on top)
tylertoo tiles in.parquet out.pmtiles --feature-order level -f

# ...or first (underneath)
tylertoo tiles in.parquet out.pmtiles --feature-order level:desc -f
```

Both `tiles` and `export-pmtiles` accept it.

Sorting by a column fixes any nested-polygon choropleth, where small
high-value shapes sit inside larger low-value ones and must land on top.
`--feature-order level` does in the archive what
`"fill-sort-key": ["get", "level"]` does in every downstream style, so
consumers need not each know.

Details that make the result reproducible:

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
- **Naming a column the layer does not publish warns and changes nothing.**
  Every feature is then equally unranked, so the output is input order, which
  on its own looks exactly like success. The run lists the available property
  names so a typo is obvious.

`--feature-order input` is the default and costs nothing. Naming a column adds
one stable sort per tile over features already in memory.

---

## Stable feature ids: `--feature-id`

Every MVT feature carries an `id`. **Without `--feature-id` that id is
tile-local**: it is the feature's position within one tile at one zoom. The
same building gets a different id in the neighboring tile and at the next
zoom. An `id` exists, but nothing can key on it.

`--feature-id COLUMN` writes the column's value as the feature id instead, on
every tile and zoom the feature appears in. It matches tippecanoe's
`--use-attribute-for-id`:

```bash
tylertoo tiles buildings.parquet buildings.pmtiles --feature-id building_id
tylertoo export-pmtiles overview.parquet out.pmtiles --feature-id building_id
```

With a stable id, MapLibre's feature state works across tile and zoom
boundaries without `promoteId`. `map.setFeatureState({ source, sourceLayer, id },
{ hover: true })` highlights the same feature wherever the map draws it, and
external data joined on the id stays attached as the map pans and zooms.
`promoteId` still works on any property, with or without `--feature-id`. The
flag lets clients that read the MVT id directly skip it.

Rules:

- **Integer columns only**: `Int8` to `Int64`, `UInt8` to `UInt64`, or an
  unscaled `DECIMAL(p,0)`. MVT ids are unsigned 64-bit integers. Tippecanoe
  also parses numeric strings and integral floats, and tylertoo rejects them.
  A string id, such as an Overture GERS id, has no lossless integer form. Cast
  or hash it to an integer column before tiling, with `gpio` or DuckDB (for
  example `hash(id)::UBIGINT`), and keep the original string as a property if
  clients need it.
- **Every row must hold a value in `0..=2^64-1`.** A null, a negative, or an
  out-of-range decimal fails the export. Unlike tippecanoe, tylertoo does not
  turn `-5` into `18446744073709551611`. tylertoo checks the whole column of
  the overview file, every level, **before writing any tile**, so the error
  arrives in seconds. It names the overview file's row and level, not the
  source row, because the convert reorders rows into levels.
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

Both act at export, on the tiles, not on the overview file.

- **`--tile-buffer` (default 8, at most 256, tile pixels).** How far past its
  edge a tile carries geometry, so a feature spanning a seam renders
  continuously. The unit is the 256-pixel nominal tile, the same as
  tippecanoe's `--buffer` (default 5). The cap is one full tile width. At 256 a
  tile already holds every feature of its eight neighbors, and past it a tile
  would duplicate geometry from tiles it does not border. The cap also keeps
  export bounded. The buffer is what places a feature in more than one tile,
  so a huge value puts every feature in every tile and makes export
  O(features × tiles). tylertoo refuses a wider value before any convert or
  export work runs. A sharded build reads two pivot tiles (512 px) past its
  range, so this cap sits inside that margin. See
  [Scaling](guides/scaling.md#sharded-builds).
- **`extent` (default 4096, set with Python `export_pmtiles(extent=...)`,
  not a CLI flag).** The MVT tile-local coordinate resolution, which must be positive.
  At `0`, every coordinate quantizes to the tile's origin, so every line and
  polygon degenerates and drops, and consumers divide by zero on the layer.
  tylertoo refuses it. The MVT spec recommends a power of two, and decoders
  assume one when they reason about coordinate precision. tylertoo accepts
  any other positive value with a warning. `tylertoo decode` refuses an
  archive whose layer declares `extent: 0` for the same reason.

---

## Export and archive commands: `export-pmtiles`, `merge`, `stats`

**The per-tile size cap.** `export-pmtiles --tile-size-limit`, also spelled
`--max-tile-size` as on `tiles`, caps each tile's encoded MVT size. The
default of 500K matches tippecanoe, and `0` disables the cap. A tile over the
cap sheds features for that tile only, in one non-iterative pass. It drops
the largest first for polygons and lines, and keeps a uniform spatial stride
for point tiles.

**The simple-clip fast path.** By default, a polygon whose rings are already
simple skips the `i_overlay` boundary-bridge fallback when tylertoo clips it
to a tile. Fine-zoom polygon export runs faster and renders the same, but a
simple ring comes out rotated to a different start vertex. Pass
`--no-simple-clip-fastpath` when you need byte-stable tile output.

**Declaring a minimum zoom.** `export-pmtiles --min-zoom` sets the minimum zoom
the archive declares in its metadata (`vector_layers[].minzoom`), even when
the overview file lacks its coarsest levels. `overview` omits a level that
generalizes to nothing, so a file built for z0–13 can start at z2. This flag
records the requested z0 anyway. The PMTiles header's minimum zoom does not
widen: it always reports the shallowest zoom that holds a tile, as
`go-pmtiles verify` requires. A renderer that reads the header gets z2, and
the empty zooms would render nothing either way. The value must not be finer
than the coarsest level present. Unset, it is the coarsest level's zoom.
`tiles` and Python's `convert()` pass their own `--min-zoom` here.

**Export-time property selection.** `export-pmtiles --include-property`,
`--exclude-property`, and `--exclude-all-properties` act on the tiles only.
See [Property selection](#property-selection-include-property-exclude-property-exclude-all-properties).

**The export report's encode tallies.** `--report`, and the return value of
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

**Sharded export flags.** `--tile-range LO..HI` takes two tile ids at the same
pivot zoom and emits every descendant of those tiles at every deeper zoom. The
ids under one tile are contiguous on the Hilbert curve, so the restriction is
an exact interval test at each zoom. Tiles coarser than the pivot are not
emitted. `--zoom-ceiling Z` emits only the zooms at or below `Z`, the coarse
half. It is a ceiling, not a `--max-zoom`, because it chooses which zooms to
emit, while `--min-zoom` only widens what the metadata declares. The overview
file still holds every level. [Scaling](guides/scaling.md#sharded-builds)
covers both in context.

**`tylertoo merge`.** It takes two or more inputs that hold disjoint tile ids
and agree on tile type and tile compression. The merged archive's bounds,
zoom range, and `vector_layers` are the unions of the inputs'. Layers sharing
an id collapse into one entry spanning their combined zooms, with the union
of their fields. A shard's ids need not form a contiguous slice of the id
space, because subtrees at different depths interleave on the Hilbert curve.
tylertoo therefore checks disjointness per tile id, not per range.
`--work-dir` holds the spool file of merged tile data until merge assembles
the archive. Set it when the output is large and `/tmp` is a small tmpfs.
`--report` writes per-zoom tile counts, the figures to check a sharded build
against. To combine
archives that overlap, such as different data at different zooms or several
layers over the same zooms, use [`tylertoo pyramid`](#several-inputs-one-archive-tylertoo-pyramid).

**`tylertoo stats`.** For each zoom it prints the tile count and the total,
mean, p50, p99, and maximum tile size. `--largest N` adds the largest tiles by
z/x/y, and `--json` prints the same numbers machine-readably:

```bash
tylertoo stats buildings.pmtiles --largest 5
tylertoo stats buildings.pmtiles --json
```

Sizes are **stored** (compressed) bytes per addressed tile. `stats` reads them
from the directory entries alone and never reads or decompresses a tile. Run-length
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

These knobs never change *which* features or vertices survive: geometry and
attributes are byte-identical whatever their values. They control the
**physical Parquet layout**, which drives remote read cost through footer
size and bbox pruning.

### `--row-group-size` (default 10000): per-level row-group sizing

The value is a per-level **cap**, not a global row-group size:

- A level with at most `row-group-size` features becomes a **single** row
  group. A coarse band of a handful of features therefore becomes one broad
  row group, which a reader fetches whole anyway for the quick look.
- A larger level splits into `ceil(features / row-group-size)` row groups of
  **roughly uniform** size. Fine bands keep many small row groups, so their
  per-row-group bbox statistics prune tightly against a viewport.

Each level always ends on a row-group boundary, and no row group mixes two
levels (spec §4.2), whatever the knob says.

Smaller values give tighter bbox pruning, fetching fewer features for a small
viewport, but more row groups and a larger footer. Larger values do the
reverse. The default 10000 balances the two. String and geometry statistics
are off by default (below), so even hundreds of row groups keep the footer
small, and raising the value seldom helps. Lower it to serve tiny
viewports over a high-latency store with tighter pruning.

**The value is a request, not a guarantee.** A Parquet file holds at most
32,768 row groups, because the row-group ordinal is an `i16`. A low
`--row-group-size` on a planet-scale input can project past that. Before pass
2 opens the output file, the converter projects the total row-group count
from pass 1's per-level winner counts. It compares the projection against a
preflight ceiling of 32,000 groups, leaving headroom because the projection
is an upper bound taken before simplification. Over the ceiling, tylertoo
**scales the cap up** to the smallest clean value that fits and logs the old
and new values at `warn`. `--report` JSON records the cap used as
`effective_max_row_group_size`, and the summary prints it, so the change
stays visible after the run.

A raised cap costs memory. The writer holds a whole row group in RAM before
flushing it, so peak write memory scales with the cap. When memory matters
more than layout, cut the projected row-group count at the source. Plan fewer
levels with `--min-zoom` and `--max-zoom`, or use
`--row-group-size-policy zoom-scaled`, which gives coarse bands far larger
caps.

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

By default the writer **suppresses** Parquet per-row-group min/max statistics
on the well-known binary (WKB) geometry column and on every string or binary property column.
An Overture 26-character ULID `id` is a typical example. The overview read protocol never
uses those statistics. Spatial pruning uses the **bbox covering** struct, and
level selection uses the **`level`** column, and both *always* keep full
statistics (spec §4.4). On high-cardinality data, though, the suppressed
statistics dominate the Thrift footer, which every remote query reads in full
whatever its viewport. On the Moldova polygon set (631k features, ULID ids),
the footer is **8.84 MB** with full statistics, larger than most viewports'
data. With suppression it is under **1 MB**.

Pass `--full-column-stats` to keep statistics on all columns. Do this only if
remote clients push predicates on property columns, such as `WHERE id = …`
or `WHERE class = 'motorway'`, and want row-group skipping on them. You trade
a bigger footer for that pushdown.

---

## Memory / streaming knobs: `--no-streaming`, `--read-batch-size`

Like the [file layout knobs](#file-layout-knobs-row-group-size-full-column-stats),
these never change the output's content. They control how much memory the
conversion uses. [How streaming bounds memory](guides/scaling.md#how-streaming-bounds-memory)
in the Scaling guide explains the two-pass pipeline, its memory footprint,
and the pass-1 floor of 64 bytes per row.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--read-batch-size N` | `8192` | rows per read batch (max 1048576) | **bigger = slightly faster, more memory** |
| `--no-streaming` | off | flag | use the one-pass in-memory pipeline |

**`--read-batch-size`** bounds the transient working set of both passes:
tylertoo decodes, filters, simplifies, and writes each batch before reading
the next. The default keeps per-batch transients in the tens of MB even for
vertex-heavy polygons. Lower it, for example to 1024, on memory-constrained
machines or for huge geometries, since a batch of coastline-sized
multipolygons can be large. Raise it, for example to 65536, only if profiling shows
per-batch overhead dominating on a machine with RAM to spare. It also sets
how finely pass 1 fans out: each batch splits across the rayon pool into
chunks of `read_batch_size / threads` rows, clamped to 256 through 1024.
Lowering it for memory costs some pass-1 parallelism but never disables it.

**`--no-streaming`** holds the whole table and every decoded geometry at once,
`O(dataset)` memory. It decodes each geometry once, where streaming
re-decodes the winners in pass 2, so it can run slightly faster on small
inputs that fit in RAM. On large inputs it is slower and needs far more
memory. It stays as the equivalence-tested reference implementation and an
escape hatch. No output-quality reason favors it.

During pass 2, the winner table holds 1 byte per feature, about 0.6 MB for a
632k-feature file.

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

Pass degrees even for an EPSG:3857 input. The converter reprojects the box.
Without covering statistics, tylertoo reads every row group and applies only
the exact per-feature test, so the output is identical and only slower.
Compare `ConvertReport.row_groups_read` with `row_groups_total` to see
whether pruning fired.

---

## Attribute filter: `--filter` / `--where`

`--filter <EXPR>`, alias `--where`, converts only the features matching a
SQL `WHERE`-style predicate over the input's property columns. It is the
attribute analogue of `--bbox`, composes with it, and works on both
`overview` and `tiles`.

```bash
# Only high-confidence field boundaries, straight from the source file
tylertoo tiles fields.parquet fields.pmtiles \
  --filter "confidence > 0.8" --max-zoom 14

# Composes with --bbox and richer predicates
tylertoo overview brazil.parquet subset.parquet \
  --bbox=-48.0,-16.0,-47.0,-15.0 \
  --where "crop_type IN ('soy', 'corn') AND confidence >= 0.5"
```

**Expression language.** A small built-in recursive-descent parser reads the
expression, with no SQL engine behind it.

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

**Null semantics** follow SQL three-valued logic:

- A comparison or `IN` over a `NULL` is `UNKNOWN`.
- `AND`, `OR`, and `NOT` combine with Kleene logic.
- A row stays only when the whole predicate is `TRUE`.

So `confidence > 0.8` drops null-confidence rows, and so does
`NOT (confidence > 0.8)`. Use `IS NULL` and `IS NOT NULL` to test nulls
explicitly.

A NaN in a float column is UNKNOWN too: no comparison against it has an
answer, and it is far more often nodata than a value. Infinities are ordinary
values and compare normally. This makes `!=` and `NOT IN` drop NaN rows as
well. `NaN != 5` is `TRUE` under IEEE-754 but `UNKNOWN` here, which is the SQL
reading. Unlike a null, a NaN is a *present* value, so `IS NULL` does **not**
match it and `IS NOT NULL` does. Together, the two rules mean **no predicate
selects NaN rows**. Only a predicate over another column can keep them. Clean
the column upstream, for example with `gpio`, to address those rows.

Like `--bbox`, the filter checks row-group statistics before it reads data.
It then evaluates each row exactly in pass 1, so the output is identical
whether pruning fired or not. See
[Remote reads](guides/remote-reads.md#filters). `AND` intersects the prunable
sets and `OR` unions them. `NOT (...)` subtrees and columns without usable
statistics keep the row group.

**Interactions.** The filter runs before level assignment, ranking, density
budgets, clustering, and coalescing. Dropped features never enter the
pipeline, exactly as if you had pre-filtered the input.
`ConvertReport.input_features` counts only survivors. Compare
`row_groups_read` with `row_groups_total` to see whether pruning fired.
Sorting the input by a filtered column, or lowering `--row-group-size`,
tightens per-row-group statistics and prunes more.

---

## Property selection: `--include-property` / `--exclude-property` / `--exclude-all-properties`

These choose the attribute columns the output carries, like tippecanoe's
`-y`, `-x`, and `-X`. On `overview` and `tiles`, tylertoo drops everything
not selected **at scan time**. It never decodes the excluded columns, the
intermediate overview file carries only the kept ones, and the tiles'
per-feature bytes shrink. That matters for the `--max-tile-size` valve: a tile
sheds fewer features to fit its byte budget when it does not carry eight
columns nobody asked for. The geometry column always stays.

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

The three combine as tippecanoe's do. An include list is the whole answer,
and tylertoo then ignores the exclude flags. In tippecanoe, `-y` implies `-X`,
and the attribute filter consults only the include set once one exists. So
`--exclude-all-properties --include-property foo` keeps `foo`, and
`--include-property a --include-property b --exclude-property b` keeps both.
Without an include list, `--exclude-all-properties` keeps nothing and
`--exclude-property` drops what it names.

**Interactions.** A column another knob reads must stay included:
`--sort-key`, `--class-rank`, `--magnitude-ladder`, `--entry-zoom`,
`--accumulate-attribute`, and `--filter`. tylertoo rejects excluding it and
names the knob, because the knobs evaluate over the selected columns and
would otherwise go silently inert.

`export-pmtiles` takes the same three flags and applies them to the tiles
only, matched on the property names the tiles publish. The overview file
stays untouched. There, the `--feature-order` column must stay. `tiles`
rejects the same pairing up front: its selection applies at convert, so the
column would vanish before export could sort on it. An explicit
`--include-property coalesced_count` on export overrides the omission of an
all-1 column: the column is in the file, and naming it is not a typo. As in
tippecanoe, an include list overrides the exclusions. Naming a property the
file does not export is an error.

**Column types.** tylertoo exports struct, list, and map columns as JSON
strings, tippecanoe's convention for nested attributes. So
`--include-property names` on an Overture file keeps the whole `names` struct
as one string property. A column with no MVT encoding, such as a binary
column, drops with one warning per column and appears in the export report's
`skipped_property_columns`. tylertoo rejects it by name if `--include-property`
asks for it, and an `--exclude-property` naming it silences the warning. The
full type table is in [preparing input](tutorials/madagascar.md#1-prepare-the-input).

---

## Performance profiles: `--profile`, `--in-flight-batches`, `--read-workers`

Like the [memory and streaming knobs](#memory-streaming-knobs-no-streaming-read-batch-size),
these never change the output's content. **The output is byte-identical
across every profile, `--in-flight-batches` value, `--read-workers` value,
and thread count.** They control only speed and memory.

The Scaling guide covers [memory profiles](guides/scaling.md#memory-profiles),
[read concurrency](guides/scaling.md#read-concurrency), and
[spill files](guides/scaling.md#spill-files) in depth, including container
memory limits and `--spill-dir`.

Pass 2 reads the input Parquet **once** and pipelines read and decode with
per-feature simplification across **all cores**. The profile decides where
each output level's rows wait between compute and write.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--profile speed\|bounded\|auto` | `auto` | preset | `speed`: least wall time, most RAM. `bounded`: capped RAM, temp I/O |
| `--in-flight-batches N\|auto` | `auto` | read batches in flight (auto = cores, clamped to 4–16) | **bigger = more overlap and core use, more memory** |
| `--read-workers N\|auto` | `auto` | pass-2 reader threads (auto = cores/4, at most 4) | **more = more read throughput, more resident batches** |

- **`speed`** buffers each level's rows in RAM. It runs in the least wall
  time, with no temp I/O, but peak RAM grows with total *output* size.
- **`bounded`** spills each level's rows to temporary Arrow IPC files and
  streams them back at write time, capping peak RAM whatever the output size.
- **`auto`** spills when the estimated buffered output exceeds 0.6 of
  available RAM and keeps rows in RAM otherwise.

**How `auto` estimates.** The estimate is `buffered rows × per-row cost`. The
per-row cost comes from pass 1's measured average encoded-geometry size for
this input, because geometry dominates a buffered row and varies about 400×
across datasets. The estimate leans high on purpose, toward the near-free
spill path. When pass 1 scanned nothing, calibrated constants apply: about
8 KiB per row in duplicating mode and 16 KiB in partitioning mode.
Partitioning also always spills above 2M buffered rows. tylertoo logs the
decision with its measured average, estimate, and budget.
`TYLERTOO_AUTO_MEM_LIMIT_BYTES` overrides the detected available RAM.

**Pass-1 winner grids.** The profile also governs pass 1's level assignment,
which builds one cell-winner grid per coarse level. On large simple-geometry
layers, the grids live at the same time and set the convert's RSS peak:
5.9 GiB on germany-segments, and about 24 GiB at Brazil scale (see the
`[rss]` phase logs). Under `bounded` and `auto`, tylertoo estimates the
grids' footprints up front against the same budget. When the estimate
exceeds the budget, it builds the levels in **memory-budgeted waves**, so only
one wave's grids are live at a time. In the limit this degrades to one level
per wave instead of running out of memory. Each level's grid still builds
across every core, so a one-level wave is not a serial build. That
parallelism adds one transient the estimate does not count. It takes about
2 MiB per thread across a wave, at least 1 MiB per level, and frees after every
reduce. A one-line `[assign] winner grids …` log reports any split. On a
roomy machine the plan is one wave and nothing changes. `speed` opts out,
with unbounded grids and full parallelism.

**`--in-flight-batches`** sets how many Arrow read batches move through each
pass at once, the bounded-channel depth. Raise it for more read/compute
overlap and better core use when a few long-pole geometries stall the
pipeline. Each extra batch costs `read_batch_size` more resident rows per
pass. The passes never overlap, so the cost does not double.
`--read-batch-size` sets the rows per batch, and `--in-flight-batches` sets
how many batches coexist. Pass 2's readers add `--read-workers` × their queue
depth on top. Under `bounded`, each level's spill writer adds up to three
more batches: two queued and one in the encoder.

**`--read-workers`** splits the pass-2 read across threads. Parquet row
groups read independently, so several threads decode disjoint runs of them
at once. An in-order merge then reassembles the exact batch sequence a single
reader would produce, so the output is byte-identical for every value.
`crates/cli/tests/thread_count_determinism.rs` checks `1` against `2` and
`4`. tylertoo honors an explicit value up to **2× the machine's cores**, at
least 4. Above that the CLI rejects it, and the library clamps it with a
warning, because each worker is a thread plus its own read-ahead queue. A
worker buffers its run ahead of the merge. A row group larger than its queue
makes the worker wait mid-run, and the reads serialize again. A
`[convert] pass 2 read:` debug line reports it. A single-row-group file
always reads sequentially.

⚠️ **`speed` with partitioning on a multi-GB input risks running out of
memory.** `speed` buffers whole output levels in RAM, and partitioning's
output can approach the input's size. `auto` already sends partitioning and
any over-budget run to `bounded`. Only an explicit `--profile speed`
overrides that. If you force `speed` on a large partitioning run, watch peak
RSS.

---

## Reusing a plan: `--save-plan`, `--plan`

Pass 1 streams the whole input to build the *winner table*: the level each
row enters at. The level assignment and density budget run over it.
Pass 2 writes. On a large input, pass 1 and the assignment take most of the
wall time, and they depend on nothing the write side does.

`--save-plan PATH` persists that result, and `--plan PATH` replays it instead
of recomputing it. Both work on `overview` and `tiles`, need the streaming
pipeline, and exclude each other. Neither is in the Python bindings.

```bash
# Once: scan, assign, and keep the plan.
tylertoo overview roads.parquet roads.parquet.overview \
  --min-zoom 0 --max-zoom 14 --save-plan roads.plan

# Again, tuning only the write side; pass 1 and the assignment are skipped.
tylertoo overview roads.parquet roads-tuned.overview \
  --min-zoom 0 --max-zoom 14 --plan roads.plan \
  --profile bounded --row-group-size 50000
```

The plan is a header with magic bytes and a checksum, followed by one Arrow
IPC file. Its size follows input **rows**, not bytes: one winner byte per row
plus small per-level side tables. A 28 MB, 24k-feature polygon file yields a
49 KB plan. Line coalescing is the exception: the plan carries the collected
line geometries as WKB, so a line-heavy input produces a proportionally
larger plan.

**The file is checksummed.** Plans travel between machines, so `--plan`
verifies an xxh3-64 hash over the whole payload before decoding any of it. A
truncated, edited, or corrupted plan gives one named error instead of an
Arrow panic:

```
--plan /mnt/shared/roads.plan: is corrupt: the payload hashes to
1f3c...  but the header records 90ab.... The file was truncated, edited, or
damaged in transit — re-create it with --save-plan.
```

Every other structural problem is likewise an error naming `--plan` and the
path:

- a file that is not a plan
- a foreign format version
- an impossible level count or cluster stride
- an unknown geometry-kind code
- row-indexed sections of disagreeing length

The checksum proves the file is the one written, not that it fits this run.
tylertoo therefore also compares the plan's row domain against the rows this
run reads, the sum over the row groups `--bbox` and `--filter` selected,
every time.

**tylertoo verifies the plan instead of trusting it.** It stores a fingerprint: the tylertoo
version, every flag that affects thinning, and each input's identity. A
mismatch is an error naming the field:

```
--plan: saved plan does not match this run: input "roads.parquet" mtime was
"1790242802390408452" when the plan was saved but is "1790244114398193000"
now. Re-run without --plan (add --save-plan to write a fresh one).
```

**What input identity pins.** For every part, local file or remote object,
the fingerprint holds:

- the path or URL
- the byte size
- the **row count** and **row-group count** from the Parquet footer
- the row groups `--bbox` and `--filter` pruned to

A **local** part also pins its mtime.

| | local file | remote object |
|---|---|---|
| path / URL | ✅ | ✅ |
| byte size | ✅ (`stat`) | ✅ (Content-Length) |
| row count | ✅ (footer) | ✅ (footer) |
| row-group count + pruned selection | ✅ | ✅ |
| mtime | ✅ | ❌ |
| content hash / ETag | ❌ | ❌ |

The row count carries the most weight. The winner table holds one byte per
input row, addressed by row *position*. An input swapped under a saved plan
either produces a silently wrong pyramid (fewer rows) or indexes out of
bounds (more rows). Pinning the footer row count turns both into a named
error, for remote and local inputs alike. tylertoo prints a one-line warning
naming what is and is not pinned whenever a part is remote.

Neither a local mtime and size nor a remote size and row count is a content
hash. `cp -p` and `rsync -a` preserve mtime and size, and an object can be
rewritten in place with the same size and row count. Treat the fingerprint as
a strong staleness check, not a cryptographic seal.

The fingerprint deliberately leaves out these flags, so one plan replays
across them: `--profile`, `--row-group-size`, `--row-group-size-policy`,
`--full-column-stats`, `--read-batch-size`, `--in-flight-batches`,
`--spill-dir`, and `--cogp-compat`. It covers everything that changes *which*
features land at *which* level, and output with `--plan` is byte-identical
to the run that saved it.

⚠️ **Sharded builds need one shared plan.** The assignment is not a
per-feature function:

- The density budget water-fills a 128 × GSD super-cell budget over *every*
  candidate of a level.
- The level walk carries a running kept count from coarse to fine.
- `--magnitude-ladder` dense-ranks the *global* distinct values of its column.
- The automatic class ranking picks its column from a global vocabulary scan.

A shard that recomputed the assignment over its own subset would reach a
different answer, and the shards' pyramids would disagree. `tiles --shard`
therefore refuses to run without `--plan`. A shard also *narrows* the plan's
row-group selection, reading only the groups whose bbox reaches its tile
range. The fingerprint compares that term as a subset relation, and tylertoo
re-addresses the plan's row-indexed tables onto the shard's shorter row
stream before pass 2. See [Scaling](guides/scaling.md#sharded-builds) for the
whole recipe.

**tylertoo checks both paths before it scans anything.** It writes
`--save-plan` only after pass 1 *and* the assignment finish. Its parent
directory must therefore exist and be writable at option validation, so an
unmounted volume fails at once instead of after the whole scan. An existing
file at that path must itself be writable, and tylertoo overwrites it and
logs a line. `--plan` must be a readable convert plan, with its magic bytes checked
and a future format version named as such.

**Overwrites and partial outputs.** Every subcommand that writes a file
(`overview`, `tiles`, `export-pmtiles`, `decode`, `pyramid`, `merge`, and
`shard-plan`) refuses an existing output unless given `-f`/`--force`. The
GeoParquet writers, `overview` and `decode`, build their output in a uniquely
named sibling (`OUTPUT.<random>.partial`) and rename it over `OUTPUT` only
after writing the footer. A run killed part-way therefore leaves a previous
output intact. A failed run removes the sibling. After a killed run, delete
it by hand.

---

## Worked scenarios

| Symptom | Fix |
|---------|-----|
| Coarse levels look too sparse (too few features) | Lower the thinning factors (`--point/line/polygon-thinning`) or the visibility gates (`--line/polygon-visibility`), or raise `--gsd-base` |
| Coarse roads look like sparse disconnected dashes | Lower `--line-thinning` or `--line-visibility`, keep [line coalescing](#line-coalescing-no-coalesce-lines-coalesce-junction-angle-coalesce-snap-coalesce-max-level-rows) on, or raise `--gsd-base` |
| Coarse features are all there but jagged or over-smoothed | Lower `--simplify-factor` (for example, 1.0 → 0.5) |
| Wrong roads survive (residential instead of highways) at coarse zoom | Add `--class-rank road_class:…` or rely on automatic detection (don't pass `--no-auto-rank`) |
| Coarse levels are too large or slow | Raise the thinning factors or `--simplify-factor`, or lower `--gsd-base` |
| Small buildings vanish too early | Lower `--polygon-visibility`, or pass `--collapse` to keep them as points |
| Country-scale view of a dense building or parcel layer is empty | `--polygon-visibility 0 --collapse` plus a circle layer for points ([dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)), or `--collapse-square` to keep polygons |
| Whole map uniformly too sparse or too dense | Move `--gsd-base` (up = denser, down = sparser) instead of tuning each family |
| Mid zooms (about z9–z12) have far more features than tippecanoe, or duplicating files are too large | Raise `--drop-rate` (density budget), or pass `--no-density-drop` to turn it off |
| Density cut strips sparse rural areas to keep cities | Raise `--drop-gamma` (sparse-area protection) |
| Dense point data renders as a misleadingly sparse dot field at coarse zooms | Pass `--cluster`, and style the symbol size by `point_count` |
| Need per-cluster totals or averages of a numeric column | `--accumulate-attribute col:sum` or `col:mean` (with `--cluster`) |
| Cell aggregates or pre-built levels lose features at coarse zooms | `--verbatim` |
| Every remote query fetches a huge footer before any data | The default already suppresses string and geometry statistics, so do not pass `--full-column-stats` |
| Need server-side row-group skipping on a property predicate | Pass `--full-column-stats` (bigger footer, gains column pruning) |
| Tiny viewports over high-latency storage fetch too much | Lower `--row-group-size` for tighter bbox pruning |
| Conversion runs out of memory or swaps on a big file | Lower `--read-batch-size`, keep streaming on (no `--no-streaming`), and see [Scaling](guides/scaling.md) |
| Conversion leaves cores idle | Raise `--in-flight-batches` for more read/compute overlap |
| Conversion runs out of memory under `--profile speed` | `--profile bounded`, which spills each level to temp files. `auto`, the default, picks it when the estimate exceeds the budget |
| `memory allocation of N bytes failed` on a v0.7.1-or-earlier musl binary, with RAM to spare | `--profile bounded`, or upgrade: later musl binaries ship mimalloc and avoid the musl `mallocng` fragmentation ([#480](https://github.com/geoparquet-io/tylertoo/issues/480)) |

[`corpus/SWEEPS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)
holds the corpus sweeps behind the defaults, including `--line-thinning` ×
`--simplify-factor` on Portland roads and the `--drop-rate` calibration
against tippecanoe.
