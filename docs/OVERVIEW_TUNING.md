# Overview Generalization Tuning

`gpq-tiles overview` turns a GeoParquet file into a multi-resolution
[overview file](architecture.md) — several precomputed generalizations of the
dataset at increasing detail (levels `0` = coarsest … `L-1` = finest /
canonical). Two families of knobs control how much detail each coarse level
sheds:

- **thinning / visibility** — which whole *features* survive at a level
  (feature dropping);
- **simplification** — how many *vertices* each surviving feature keeps
  (geometry smoothing).

Everything is expressed relative to each level's **GSD** (ground sample
distance, meters — the smallest ground distance independently meaningful at that
level), and the GSD ladder itself is set by `--gsd-base` (or `--gsd`). This page
explains every knob in plain English: what it does, its default, its units, the
direction that makes maps denser/sparser or smoother/cruder, and how the knobs
interact.

> TL;DR cheat sheet
>
> - **Coarse levels look too *sparse* (too few features)** → LOWER the thinning
>   factors (`--point/line/polygon-thinning`) and/or the visibility gates
>   (`--line/polygon-visibility`); or RAISE `--gsd-base`.
> - **Coarse levels look too *crude / blocky* (features present but jagged or
>   over-smoothed)** → LOWER `--simplify-factor`.
> - **Coarse levels are too *heavy / dense / large*** → RAISE the thinning
>   factors and/or `--simplify-factor`; or LOWER `--gsd-base`.
> - **Mid zooms (≈z9–z12) over-retained (way more features than tippecanoe,
>   large duplicating files)** → RAISE `--drop-rate` (the density budget); to
>   disable it entirely use `--no-density-drop`.
> - **A rank-ordered cut is emptying sparse rural areas** → RAISE `--drop-gamma`
>   (protects sparse neighborhoods).

---

## The GSD ladder: `--gsd-base` and `--gsd`

A zoom-range plan (`--min-zoom` / `--max-zoom`, the default) derives each
level's GSD from its Web Mercator zoom:

```
gsd(z) = 40075016.69 / gsd_base / 2^z          (meters, spec §5.2)
```

| Knob | Default | Units | Effect |
|------|---------|-------|--------|
| `--gsd-base F` | `1024.0` | dimensionless | tile-band base in the formula above |
| `--gsd G1,G2,…` | — | meters, strictly decreasing | explicit per-level GSDs; **overrides** the zoom range and `--gsd-base` |
| `--min-zoom` / `--max-zoom` | `0` / `6` | Web Mercator zoom | coarsest / finest (canonical) level |

`--gsd-base` is the **master detail knob** for a zoom-range plan. It scales the
*whole ladder* at once:

- **LARGER `--gsd-base` ⇒ SMALLER GSDs at every zoom ⇒ denser, more detailed,
  larger coarse levels** (less is thinned/simplified for a given zoom).
- **SMALLER `--gsd-base` ⇒ LARGER GSDs ⇒ sparser, cruder, cheaper coarse
  levels.**

The default `1024` is the cogp-rs convention (~4× a 256 px tile, so sub-pixel
features drop; spec §5.2 / Q6). `--gsd-base` has **no effect** when `--gsd` is
given, because those GSDs are already absolute meters.

Because every other knob below is measured *in multiples of the level GSD*,
changing `--gsd-base` moves all of them together. Reach for a per-family knob
when you want to rebalance *one* geometry class; reach for `--gsd-base` when the
whole map is uniformly too sparse or too dense.

For non-default `--gsd-base` runs the chosen base is recorded in the footer
`geo:overviews` → `generalization.gsd_base` provenance (a default run omits it —
the footer `levels[].gsd` already imply `1024`, so default output is unchanged).

---

## Feature thinning: `--point/line/polygon-thinning`

Thinning keeps **one winning feature per grid cell** per level (the winner is
chosen by the ranking — see [Ranking](#ranking-which-feature-wins-a-cell)). The
grid cell size for a kind is:

```
cell_size = thinning_factor * gsd(level)          (in the CRS's units)
```

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--point-thinning` | `4.0` (`16.0` with `--cluster`) | × GSD (cell size) | **bigger = sparser** |
| `--line-thinning` | `1.0` | × GSD (cell size) | **bigger = sparser** |
| `--polygon-thinning` | `1.0` | × GSD (cell size) | **bigger = sparser** |

Bigger factor ⇒ bigger cells ⇒ fewer cells ⇒ fewer survivors ⇒ **sparser**
map. Smaller ⇒ **denser**.

⚠️ **Interaction warning — the direction is counter-intuitive.** A *bigger*
thinning number makes the map *sparser*, because it multiplies the cell size.
This is the opposite of "turn it up to get more." If your coarse roads look too
empty, **lower** `--line-thinning`, don't raise it.

The defaults are class-aware: points thin hardest (4.0, they clutter fastest),
lines and polygons least (1.0). The line default was retuned 2.0 → 1.0 after
the 2026-07-02 Portland roads sweep (`corpus/SWEEP_NOTES.md`): at 1.0, road
networks stay visibly more continuous at coarse zooms, and the true-scale
renders showed the extra density costs little legibility. The factor multiplies the GSD, so it stacks with `--gsd-base`: doubling
`--gsd-base` halves the GSD, which halves the cell size for the same factor.

---

## Visibility gates: `--line/polygon-visibility`

A line or polygon is **eligible** at a level only if its bounding-box diagonal
clears the gate:

```
eligible  ⇔  bbox_diagonal >= visibility_factor * gsd(level)
```

Features below the gate are **dropped outright** at that level (they reappear at
finer levels once the GSD shrinks below their size). Points are never gated.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--line-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |
| `--polygon-visibility` | `4.0` | × GSD (min bbox diagonal) | **bigger = sparser** |

Bigger factor ⇒ higher bar ⇒ more small features dropped at coarse levels ⇒
**sparser**. Smaller ⇒ more small features kept ⇒ **denser**.

⚠️ **Interaction warning — the gate is multiplied by the GSD.** So it moves with
`--gsd-base` and with zoom. A polygon visible at one level can be gated out one
level coarser purely because the GSD (and therefore the gate) doubled. This is a
*hard drop*, distinct from thinning's *one-per-cell* competition: a feature can
be gated out even if its cell is otherwise empty.

---

## Per-feature simplification: `--simplify-factor`

Simplification runs Ramer–Douglas–Peucker on each *surviving* feature's geometry
with a world-space tolerance:

```
tolerance = simplify_factor * gsd(level)          (meters, then CRS-converted)
```

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--simplify-factor` | `1.0` | × GSD (RDP tolerance) | **bigger = cruder + lighter** |
| `--collapse` | off | flag | below-gate polygons become a representative point instead of dropping |

- **LOWER `--simplify-factor` = smoother / less aggressive** = more vertices
  kept = crisper but heavier coarse levels.
- **HIGHER = cruder / blockier** = fewer vertices = lighter levels.

Duplicating mode only; the **canonical (finest) level is always verbatim**
regardless of this factor (spec §2.4). `--simplify-factor 0` disables
simplification entirely (identity).

⚠️ **Interaction warning — high factors also thin.** A line/polygon whose bbox
diagonal falls below the tolerance is *dropped*, not just smoothed. So a very
high `--simplify-factor` removes features in addition to shedding vertices — it
is not a pure "smoothing" knob at the extremes. If you only want fewer vertices,
keep the factor modest and adjust density with the thinning/visibility knobs.

---

## Ranking: which feature wins a cell

When several features compete for one grid cell, the **winner** is the
highest-priority feature. Priority tiers (spec §3.5, Q1), highest first:

1. `--sort-key COL` — a numeric column (e.g. population, importance).
2. `--class-rank COL:VAL=RANK,…` — an explicit categorical map (e.g.
   `road_class:motorway=5,primary=4,residential=2`). Unlisted values rank below
   every listed one but above nulls.
3. **Auto-detect** (unless `--no-auto-rank`): Overture roads (`class`/
   `road_class`) get a built-in motorway→…→service ranking; Overture places get
   `confidence`.
4. **Size fallback**: larger bbox diagonal wins, ties broken by a deterministic
   hash (fair/random, tippecanoe-like).

Ranking does not change *how many* features survive (that's thinning), only
*which* ones. For road networks, a good ranking is what keeps highways visible
at coarse zooms instead of a random scatter of residential streets. The tier
used is recorded in the footer `generalization.ranking` provenance.

`--sort-key` and `--class-rank` are mutually exclusive.

---

## Density budget: `--drop-rate`, `--drop-gamma`, `--no-density-drop`

Cell-winner thinning (above) stops binding once its grid cell is smaller than
the typical feature spacing: from roughly z9 up, *every* feature wins its own
cell, so per-level counts plateau at ~the whole dataset. On Portland roads that
plateau is 2–3× tippecanoe's feature count at z9–z11 (see
`corpus/SWEEP_NOTES.md`) — visual clutter, and the main driver of duplicating
mode's storage overhead.

The **density budget** fixes this the way tippecanoe does: after cell-winner
thinning, each level is capped at a feature **budget** that decays geometrically
toward coarse zooms, and the lowest-priority survivors (same
[ranking](#ranking-which-feature-wins-a-cell) order) are dropped until the level
meets its budget.

```
budget(level) = N / drop_rate ^ (finest_level − level)      (N = input features)
keep(level)   = min(cell_winner_survivors(level), budget(level))
```

The finest (canonical) level keeps everything (never dropped, spec §2.4). The
cut is a *ceiling*: a level already sparser than its budget — every coarse zoom,
where cell-winner did the thinning — is untouched, so the budget only bites the
mid-zoom plateau.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--drop-rate F` | `1.65` | ratio (>1) | **bigger = sparser mid zooms** |
| `--drop-gamma F` | `1.5` | exponent (≥1) | **bigger = more sparse-area protection** |
| `--no-density-drop` | off | flag | disables the budget (pre-Q2 behavior) |

**`--drop-rate`** is the strength knob. Each coarser level keeps `1/rate` of the
next finer one. BIGGER ⇒ coarse levels shed harder (sparser mid zooms, smaller
files); SMALLER ⇒ gentler. The default `1.65` was calibrated on Portland roads
(`corpus/SWEEP_NOTES.md`): it brings z9 to 1.21× and z10 to 1.03× tippecanoe
(z11 lands at 0.67×, a storage win) and leaves z8 and the coarse zooms
essentially at their cell-winner counts.

> **Why 1.65, not tippecanoe's 2.5?** Our budget anchors on the *full canonical
> count* `N` (every feature appears at the finest level), whereas tippecanoe's
> `-rate` is relative to a per-tile basezoom count. So an equivalent per-level
> thinning lands at a smaller numeric rate. `2.5` here over-thins hard (Portland
> z9–z13 all drop below tippecanoe).

**Spatial fairness (`--drop-gamma`).** A global rank-ordered cut would empty
sparse rural areas to keep dense cities under budget. Instead the per-level
budget is shared across coarse **super-cells** (`128 × GSD` neighborhoods): each
super-cell keeps its top-priority features up to an allocation
`∝ population^(1/gamma)`, water-filled so no cell gets more than it has.
`gamma = 1` is a proportional cut (every neighborhood keeps the same fraction);
`gamma > 1` is **sublinear** — dense neighborhoods keep proportionally fewer,
sparse ones proportionally more (protected). This is exactly tippecanoe's
`-g`/gamma dot-dropping ("reduce dots to the `1/gamma` power in dense areas")
applied per super-cell. `--drop-gamma` does **not** change per-level totals (it
only redistributes *which* features survive spatially), so it is independent of
`--drop-rate`.

⚠️ **Interaction — points may not feel the budget.** The budget applies to
points, lines and polygons alike, but points are already thinned hard by
`--point-thinning` (default 4). On a large point dataset (e.g. NYC POIs) the
cell-winner point counts often sit *below* the N-anchored budget at every zoom,
so `--drop-rate` rarely binds; point over-retention is better addressed with
point clustering (spec Q4). Lines and polygons (thinning factor 1) are where the
budget does the most work.

**`--no-density-drop`** turns the budget off entirely, reverting to pure
cell-winner thinning (the pre-Q2 behavior) and a byte-identical footer.

Only the mid-zoom plateau is affected. The chosen drop mechanism + parameters
are recorded in the footer `geo:overviews` → `generalization.density_drop`
provenance (`drop_rate`, `gamma`, `supercell_gsd_factor`); a disabled run omits
the block.

---

## Clustering: `--cluster`, `--accumulate-attribute`

By default a point that loses its thinning-grid cell simply does not appear at
that level — the survivor says nothing about how many features it stands for.
**`--cluster`** (opt-in, duplicating mode only) makes the survivor **absorb**
its cell's losers instead:

- Every output row gains a **`point_count`** INT64 NOT NULL column — the
  number of source features the row represents at its level (the tippecanoe /
  supercluster convention). At the canonical (finest) level every value is 1;
  lines and polygons always carry 1 (clustering only applies to points).
- The winner keeps its **own geometry and attribute values** — clusters are
  anchored on a real feature, not moved to a centroid (a deliberate divergence
  from supercluster's re-centering: deterministic, and the anchor stays a real
  place).
- Absorption is **per level**: a point absorbed at z4 may itself be a winner
  at z6 with its own (smaller) cluster. At each level,
  `sum(point_count) == total source point count` — the clusters partition the
  dataset at every level's grid.

Use it for graduated-dot rendering of dense point data (POIs, addresses): the
client scales the symbol by `point_count` instead of drawing a misleadingly
sparse constant-size dot field.

**Clustering changes the `--point-thinning` default from 4.0 to 16.0.** With
thin-only output a coarse grid *discards* data, so the default stays dense;
with clustering the losers are summarized into `point_count`, so a sparser
grid is pure win and gives the familiar graduated-cluster look (supercluster's
default radius is ~40 px; 16 × GSD ≈ one dot per ~16 display pixels). Chosen
from the NYC pt={4,16,48} sweep — each 4× step in the factor shifts the whole
density ladder two zooms. Pass `--point-thinning` explicitly to override in
either mode.

**`--accumulate-attribute COL:OP`** (repeatable; requires `--cluster`)
aggregates a numeric column across each cluster: the winner's value of `COL`
becomes the `OP` over itself + everything it absorbed at that level. Ops:
`sum`, `max`, `min`, `mean`. Examples:

```bash
gpq-tiles overview places.parquet places_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --cluster \
  --accumulate-attribute population:sum \
  --accumulate-attribute confidence:mean
```

Notes:

- Aggregates are computed **per level from source values** (never from
  already-aggregated coarser values), so `mean` is exact at every level.
- Null values don't contribute; a cluster whose members are all null keeps
  the winner's null. Non-accumulated columns keep the winner's own values.
- Aggregation is computed in `f64` and written back in the column's original
  type; a `mean` over an integer column rounds to the nearest integer
  (prefer float columns for `mean`).
- The column must exist and be numeric — the conversion fails early otherwise.
- Interaction with the density budget: a budget-deferred cell winner leaves
  its cell without a representative; those features attach to the **nearest
  surviving point** at that level, so counts still sum correctly.
- **Partitioning mode is rejected.** A partitioning row is read at many zooms
  (prefix reads) but exists at exactly one level, so a single stored
  `point_count` cannot reflect every zoom's grid — and absorbed features
  reappear as their own rows at finer levels while remaining counted in
  coarser winners, double-counting every prefix sum.

Clustering is recorded in the footer `geo:overviews` →
`generalization.clustering` provenance (`enabled`, `point_count_column`,
`accumulated: [{column, op}]`); `gpq-tiles validate` checks that the column
exists as INT64 NOT NULL and that canonical-level values are all 1.

---

## Line coalescing: `--no-coalesce-lines`, `--coalesce-junction-angle`, `--coalesce-snap`, `--coalesce-max-level-rows`

At coarse levels a line network (roads, rivers) can degrade into scattered
dashes: a segment whose bbox diagonal is below the visibility gate
(`--line-visibility × gsd`) is dropped outright, and cell-winner thinning
keeps disconnected fragments of what remains. Selection is smart;
*continuity* is destroyed.

Line coalescing is therefore **ON by default** (maintainer render review
2026-07-03 — like `--line-thinning 1.0` and the clustered point grid,
defaults should look right; pass **`--no-coalesce-lines`** to opt out). It
chains touching compatible segments into single "stroke" LineStrings at
each non-canonical duplicating level, **before** the gate and thinning
run:

- A chain of individually sub-visibility segments survives as **one long
  visible artery** — the gate evaluates the chain's extent, not each
  fragment's. This ordering is the entire payoff.
- Chains never merge **across class values** (when a class ranking is
  active — explicit `--class-rank` or auto-detected Overture
  `class`/`road_class`). With no class ranking, all lines are compatible.
- At **junctions** (3+ compatible endpoints meeting), chains continue only
  through the pair of lines that best continue each other within
  `--coalesce-junction-angle` of straight (see below); everything else
  terminates there, preserving network topology.
- The merged feature keeps the **attributes of its highest-priority
  member** (same class-rank → size → hash order as the cell-winner stage)
  and the output gains a **`coalesced_count`** INT32 NOT NULL column
  (source segments merged per row; 1 for unmerged rows and everywhere at
  the canonical level, which is never coalesced).
- Points and polygons are untouched. MultiLineString rows pass through
  unmerged.

```bash
# Coalescing is on by default (auto class ranking groups by road class):
gpq-tiles overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14

# Opt out (pre-Q3 behavior, no coalesced_count column):
gpq-tiles overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14 --no-coalesce-lines
```

### `--coalesce-junction-angle` (default 30, degrees)

Chains would otherwise break at every same-class crossing — on a road
network that is almost every block, so strokes stay short exactly where
the maintainer wants long arteries. At each junction the best-continuing
pair of lines merges when its deviation from a straight continuation is
at most this angle, best pair first (a 4-way crossing continues **both**
through-streets). BIGGER = chains bend further through junctions (fewer,
longer strokes; a genuine turn may get absorbed); `0` = never merge
through junctions (strict degree-2 chaining, the original Q3 behavior).
The Portland sweep for picking the default lives at
`corpus/data/bench/q3/portland-roads-junction{00,30,60}.pmtiles`.

### `--coalesce-snap` (default 1.0, GSD multiples)

Exactly-touching endpoints always chain (Overture/OSM segments share exact
node coordinates). The snap pass additionally joins chain ends within
`factor × gsd` of each other — two endpoints closer than one ground sample
are indistinguishable at that level. BIGGER bridges larger digitization
gaps but risks fusing the ends of nearby parallel lines; `0` disables the
snap pass (exact matches only).

### `--coalesce-max-level-rows` (default 2,000,000): memory guard

Chaining needs a level's candidate line geometries in memory at once, and
the candidate set at every non-canonical level is **all** lines (dropped
fragments must be reclaimable, so no winner-table pre-filter applies).
Datasets with more lines than this ceiling skip coalescing with a warning
— the file still carries the `coalesced_count` column (all 1) and the
provenance block, so the schema is stable. Levels that large are
near-canonical anyway, where segments are individually visible and
coalescing matters least. This is the streaming pipeline's one deliberate
`O(lines)` residual allocation.

### Interactions

- **`--line-visibility` / `--line-thinning`** now act on *chains*: the
  gate tests the merged extent, and one **chain** (not one segment)
  survives per thinning cell. Expect coarse levels to show **fewer rows
  but much more retained line length** than a non-coalesced run.
- **Class ranking** does double duty: it both prioritizes which chain wins
  a cell and defines the compatibility groups. `--no-auto-rank` (or a
  numeric `--sort-key`) makes all lines compatible — fine for
  single-class datasets (rivers), usually wrong for mixed road networks.
- **Density budget (Q2)** applies to **chains**: after gate + thinning,
  each level keeps at most `num_lines / drop_rate^(finest − level)` chains
  (same geometric ladder, floor, and spatial-fairness gamma as the
  point/polygon budget), cutting the lowest-priority chains first. Without
  this the reclaimed fragments would re-inflate exactly the mid-zoom
  counts the budget was calibrated to cap. `--no-density-drop` disables
  the chain budget too. Points and polygons keep the row-level budget as
  usual.
- **Partitioning mode: inert.** Partitioning places each feature exactly
  once with geometry verbatim; a merged chain is a new geometry replacing
  several source rows, which that contract cannot represent. Since
  coalescing is on by default, partitioning conversions simply proceed
  without it (no `coalesced_count` column, no provenance, info log); an
  explicit `--coalesce-lines` with `--mode partitioning` is rejected.

Coalescing is recorded in the footer `geo:overviews` →
`generalization.coalescing` provenance (`enabled`,
`snap_tolerance_gsd_factor`, `coalesced_count_column`); `gpq-tiles
validate` checks that the column exists as INT32 NOT NULL, all values are
>= 1, and canonical-level values are all 1.

---

## File layout knobs: `--row-group-size`, `--full-column-stats`

These do not change *which* features or vertices survive — geometry and
attributes are byte-identical regardless — but they control the **physical
Parquet layout**, which drives remote read cost (footer size + bbox pruning).

### `--row-group-size` (default 10000): per-level row-group sizing

The value is a per-level **cap**, not a global row-group size:

- A level whose feature count is `<= row-group-size` is written as a **single**
  row group. Coarse bands (a handful of features) therefore become one broad
  row group — which is what a reader fetches whole anyway (the coarse overview
  is the "quick look").
- A larger level is split into `ceil(features / row-group-size)` row groups of
  **roughly uniform** size. Fine bands keep many small row groups, so their
  per-row-group bbox statistics prune tightly against a viewport.

Each level always ends exactly on a row-group boundary and no row group ever
mixes two levels (spec §4.2) — this is invariant and independent of the knob.

Smaller values → tighter bbox pruning (fetch fewer features for a small
viewport) but more row groups → a larger footer. Larger values → the reverse.
The default 10000 balances the two; because string/geometry stats are
suppressed by default (below), even hundreds of row groups keep the footer
small, so there is rarely a reason to raise it. LOWER it if you serve tiny
viewports over a high-latency store and want tighter pruning.

### `--full-column-stats` (default off): statistics suppression

By default the writer **suppresses** Parquet per-row-group min/max statistics
on the WKB geometry column and on every string/binary property column (e.g. an
Overture 26-character ULID `id`). These stats are never used by the overview
read protocol — spatial pruning uses the **bbox covering** struct, and level
selection uses the **`level`** column, both of which *always* keep full stats
(spec §4.4). But on high-cardinality data they dominate the Thrift footer,
which is read in full on *every* remote query regardless of viewport. On the
Moldova polygon set (631k features, ULID ids) the footer was **8.84 MB** with
full stats — larger than most viewports' actual data — and drops below **1 MB**
with suppression.

Pass `--full-column-stats` to keep stats on all columns. Do this only if remote
clients push predicates on property columns (e.g. `WHERE id = …` or
`WHERE class = 'motorway'` server-side) and want row-group skipping on them —
you trade a bigger footer for that pushdown.

---

## Memory / streaming knobs: `--no-streaming`, `--read-batch-size`

Like the [file layout knobs](#file-layout-knobs---row-group-size---full-column-stats),
these never change the output's content: level assignments, geometry,
attributes, and footer metadata are equivalent either way. They control **how
much memory the conversion itself uses**.

By default the converter runs a **two-pass streaming pipeline** (H3):

1. **Pass 1** streams the input once and keeps only a tiny record per feature
   (bbox, geometry kind, ranking key — no geometry). The level-assignment
   engine and density budget run over those records to build the **winner
   table**: one byte per feature saying which levels it survives at.
2. **Pass 2** re-reads the input once per level (Parquet is seekable, so
   re-reads are cheap), filters each read batch against the winner table,
   simplifies only the selected rows, and writes batch-by-batch.

Peak memory is `O(read batch + winner tables)` instead of `O(dataset)`: on the
Moldova corpus file (632k polygons, 38M vertices) peak RSS drops from ~5.4 GB
(in-memory) to well under 1 GB, with equivalent output.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--read-batch-size N` | `8192` | rows per read batch | **bigger = faster-ish, more memory** |
| `--no-streaming` | off | flag | revert to the one-pass in-memory pipeline |

**`--read-batch-size`** bounds the transient working set of both passes: each
batch is decoded, filtered, simplified, and written before the next is read.
LARGER batches amortize per-batch overhead (marginally faster) at the cost of
proportionally more peak memory; SMALLER batches bound memory tighter. The
default 8192 keeps per-batch transients in the tens of MB even for
vertex-heavy polygon data; you rarely need to change it. LOWER it (e.g. 1024)
on very memory-constrained machines or for monster geometries (a single batch
of coastline-sized multipolygons can be large); RAISE it (e.g. 65536) only if
profiling shows per-batch overhead dominating on a machine with RAM to spare.

**`--no-streaming`** runs the original in-memory pipeline: the whole table and
every decoded geometry are held at once (`O(dataset)` memory). It reads the
input exactly once instead of once per level, so it can be marginally faster
on *small* inputs that comfortably fit in RAM; on large inputs it is both
slower and enormously more memory-hungry. It is kept as the reference
implementation — the two paths are equivalence-tested against each other —
and as an escape hatch; there is no output-quality reason to use it.

Residual per-feature memory in streaming mode (the "winner tables") is ~50–80
bytes per input feature during pass 1 and 1 byte per feature during pass 2 —
about 40 MB / 0.6 MB for a 632k-feature file — so even planet-tier inputs
stay laptop-sized.

---

## Worked scenarios

| Symptom | Fix |
|---------|-----|
| Coarse roads look like sparse disconnected dashes | LOWER `--line-thinning` (e.g. 2 → 1) and/or `--line-visibility`; or RAISE `--gsd-base` |
| Coarse roads are all there but jagged / over-smoothed | LOWER `--simplify-factor` (e.g. 1.0 → 0.5) |
| Wrong roads survive (residential instead of highways) at coarse zoom | add `--class-rank road_class:…` or rely on auto-detect (don't pass `--no-auto-rank`) |
| Coarse level files are too large / slow | RAISE the thinning factors and/or `--simplify-factor`; or LOWER `--gsd-base` |
| Small buildings vanish too early | LOWER `--polygon-visibility`; or `--collapse` to keep them as points |
| Whole map uniformly too sparse or too dense | move `--gsd-base` (up = denser, down = sparser) instead of tuning each family |
| Mid zooms have far more features than tippecanoe / duplicating files too large | RAISE `--drop-rate` (density budget); or `--no-density-drop` to turn it off |
| Density cut is stripping sparse rural areas to keep cities | RAISE `--drop-gamma` (sparse-area protection) |
| Dense point data renders as a misleadingly sparse dot field at coarse zooms | `--cluster` and style the symbol size by `point_count` |
| Need per-cluster totals/averages of a numeric column | `--accumulate-attribute col:sum` / `col:mean` (with `--cluster`) |
| Every remote query fetches a huge footer before any data | default already suppresses string/geometry stats; do NOT pass `--full-column-stats` |
| Need server-side row-group skipping on a property predicate | pass `--full-column-stats` (bigger footer, gains column pruning) |
| Tiny viewports over high-latency storage fetch too much | LOWER `--row-group-size` for tighter bbox pruning |
| Conversion runs out of memory / swaps on a big file | streaming is already the default; LOWER `--read-batch-size`; make sure `--no-streaming` is NOT set |

See `corpus/SWEEP_NOTES.md` for an empirical `--line-thinning` ×
`--simplify-factor` sweep on Portland roads, and the Q2 section there for the
`--drop-rate` calibration (before/after ratios vs tippecanoe).
