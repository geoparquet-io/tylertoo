# Overview Generalization Tuning

`tylertoo overview` turns a GeoParquet file into a multi-resolution
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
> - **Country-scale view of a dense small-polygon layer (buildings, parcels)
>   is EMPTY when zoomed out** → the features are sub-GSD at those scales and
>   no gate value alone can save them; use the
>   [dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers):
>   `--polygon-visibility 0 --collapse` (+ a circle layer in your style).
> - **A rank-ordered cut is emptying sparse rural areas** → RAISE `--drop-gamma`
>   (protects sparse neighborhoods).
> - **Conversion is slow / pins one core** → it now reads the input once and
>   uses all cores; RAISE `--in-flight-batches` for more read/compute overlap.
> - **Conversion runs out of memory in `speed` mode** → `--profile bounded`
>   (spills each level to temp files; `--profile auto`, the default, now does
>   this for you when the estimated buffered output exceeds a fraction of
>   available RAM — large duplicating *and* partitioning runs both spill).
>   Output is byte-identical either way.

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
| `--min-zoom` / `--max-zoom` | `0` / `6` | Web Mercator zoom | coarsest / finest (canonical) level (max **30**) |

**Zoom ceiling: 30 — an addressability limit, not a recommendation.** Every
zoom tylertoo writes must fit the tile arithmetic: tile coordinates are 32-bit
(so z31 is the hard limit) and the PMTiles Hilbert tile id needs `4^z` of
headroom in a `u64`. 30 is what the math supports with a level of margin. A
`--max-zoom` above 30 is **rejected at option validation**, before the input is
opened, rather than running for hours to produce an archive whose every tile id
is wrong. The same ceiling applies to `--min-zoom` and to a `--gsd` fine enough
to *imply* a zoom above it — an explicit GSD ladder records no zoom, so the
implied one is checked on the same footing (#371).

The ceiling says a zoom can be *addressed*, not that it is practical. Cost
grows roughly **4x per zoom**: z30 is ~1.15e18 tiles at a GSD of ~3.6e-5 m, far
past what any real dataset resolves. Points are cheap — a point touches one
tile at every zoom — but line and polygon data is not: a single 1-degree
linestring spans on the order of 3e6 tiles at z30, and rendering it is the
same CPU-bound, RSS-growing walk that made an unbounded `--max-zoom` a bug in
the first place. In practice the highest useful `--max-zoom` for non-point data
is far below the ceiling; raise it a level at a time and watch the output size.

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

## Several inputs, one archive: `tylertoo pyramid`

`--verbatim` makes one input tile honestly across the zooms it owns. A
**pyramid** goes further: it serves the same map from a *different input* at
different zooms — a pre-aggregated summary at coarse zooms, the raw features at
fine ones — in one archive, so a client switches at the handover with no second
request.

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

Two bands may share a layer name, as `aggregate` does here: to a client it is
one layer whose content changes at z6. Two bands of the **same** layer must not
share a zoom — they would write the same tile ids — and that is checked before
any tiling happens, so a bad plan costs an error rather than a conversion.
Bands in **different** layers may share zooms (tippecanoe's `-L`): at the
zooms they share, a tile every band wrote carries every band's layer, and a
tile only one band wrote passes through untouched. So
`--band 0-13:a.parquet:2024 --band 0-13:b.parquet:2025` is one archive with
two independent layers over the same zooms.

**A band is tiled verbatim by default.** That is the premise of banding: the
input already *is* the right resolution for the zooms it owns, so running the
ladder over it would re-introduce exactly what the pyramid avoids. Pass
`--generalize` when a band is raw features spanning several zooms and you do
want the ladder inside it.

**A band may be GeoParquet or an already-tiled PMTiles archive.** Both are
spelled `--band LO-HI:PATH[:LAYER]`; the kind is detected from the file
contents, not the extension. A source is tiled here, restricted to the band's
range; an archive is merged as-is. That keeps the older two-step workflow
(tile each band yourself, then merge) working unchanged, and lets you mix:
reuse last week's coarse archive and re-tile only the fine band.

**A band source may be remote.** `--band 9-13:https://data.source.coop/…/features.parquet`
streams with byte-range requests exactly as `tiles` and `overview` do, so a
band no longer has to be staged locally first. A band *archive* must be local:
the merge reads its directories and tile bytes by offset, so a remote
`.pmtiles` band is refused with a message saying to stage it.

`LAYER` defaults to the input's file stem — or, for a glob or a directory,
the directory name, since a layer called `*` is not useful to a client. A path
containing a colon (`s3://…`, `https://…`, `C:\…`) is fine; the layer is only
split off the **last** colon when what follows it has no `/`, `\` or `:`, so a
URL's scheme, a port and a `2024:06/` directory all stay part of the input. A
drive-relative path with no `\` after the drive, e.g. `C:data.parquet`, stays
whole too: a single ASCII letter before the last colon is treated as a drive
letter rather than a path, even though `data.parquet` alone would otherwise
look like a bare layer name.
The one shape that rule cannot express is an input *ending* in a bare colon
segment — a Hive directory such as `admin:country_code=BR`. For those, spell
the band with `=` instead: **`--band LO-HI=INPUT[=LAYER]`**, where the range is
split at the first `=` and the layer at the last, again only when the segment
after it has no `/`, `\` or `:`. That keeps
`--band 0-13=admin:country_code=BR/part.parquet` whole; an input that itself
ends in `=VALUE` is indistinguishable from a layer, so give it an explicit
`=LAYER`. For a pre-tiled archive the layer is a *label*: the layer
name inside its tiles is whatever the archive was exported with, and when bands
share zooms the two must agree, or the combined tile would hold two layers of
one name (which a client resolves by dropping one). A **gap** between bands is
allowed but warns, because the merged archive advertises one continuous range
and a client will request the uncovered zooms and get nothing.

Intermediates go to `--work-dir` (default: the system temp directory) and are
removed whether the build succeeds or fails. Budget for it: a band is tiled in
duplicating mode, so its intermediate overview holds roughly *levels × input
size*, every band's finished archive is on disk at once before the merge, and
the merge itself spools through the temp directory.

---

## Switching the ladder off entirely: `--verbatim`

Everything below this heading generalizes: thinning, visibility gates,
simplification, the density budget, coalescing. That is right when the coarse
level should be a *coarser drawing of the same features* — a road network, a
building footprint layer.

It is wrong when the input is already the right resolution for the zooms you
are asking for. An H3 r6 cell is not a simplified r7 cell; it is their parent,
and its count is their sum. Run a cell aggregate through the gates and the
gates ask "is this feature big enough to see" when the question is "what do
these cells sum to" — so a coarse level shows *some* cells and silently omits
the rest:

```
$ tylertoo tiles cells.parquet out.pmtiles --min-zoom 0 --max-zoom 5
WARN omitting 1 empty level(s) [0] … none of the 3399 input feature(s)
     are visible at those scales (visibility gates / density budget)
  z2  (level 0):  757 features      ← 3,399 cells in, 757 drawn
  z3  (level 1): 1446 features
```

`--verbatim` turns the whole ladder off, so every level reproduces the input:

```
$ tylertoo tiles cells.parquet out.pmtiles --min-zoom 0 --max-zoom 5 --verbatim
  z0  (level 0): 3399 features
  z1  (level 1): 3399 features
  z2  (level 2): 3399 features
  …
```

It is exactly equivalent to setting every ladder knob by hand — byte-identical
output — and exists because that is eight flags of ceremony for one
intention:

```bash
--no-density-drop --no-coalesce-lines --simplify-factor 0 --point-thinning 0 --line-thinning 0 --polygon-thinning 0 --polygon-visibility 0 --line-visibility 0
```

Reach for it when the input is a **DGGS or cell aggregate**, is **pre-levelled**
by an upstream tool, or is **one band of a pyramid** (see `tylertoo pyramid`)
where a different input already owns the coarser zooms.

What it does *not* touch: mode, the level plan, CRS, row-group layout, an
explicit `--cluster`, and `--representation` (a point or square band still
replaces polygons with centroids or dithered placeholders — the one
generalizing knob left on, inert unless you ask for it).

`--verbatim` supplies **defaults**, it does not override: any knob you set
explicitly wins, so `--verbatim --simplify-factor 0.5` keeps every feature but
still simplifies geometry. When you do override something, the run says so
rather than claiming the output is verbatim.

It requires `--mode duplicating`. Partitioning writes each feature at exactly
one level, so with thinning off every feature lands in the coarsest level and
every finer level comes out empty; that is rejected rather than produced.

On `tiles` it also disables the per-tile size cap, since a valve that sheds
features to fit a byte budget is not verbatim either. Pass `--max-tile-size`
explicitly to put a cap back; an explicit value always wins.

⚠️ **The two-step form does not inherit that.** `tylertoo overview --verbatim`
followed by `tylertoo export-pmtiles` still applies the export's own 500K cap
and sheds features from oversized tiles — the flag is convert-side, and the
export reads a file, not your flags. Pass `--tile-size-limit 0` to
`export-pmtiles` (that is the export spelling; `--max-tile-size` is the `tiles`
one) when every feature must reach a tile.

### `0` is the off switch

Every thinning factor accepts `0`, meaning **no thinning**: each feature is its
own grid cell, so every feature that passes the gates survives. Previously `0`
was rejected (`must be a finite value > 0`) and callers used `1e-9` to
approximate it; that workaround is no longer needed, and `0` is exact rather
than dependent on float resolution to separate neighbouring cells.

Negative and non-finite factors remain errors.

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
the 2026-07-02 Portland roads sweep (`corpus/SWEEPS.md`): at 1.0, road
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
| `--polygon-visibility` | `2.0` | × GSD (min bbox diagonal) | **bigger = sparser** |

Bigger factor ⇒ higher bar ⇒ more small features dropped at coarse levels ⇒
**sparser**. Smaller ⇒ more small features kept ⇒ **denser**.

The polygon default was retuned 4.0 → 2.0 in the 2026-07-15 coarse-zoom
sweep (#259, `corpus/SWEEPS.md` Decision 6). The write stage independently
drops any polygon whose *simplified* geometry falls below the level
tolerance (`--simplify-factor × GSD` — an effective ~2 × GSD survival bar
on real-world shapes), so a 4 × GSD eligibility gate was strictly stricter
than what could ever be written and only starved coarse zooms: on Germany
buildings, 4.0 → 2.0 gives 2.7–4.6× more features at z8–z12 and starts the
pyramid one zoom coarser, for +13 % file size and +7 % conversion time.
Values *below* 2.0 change little on their own — the extra eligible features
are almost all RDP-collapsed at write time — **unless** `--collapse` keeps
them as representative points (see the
[dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)).

⚠️ **Interaction warning — the gate is multiplied by the GSD.** So it moves with
`--gsd-base` and with zoom. A polygon visible at one level can be gated out one
level coarser purely because the GSD (and therefore the gate) doubled. This is a
*hard drop*, distinct from thinning's *one-per-cell* competition: a feature can
be gated out even if its cell is otherwise empty.

### Empty coarse levels: the auto-clamp

When **every** feature is culled at a coarse level — the normal outcome for
datasets of small features at world zooms (e.g. country-scale buildings with
`--min-zoom 0`: no building's bbox clears `2 × gsd(z0)` ≈ 78 km) — that level
is **omitted from the output and the remaining levels are renumbered** (spec
§7.3), instead of failing the conversion. You will see a `WARN` like:

```
omitting 6 empty level(s) [0, 1, 2, 3, 4, 5] spanning GSD 39135.76–1222.99 m:
none of the 59032924 input feature(s) are visible at those scales (visibility
gates / density budget); the output pyramid starts at GSD 611.50 m (zoom 6).
To populate coarse levels, lower --polygon-visibility/--line-visibility, or
pass --collapse to keep sub-GSD polygons as representative points (see
docs/OVERVIEW_TUNING.md)
```

plus a `note:` line under the CLI level table, and the omitted levels are
recorded in the report's `skipped_empty_levels` (planned level index, GSD,
zoom) — the written `levels` array is the effective range. The same clamp
applies when a level empties late, during simplification (a pathological
feature whose bbox clears the gate but whose geometry collapses below the
level tolerance). PMTiles export of a clamped file starts at the coarsest
*written* level's zoom.

This is expected behavior, not data loss: the features are simply not
representable at those scales. To *force* coarse levels to be populated,
lower the gates (`--polygon-visibility` / `--line-visibility`), coarsen the
ladder (`--gsd-base`), or — for dense small-polygon layers — use the
[dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)
(`--polygon-visibility 0 --collapse`). If **no** level has any rows at all
(empty input, or an empty `--bbox` selection), the conversion still fails
with a hard error.

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
| `--collapse-square` | off | flag | dropped polygons stand in as ~1×GSD placeholder squares: a per-patch area accumulator for everything a level does not carry, an area dither for members that collapse at write time (type-preserving) |
| `--representation` | none (all `geom`) | `LO-HI:KIND,…` | per-zoom-band representation: `geom`, `point`, or `square` |
| `--no-cascade` | off (cascading **on**) | flag | disables cascading simplification, reproducing pre-cascade output byte-for-byte |

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

### Cascading simplification (default on): `--no-cascade`

By default (duplicating mode), each coarser level is simplified from the
**next-finer level's already-simplified output** rather than from canonical
full-resolution geometry (tippecanoe-style, #218). Because a feature at its
coarsest level would otherwise be re-simplified from full resolution at every
finer level it repeats on, cascading removes the dominant repeat cost of
duplicating-mode conversion; it also repairs a self-intersecting RDP candidate
into its valid even-odd interpretation in one boolean-overlay pass instead of
epsilon-retrying it four times and shipping full resolution.

Trade-off: coarse-level coordinates differ slightly from the non-cascaded
pipeline. Cascaded output vertices are still a subset of canonical vertices,
every step's output is validity-checked or repaired, and the cumulative
positional deviation is bounded by the geometric GSD ladder (≈ 2× the target
level's tolerance instead of 1×). Files record `generalization.cascade: true`
in the footer provenance. Pass `--no-cascade` to reproduce the pre-cascade
output byte-for-byte.

Validity-check vertex cap (#242): a simplification candidate with more than
2,048 total vertices skips the exact validity check and is assumed valid —
the check is O(V²) in ring size, and on continental-scale rings at fine GSDs
it stalled conversion for tens of minutes per feature. Oversized candidates
arise exactly when RDP removed few vertices from already-valid input, the
least likely case to have acquired a self-intersection; geometry validity is
not an overviews conformance requirement (OVERVIEWS_SPEC §2). Candidates at
or below the cap are validated (and repaired) exactly as before. A skipped
check is counted and summarized at the end of conversion at info level.

---

## Country-scale dot fill for dense polygon layers

A dense layer of *small* polygons (buildings, parcels, field boundaries)
renders an **empty country view** with type-preserving defaults, no matter
how the gates are tuned. Two independent mechanisms cull sub-GSD polygons
at coarse levels:

1. the **visibility gate** (assign time): ineligible below
   `--polygon-visibility × GSD`;
2. the **write-time collapse** (simplify time): even an eligible polygon is
   dropped when its simplified geometry falls below the level tolerance —
   at z4 the tolerance is ≈ 2.4 km, and no building survives that.

A 20 m building is therefore unrepresentable *as a polygon* at z0–z8; the
per-feature fix is representing it as something else. That is what
**`--collapse`** does (spec Q4, opt-in): a below-tolerance polygon becomes a
**representative point** instead of vanishing. Combining it with a zero
gate turns the coarse levels into a cell-winner/budget-bounded **dot
field**:

```bash
tylertoo overview buildings.parquet buildings_overview.parquet \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse

# One-shot to PMTiles. The 500K tile cap is on by default (tippecanoe
# parity, #280) — shown explicitly here for clarity. Uncapped coarse dot
# tiles on a country-scale layer reach several MB (Germany z6: 12 MB),
# far past renderer norms; the cap drop-to-fits them to ~500 KB (keeping a
# spatially even stride of the dots). Pass --max-tile-size 0 to opt out.
# On multi-GB inputs the recipe buffers the (now much larger) coarse
# levels; the default --profile auto estimates this and spills when it
# would exceed a fraction of available RAM (Germany peak RSS 15 -> 29 GB
# if forced to speed). Pass --profile bounded to force spilling.
tylertoo tiles buildings.parquet buildings.pmtiles \
  --min-zoom 0 --max-zoom 14 \
  --polygon-visibility 0 --collapse --max-tile-size 500K \
  --profile bounded
```

Measured on Overture Germany buildings (59M footprints, z0–14,
`corpus/SWEEPS.md` Decision 6): every level populates (z0 = 581 dots,
z4 = 128,886, z6 = 1,074,540), coarse levels z6–z13 land exactly on the
[density budget](#density-budget---drop-rate---drop-gamma---no-density-drop)
ladder `N / 1.65^(14−z)` with the gamma-fair spatial spread (Ruhr/Berlin
visibly denser at z6, rural areas protected), and the overview file grows
+31 % (11.9 → 15.6 GB) with +40 % conversion time. On the Moldova field
corpus the same recipe costs **+0.3 %** file size.

Two caveats:

- **Style the points.** A `fill` layer silently ignores Point features — a
  fill-only style renders the same empty country view you started with.
  Add a small `circle` layer filtered to `["==", "$type", "Point"]` (see
  `docs/demo/viewer.html`).
- **Geometry type changes mid-zoom.** The output's `geometry_types` lists
  the union (e.g. `["Point","Polygon"]`), per spec §7.5; collapse is
  opt-in precisely so renderers are never surprised by default
  (spec Q4). Files record the collapse in the `generalization` provenance.

`--drop-rate` / `--drop-gamma` need **no** retuning for this recipe: the
coarse levels are capped by the existing budget ladder, which is what
shapes the dot density (the #250 demo's `--drop-rate 1.3` only inflated
mid-zoom row counts by +13 % without changing the coarse fill).

---

## Zoom-band representation: `--representation` (#317) and placeholder squares: `--collapse-square` (#279)

The dot-fill recipe above changes the representation of below-tolerance
polygons **globally**. Two further knobs give you (a) full per-zoom-band
control and (b) a **type-preserving** alternative to points.

### `--representation LO-HI:KIND,…` — the band selector

One run, one archive, different representations per zoom band — no
two-archive merge:

```bash
# Dots zoomed out, full polygons zoomed in, in ONE PMTiles:
tylertoo tiles buildings.parquet buildings.pmtiles \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point,8-14:geom"

# Tippecanoe-style placeholder squares at coarse zooms instead:
tylertoo tiles buildings.parquet buildings.pmtiles \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:square"
```

`KIND` per band:

- **`point`** — every polygonal feature in the band becomes its
  **representative point** (centroid, falling back to bbox center then
  first vertex for degenerate rings) — unconditionally, whatever its size.
  In-band polygons **bypass the visibility gate** (a dot is always
  visible) and thin on the **point grid** (`--point-thinning`), so the
  band renders as a dot field with genuine coverage. Style with a
  `circle` layer (fill layers ignore points).
- **`square`** — normal simplification, but **below-tolerance** polygons
  emit an area-dithered ~1×GSD **placeholder square** (see
  `--collapse-square` below) instead of dropping. Visible polygons are
  untouched, the level stays all-`Polygon` (type-preserving — plain fill
  styles keep working). In-band polygons bypass the visibility gate (the
  tiny ones are exactly what the dither must see) but keep the polygon
  thinning grid.
- **`geom`** — the normal path (default for zooms not listed in any
  band). Below-tolerance polygons follow the global disposition
  (drop / `--collapse` / `--collapse-square`).

Rules (validated at convert entry): duplicating mode with a zoom-range
plan only (`--gsd` plans carry no zooms to band on); bands must not
overlap; a non-`geom` band must end **before** `--max-zoom` (the canonical
level is always verbatim, spec §2.4); and `point` bands must be contiguous
from the coarsest planned zoom — the cascade's point passes through every
coarser level regardless, so a "polygons coarser than the point band"
request is not representable and is rejected rather than silently ignored.

With cascading (default on), a feature entering a point band collapses at
the band's **finest** level and every coarser band level reuses that same
point. Lines and native points are unaffected by every band kind; bands
combine freely with clustering, coalescing, and the density budget (which
caps band levels exactly like any other level).

The requested bands are recorded in the footer provenance
(`generalization.representation: [{"zooms": [0,7], "repr": "point"}, …]`).

### `--collapse-square` — tippecanoe tiny-polygon squares as the global disposition

`--collapse-square` is the third **below-tolerance disposition**, next to
the drop default and `--collapse`-to-point: polygons a level cannot show
as themselves stand in as `tol × tol` squares (`tol = simplify-factor ×
GSD`, CRS-converted). This is tippecanoe's **tiny-polygon reduction** —
the primary-reference behavior for keeping dense small-polygon layers
(fields, buildings, parcels) visible at coarse zooms with **no style
changes**: squares are polygons, `geometry_types` stays `["Polygon"]`, and
there is no spec-Q4 geometry-type opt-in involved.

Two mechanisms share the threshold `T = tol²` (#384):

**The accumulator** covers every polygon the level does *not* carry —
failed the visibility gate, lost its thinning cell, cut by the density
budget. After assignment, each such polygon adds its area, **clamped to
`T`**, to a running total for its **patch** (a 32×GSD square, 1/32 of a
1024-px tile), in input order; each time a patch's total crosses `T`,
the polygon that crossed it becomes the level's *carrier* and is emitted
as a `T`-area square at its representative point, with its own
attributes. A polygon contributes at most one placeholder of area
(a gate-failed polygon is often bigger than `T` — the gate is
`--polygon-visibility` pixels wide — and a thinning or budget loser can
be any size), and a patch keeps less than one `T` unemitted. This is
what makes a country of 25 m fields read as farmland at z0 instead of
vanishing: on a 368k-field sample, z1–z6 carry ~98% of the input area
against 1.5–7% with the dither alone. Features placed by an entry-zoom
ladder (`--entry-zoom`) are never accumulated: the ladder decides where
they first appear.

**The dither** covers the polygons the level *does* carry but that
collapse below `T` at write time (RDP shrinks them under the tolerance):
one of area `A` survives as a square with probability `A / T`, decided by
a hash of its anchor coordinates. The two sets are disjoint, so nothing
is counted twice.

Both are **deterministic**: the accumulator runs once over the pass-1
feature table in input order, so every engine (in-memory / streaming /
pipelined) reads the same carrier set, and the dither is a pure function
of the feature. The same input produces byte-identical output across
runs, engines and thread counts. Under cascading, a kept square's anchor
is its own center, so coarser levels re-dither it against the same hash
draw with a shrinking keep probability — survival is monotone fine→coarse.

Divergences from tippecanoe (see `context/ARCHITECTURE.md`):

- tippecanoe accumulates **per tile**; we accumulate per 32×GSD patch of
  the level, since overview levels have no tile scope. Squares therefore
  land at the carriers' own positions inside the patch (input order is
  Hilbert order for a prepared file, so they cluster where the fields
  are).
- tippecanoe only accumulates rings with area ≤ `tiny_polygon_size²` and
  keeps larger rings as geometry; we clamp each polygon's contribution to
  one placeholder instead of skipping large ones — a non-member was
  already dropped by the gate or thinning here, so there is no geometry
  to keep.
- tippecanoe places the placeholder at the ring's first vertex with side
  `tiny_polygon_size` (default 2 px); ours sits at the polygon's
  representative point with side `simplify-factor × GSD`.
- the accumulator needs duplicating mode (a carrier is a second appearance
  of a feature, which partitioning's feature-once contract cannot
  represent). In partitioning mode neither mechanism applies: levels are
  verbatim, so `--collapse-square` is accepted and logged as inert.

At the mid zooms, a dense square carpet can exceed `--max-tile-size`; the
valve then sheds squares for that tile, as it would any feature. Raise
the cap or accept the thinner carpet.

Opt-in for now: the drop default is unchanged pending the #259-fixture
sweep (#279 tracks the default decision). Recorded in the footer
provenance as `generalization.collapse: "square"` (`--collapse` records
`"point"`).

---

## Attribute-driven entry zoom: `--magnitude-ladder`, `--entry-zoom`

Everything in the next section decides *which of the features competing for a
cell wins*. It runs **after** the visibility gate, which has already dropped
features for being small. That ordering is backwards whenever a dataset's most
important features are also its physically smallest.

A population-density choropleth is the everyday case: the dense urban tracts
carrying the highest values are tiny next to the sparse rural ones. A coarse
level therefore keeps the large low-value polygons and hides the small
high-value ones — precisely the opposite of what the map should show zoomed
out. `--sort-key` helps where features survive to compete, but cannot reach a
feature the gate removed first.

A **ladder** answers a different question: not "which of these wins a cell" but
"how early may this feature appear at all". Each distinct value of a column
gets an *entry zoom*; a feature appears from its entry zoom inward and at none
before it, **exempt from the visibility gate and from thinning** throughout.

```bash
# Derive it: distinct values ranked descending, one zoom apart from --min-zoom
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 6 \
  --magnitude-ladder density

# ...or place the rungs by hand. Rungs must fall inside the zoom range, and
# each names a value the column actually carries.
tylertoo tiles in.parquet out.pmtiles --min-zoom 0 --max-zoom 8 \
  --entry-zoom "density:5000=0,1000=3,200=6"
```

With five distinct values over `--min-zoom 0 --max-zoom 6` and the default
step, counting features present at each zoom, the derived ladder gives a clean
staircase: the strongest rank is the only thing at z0, and each weaker rank
joins one zoom later. Without a ladder the strongest values do not appear at
all until the gate stops removing them — typically the finest zooms, because
they are the smallest features in the layer.

Ranking **distinct values** (SQL `DENSE_RANK`) rather than the values
themselves keeps the ladder scale-free. Mapping a raw value linearly onto the
zoom range strands every rung in the upper zooms whenever the values occupy a
narrow part of their nominal scale, which real data usually does.

More distinct values than the zoom range has room for is fine: the ranks that
would fall past the finest zoom get no rung, and those features keep the
ordinary gate and thinning rather than being pinned to the finest level. The
run says how many were left out.

`--ladder-step N` widens the spacing. A value the ladder does not cover (null,
or unlisted in an explicit spec) gets no entry zoom and takes the ordinary
gate, so a partly populated column degrades to today's behaviour instead of
hiding rows.

### Two mechanisms can still undo a ladder

The ladder governs **admission**. Two later stages can remove a feature it
admitted, and both matter in practice:

- **Simplification.** A feature admitted to a coarse level is still dropped
  there if its geometry simplifies below that level's tolerance — the usual
  case for small-but-strong features. `--magnitude-ladder` and `--entry-zoom`
  therefore **imply `--collapse`**, so such a feature survives as a
  representative point. `--collapse-square` overrides.
- **The density budget**, which caps each level and sheds its lowest-priority
  survivors *by size* — the same inversion the ladder corrects. Pair with
  `--no-density-drop`, or with `--sort-key` on the same column so the budget
  ranks the way the ladder does. tylertoo warns when a ladder runs with the
  budget on and no sort key.

With the budget off, the staircase is exact — each rank appears at its own
zoom and at none before it, and every rank already admitted stays:

```
zoom     rank 4  rank 3  rank 2  rank 1  rank 0   (0 = highest value)
z0            0       0       0       0     140
z1            0       0       0     140     140
z2            0       0     165     165     165
z3            0     192     176     176     176
z4          250     192     176     160     160
```

### Relation to tippecanoe

This is tippecanoe's per-feature `tippecanoe.minzoom`, the one attribute-driven
thinning lever it offers. tippecanoe takes the minzoom as an input attribute;
`--magnitude-ladder` additionally *derives* one from a column, which is what
callers were doing by hand upstream of it.

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

**Unrankable values.** A null ranks below every real key: a feature with no key
still appears, it just loses any cell it contests to one that has a key. A NaN
or infinity in a numeric column ranks the same way — those are how float
columns usually spell nodata, and neither is a priority anything can compare
against. Such a row is never dropped for it; it competes as a keyless feature.
The entry-zoom ladder reads its column by the same rule, so a non-finite value
is no rung. `--accumulate` is aggregation rather than ranking and is one notch
looser: a NaN is skipped there too, but an infinity is a real summand (see
[Clustering](#clustering---cluster---accumulate-attribute)).

---

## Density budget: `--drop-rate`, `--drop-gamma`, `--no-density-drop`

Cell-winner thinning (above) stops binding once its grid cell is smaller than
the typical feature spacing: from roughly z9 up, *every* feature wins its own
cell, so per-level counts plateau at ~the whole dataset. On Portland roads that
plateau is 2–3× tippecanoe's feature count at z9–z11 (see
`corpus/SWEEPS.md`) — visual clutter, and the main driver of duplicating
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
(`corpus/SWEEPS.md`): it brings z9 to 1.21× and z10 to 1.03× tippecanoe
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
tylertoo overview places.parquet places_overview.parquet \
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
- A **NaN** doesn't contribute either — it is nodata far more often than a
  value, and one would poison every aggregate it touched. It is not counted
  as a contributor, so `mean` is the mean of the real values. **Infinities
  are kept**: `±inf` is an ordinary summand here, so `value:max` over
  `{1.0, +inf}` is `+inf` at every level, agreeing with the canonical level,
  which carries the source value verbatim.
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
`accumulated: [{column, op}]`); `tylertoo validate` checks that the column
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
- **Junctions** (3+ compatible endpoints meeting) terminate chains by
  default, preserving network topology. `--coalesce-junction-angle` (see
  below) can optionally continue the best-aligned pair through them.
- The merged feature keeps the **attributes of its highest-priority
  member** (same class-rank → size → hash order as the cell-winner stage)
  and the output gains a **`coalesced_count`** INT32 NOT NULL column
  (source segments merged per row; 1 for unmerged rows and everywhere at
  the canonical level, which is never coalesced). It is withheld from tiles
  when it is 1 everywhere — nothing was merged, so it says nothing about
  the data (#379); the overview file keeps it.
- Points and polygons are untouched. MultiLineString rows pass through
  unmerged.

```bash
# Coalescing is on by default (auto class ranking groups by road class):
tylertoo overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14

# Opt out (pre-Q3 behavior, no coalesced_count column):
tylertoo overview roads.parquet roads_overview.parquet \
  --min-zoom 0 --max-zoom 14 --no-coalesce-lines
```

### `--coalesce-junction-angle` (default 0 = off, degrees)

By default chains stop at junctions (strict degree-2 chaining) — the
Portland sweep (`corpus/data/bench/q3/portland-roads-junction{00,30}.pmtiles`,
maintainer review 2026-07-03) found this renders better than junction
continuation, which over-merges. Set an angle to opt in: at each junction
the best-continuing pair of lines merges when its deviation from a
straight continuation is at most this angle, best pair first (a 4-way
crossing continues **both** through-streets). BIGGER = chains bend
further through junctions (fewer, longer strokes; z0–z1 gain giant
arterial strokes on Portland at 30°) at the cost of merging through
genuine turns and smearing attributes across crossings.

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
provenance block, so the overview schema is stable (the tiles then withhold
the column, as for any all-1 counter). Levels that large are
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
`snap_tolerance_gsd_factor`, `junction_angle`, `max_level_rows`,
`coalesced_count_column` — the complete knob set, spec §13.4); `tylertoo
validate` checks that the column exists as INT32 NOT NULL, all values are
>= 1, and canonical-level values are all 1.

---

## Reserved column names

tylertoo appends its own columns to the intermediate overview GeoParquet, and
those names are reserved. A source column whose name collides (comparison is
case-insensitive) is **renamed** by appending `_`, looping until the name is
free — `level` → `level_`, `LEVEL` → `LEVEL_`, `level_` → `level__`. The
reserved column is authoritative.

| Reserved name | Reserved when | Reaches MVT? |
|---|---|---|
| `level` | always | **no** — dropped from tile properties |
| `point_count` | `--cluster` | yes |
| `coalesced_count` | line coalescing (on by default) | yes, unless 1 on every row (then withheld from tiles; the overview file keeps it) |

A rename is logged at `warn`:

```
input column "level" collides with the reserved overview column "level";
renaming the input column to "level_" in the output
```

**In PMTiles output the source name is given back where it is free** (#359).
Because tylertoo's own `level` never reaches MVT properties, a source `level`
is published in tiles as `level` — the rename stays an implementation detail of
the intermediate GeoParquet, which is the only place the collision is real:

```
overview.parquet:   level_ (your data)  +  level (tylertoo's)
out.pmtiles:        level  (your data)
```

This also holds for a standalone `export-pmtiles` run on an overview file
written earlier: the rename is recorded in the file footer
(`geo:overviews.generalization.renamed_columns`, output name → source name), so
the export does not need the converting run's state.

Restoration is conditional. `point_count` and `coalesced_count` *are* real MVT
properties in the modes that append them, so a column moved aside from one of
those keeps its renamed name — merging two columns into one tile property would
be worse than the wrong name. Only names that are genuinely free are restored.
A `coalesced_count` withheld from the tiles (1 on every row) counts as free, so
a source `coalesced_count` is then published under its own name.

By-name options follow the rename automatically (`--sort-key level`,
`class_ranking.column`, `--accumulate-attribute`, `--filter`), so you do not
need to spell the renamed name yourself.
## Output feature order: `--feature-order`

MVT does not define draw order, but renderers paint features in the order the
tile lists them. So the order tylertoo writes features in **is** the paint order
for any style that does not override it — and a style that relies on it will
render differently against a differently-ordered archive of the same data.

**tylertoo's default is input row order**, exactly: the overview file's row
order, which is the source file's row order restricted to the rows each level
kept. Measured across z12–z14 on a nested-polygon fixture, the within-tile
sequence has zero inversions against source row order at every zoom.

**Do not assume tippecanoe matches it.** Tippecanoe's order is incidental
rather than specified — it varies by zoom and by tile, and on the same input it
can point the opposite way at some zooms and the same way at others (#361).
If your style depends on paint order, pin it rather than inheriting it:

```bash
# Source row order (the default, and what tylertoo has always emitted)
tylertoo tiles in.parquet out.pmtiles --feature-order input

# Sort within each tile by a property: high `level` painted last (on top)
tylertoo tiles in.parquet out.pmtiles --feature-order level

# ...or first (underneath)
tylertoo tiles in.parquet out.pmtiles --feature-order level:desc
```

Accepted on both `tiles` and `export-pmtiles`.

Sorting by a column is the fix for any nested-polygon choropleth, where the
small high-value shapes sit inside larger low-value ones and must land on top.
`--feature-order level` does in the archive what
`"fill-sort-key": ["get", "level"]` does in every downstream style, so
consumers do not each have to know.

Details that make the result reproducible:

- **Ties keep input order.** The within-tile sort is stable over the
  `(tile, row)` order, so two runs of the same build produce byte-identical
  tiles.
- **Ordering is per tile.** Nothing moves across tile boundaries; the option
  changes the sequence within a tile and nothing else.
- **Numeric types compare as numbers.** A column read as an integer in one
  row group and a double in another is one ordering class, not two.
- **A feature missing the property sorts first** (painted underneath) rather
  than being dropped. A `NaN` is treated the same way: it is a value no style
  can rank, so it joins the unrankable features underneath rather than taking
  an arbitrary place among the numbers.
- **Sorting is independent of the oversized-tile valve.** When
  `--max-tile-size` is in force, which features survive is decided by
  `select_kept_members` (largest-first, or a uniform spatial stride on
  point-dominated tiles); `--feature-order` then orders the survivors.

- **The name is the key the tile advertises**, not the schema column name. A
  source column renamed on the way in to clear a reserved overview name is
  restored on export (see [Reserved column names](#reserved-column-names)), so
  sort by the *source* name: `--feature-order level` is right for an input with
  its own `level` column, even though the overview file stores it as `level_`.
  A column that stays renamed — because the reserved name is genuinely
  published, as `point_count` is under `--cluster` — has to be named as it
  appears in the tile.
- **Naming a column the layer does not publish warns and changes nothing.**
  Every feature is then equally unranked, so the output is input order — which
  on its own looks exactly like success. The run lists the available property
  names so a typo is obvious.

`--feature-order input` is the default and costs nothing; naming a column adds
one stable sort per tile over features already in memory.

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

**The value is a request, not a guarantee (#507).** A Parquet file holds at
most 32,768 row groups (the row-group ordinal is an `i16`), and a very low
`--row-group-size` on a planet-scale input can project past that. Before pass 2
opens the output file, the converter projects the total row-group count from
pass 1's per-level winner counts and compares it against a 32,000-group
preflight ceiling (headroom, because that projection is a pre-simplification
upper bound rather than an exact count). If the projection is over, the cap is
**auto-scaled up** to the smallest clean value that fits, with a `warn`-level
log line naming the old and new values. The cap actually used is recorded as
`effective_max_row_group_size` in `--report` JSON and printed as a summary
note, so the raise is visible after the run rather than only in the log.

A raised cap costs memory: the writer holds a whole row group in RAM before
flushing it, so peak write memory scales with the cap. If that matters more
than the file layout, cut the projected row-group count at the source
instead — fewer levels (`--min-zoom` / `--max-zoom`), or
`--row-group-size-policy zoom-scaled` (below), which already gives coarse
bands far larger caps.

### `--row-group-size-policy` (default `constant`): per-level cap scaling

Controls how the `--row-group-size` cap is applied across levels (#202):

- **`constant`** (default): every level uses the same cap.
- **`zoom-scaled`**: the cap doubles for each zoom step *below* the finest
  level (`cap = row-group-size << (max_zoom − level_zoom)`). Coarse bands —
  which a wide viewport reads mostly whole anyway — collapse into fewer, larger
  row groups (fewer remote requests), while the finest level keeps tight
  per-row-group bbox pruning.

Reach for `zoom-scaled` when a deep pyramid's coarse levels fragment into many
tiny row groups and bloat the footer; leave it `constant` otherwise. See
`corpus/SWEEPS.md` (Decision 5) for the rationale.

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
2. **Pass 2** reads the input **once more** and fans each read batch out to
   *all* levels at once (the single-read pipelined engine, #213): a reader
   thread streams batches over a bounded channel while a consumer parallelizes
   every `(level × feature)` simplification across cores, and each level's
   output drains to the writer in level order. The finest (canonical) level is
   verbatim and streamed to the writer last. An older engine that re-read the
   input once *per level* survives only as the equivalence-tested reference
   path — see [Performance profiles](#performance-profiles---profile---in-flight-batches---read-workers).

Peak memory is `O(read batch + winner tables)` instead of `O(dataset)`: on the
Moldova corpus file (632k polygons, 38M vertices) peak RSS drops from ~5.4 GB
(in-memory) to well under 1 GB, with equivalent output.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--read-batch-size N` | `8192` | rows per read batch (max 1048576) | **bigger = faster-ish, more memory** |
| `--no-streaming` | off | flag | revert to the one-pass in-memory pipeline |
| `--profile speed\|bounded\|auto` | `auto` | preset | speed vs bounded RAM — see [Performance profiles](#performance-profiles---profile---in-flight-batches---read-workers) |
| `--in-flight-batches N\|auto` | `auto` | read batches in flight (auto = cores, clamped 4–16) | read/compute overlap — see [Performance profiles](#performance-profiles---profile---in-flight-batches---read-workers) |
| `--read-workers N\|auto` | `auto` | pass-2 reader threads (auto = cores/4, capped at 4; explicit capped at 2× cores) | parallel input read — see [Performance profiles](#performance-profiles---profile---in-flight-batches---read-workers) |

**`--read-batch-size`** bounds the transient working set of both passes: each
batch is decoded, filtered, simplified, and written before the next is read.
LARGER batches amortize per-batch overhead (marginally faster) at the cost of
proportionally more peak memory; SMALLER batches bound memory tighter. The
default 8192 keeps per-batch transients in the tens of MB even for
vertex-heavy polygon data; you rarely need to change it. LOWER it (e.g. 1024)
on very memory-constrained machines or for monster geometries (a single batch
of coastline-sized multipolygons can be large); RAISE it (e.g. 65536) only if
profiling shows per-batch overhead dominating on a machine with RAM to spare.
It also sets how finely pass 1 fans out: each batch is split across the rayon
pool into chunks of `read_batch_size / threads` rows (clamped to 256..=1024),
so lowering it for memory costs some pass-1 parallelism but never disables it.

**`--no-streaming`** runs the original in-memory pipeline: the whole table and
every decoded geometry are held at once (`O(dataset)` memory). It decodes each
geometry only once — the streaming default re-decodes the winners in pass 2 —
so it can be marginally faster on *small* inputs that comfortably fit in RAM;
on large inputs it is both slower and enormously more memory-hungry. It is kept as the reference
implementation — the two paths are equivalence-tested against each other —
and as an escape hatch; there is no output-quality reason to use it.

Residual per-feature memory in streaming mode (the "winner tables") is ~50–80
bytes per input feature during pass 1 and 1 byte per feature during pass 2 —
about 40 MB / 0.6 MB for a 632k-feature file — so even planet-tier inputs
stay laptop-sized. The pass-1 side of that is the `AssignFeature` table:
exactly 64 bytes/row (`size_of::<AssignFeature>()`), held resident from pass
1's scan through the level assignment — the coarse job's irreducible memory
floor on a [sharded build](diving-deeper/sharded-builds.md#sizing-the-coarse-jobs-memory),
where it can reach tens of GiB at billion-row scale. tylertoo preflights this
from footer row counts before pass 1 scans anything (#543): it warns above
~85% of the process's memory limit and hard-errors over it, naming the
`TYLERTOO_SKIP_MEMORY_PREFLIGHT=1` escape hatch for setups where the estimate
doesn't apply.

---

## Regional extract: `--bbox`

`--bbox xmin,ymin,xmax,ymax` (lon/lat degrees, EPSG:4326) converts only the
features whose bounding box intersects the given region — useful for extracting
a city from a country-wide file without processing the whole dataset.

```bash
# Tile just Antananarivo from a Madagascar-wide file
tylertoo overview madagascar.parquet antananarivo.parquet \
  --bbox 47.4,-19.0,47.6,-18.8 \
  --min-zoom 0 --max-zoom 14
```

The filter is two-stage:

1. **Row-group pruning** (statistics-based): row groups whose GeoParquet 1.1
   bbox covering column statistics don't intersect the region are skipped at
   the parquet footer level — their data pages are never read. This is the
   big win: a gpio-optimized (Hilbert-sorted) file typically reduces I/O by
   90%+ for small regional extracts.

2. **Exact feature filter**: features of the surviving row groups whose own
   bbox misses the region are dropped exactly. This guarantees correctness
   even when the input lacks covering statistics.

**Graceful degradation**: if the input has no covering column or its statistics
are unavailable (non-gpio-optimized, GDAL without covering, etc.), all row
groups are read and only the exact per-feature filter applies — the output is
identical, just slower. Check `ConvertReport.row_groups_read` vs
`row_groups_total` to see whether pruning fired.

| Knob | Default | Units | Effect |
|------|---------|-------|--------|
| `--bbox xmin,ymin,xmax,ymax` | — | lon/lat degrees | only features intersecting this region; omit = full extent |

**Tip**: for EPSG:3857 inputs, still pass lon/lat degrees — the converter
reprojects the bbox internally.

---

## Attribute filter: `--filter` / `--where`

`--filter <EXPR>` (aliased as `--where`) converts only the features matching a
SQL-WHERE-style predicate over the input's property columns — the attribute
analogue of `--bbox`, and it composes with it. Available on both `overview`
and the one-shot `tiles` facade.

```bash
# Only high-confidence field boundaries, straight from the source file
tylertoo tiles fields.parquet fields.pmtiles \
  --filter "confidence > 0.8" --max-zoom 14

# Composes with --bbox and richer predicates
tylertoo overview brazil.parquet subset.parquet \
  --bbox -48.0,-16.0,-47.0,-15.0 \
  --where "crop_type IN ('soy', 'corn') AND confidence >= 0.5"
```

**Expression language** (small built-in recursive-descent parser — no SQL
engine involved):

- Comparisons: `=` (or `==`), `!=` (or `<>`), `<`, `<=`, `>`, `>=`, with the
  column on the left: `confidence > 0.8`, `country = 'BRA'`, `active = true`.
- Membership: `col IN (v1, v2, ...)`, `col NOT IN (...)`.
- Null tests: `col IS NULL`, `col IS NOT NULL`.
- Boolean composition: `AND`, `OR`, `NOT`, parentheses. `OR` binds loosest,
  then `AND`, then `NOT`.
- Literals: numbers (`0.8`, `-2`, `1e6`), single-quoted strings (`'it''s'`
  escapes a quote), `TRUE`/`FALSE`. Keywords are case-insensitive.
- Columns: bare identifiers, or `"double quoted"` for names with spaces.
  Supported column types: numeric (int/uint/float), string, boolean,
  timestamp.
- Timestamp columns compare against single-quoted datetime strings:
  `time >= '2025-01-01'`, `time < '2025-06-01 12:30:00'`, or full RFC 3339
  with an offset. Timezone-less literals are read as UTC (Arrow timestamps
  store a UTC instant regardless of the column's display timezone), and a
  malformed datetime errors at startup, not mid-run. Statistics pushdown
  works when the file stores timestamps as INT64 (modern writers); legacy
  INT96 timestamps (older Spark) expose no statistics, so those predicates
  filter exactly but prune nothing.

**Null semantics** are SQL three-valued logic: a comparison or `IN` over a
NULL value is UNKNOWN, `AND`/`OR`/`NOT` combine with Kleene logic, and a row
is kept only when the whole predicate is TRUE. So `confidence > 0.8` drops
null-confidence rows — and so does `NOT (confidence > 0.8)`. Use
`IS NULL` / `IS NOT NULL` to test nulls explicitly. A NaN in a float column is
UNKNOWN too — no comparison against it has an answer, and it is nodata far more
often than it is a value. Infinities are ordinary values and compare normally.

Note that this also makes `!=` (and `NOT IN`) drop NaN rows: `NaN != 5` is TRUE
under IEEE-754, but UNKNOWN here, which is the SQL reading. And unlike a null,
a NaN is a *present* value, so `IS NULL` does **not** match it while
`IS NOT NULL` does. The two rules together mean **no predicate selects NaN
rows**: they can only be kept by a predicate over some other column. Clean the
column upstream (e.g. with `gpio`) if you need to address those rows.

Like `--bbox`, the filter is two-stage:

1. **Row-group pruning** (statistics-based): row groups whose parquet column
   chunk statistics (min/max/null-count) prove the predicate cannot match are
   skipped at the footer level — their data pages are never read, and on
   remote input those byte ranges are never fetched. `AND` intersects the
   prunable sets, `OR` unions them; `NOT (...)` subtrees and columns without
   usable statistics conservatively keep the row group.
2. **Exact per-row evaluation** during the pass-1 scan guarantees identical
   output whether or not pruning fired.

| Knob | Default | Units | Effect |
|------|---------|-------|--------|
| `--filter EXPR` (alias `--where`) | — | predicate | only features where EXPR is TRUE; omit = no filtering |

**Interactions**: the filter runs before level assignment, ranking, density
budgets, clustering, and coalescing — dropped features simply never enter the
pipeline, exactly as if the input had been pre-filtered (`ConvertReport.
input_features` counts only survivors). Check `row_groups_read` vs
`row_groups_total` to see whether statistics pruning fired; sorting the input
by a filtered column (or lowering `--row-group-size`) tightens per-row-group
statistics and prunes more.

---

## Property selection: `--include-property` / `--exclude-property` / `--exclude-all-properties`

Which attribute columns the output carries — tippecanoe's `-y` / `-x` / `-X`.
Everything not selected is dropped **at scan time** on `overview` and `tiles`:
the excluded columns are not decoded, the intermediate overview file only
carries the kept columns, and the tiles' per-feature bytes shrink
accordingly (which matters for the `--max-tile-size` valve — a tile that has
to shed features to fit its byte budget sheds fewer when it is not carrying
eight columns nobody asked for). The geometry column is always kept.

```bash
# Tiles carry only confidence and metrics:area
tylertoo tiles fields.parquet fields.pmtiles --max-zoom 13 \
  --include-property confidence --include-property metrics:area

# Everything except the id and the timestamp
tylertoo overview fields.parquet ov.parquet --exclude-property id \
  --exclude-property "determination:datetime"

# Geometry only
tylertoo tiles fields.parquet outlines.pmtiles --exclude-all-properties
```

| Knob | Default | Semantics |
|------|---------|-----------|
| `--include-property COL` (repeatable) | all | keep ONLY these; naming a column the input lacks is an error |
| `--exclude-property COL` (repeatable) | none | drop these; an unknown name only warns; ignored when `--include-property` is given |
| `--exclude-all-properties` | off | geometry-only output; ignored when `--include-property` is given |

The three combine as tippecanoe's do: an include list is the whole answer and
the exclude flags are then ignored (tippecanoe's `-y` implies `-X`, and its
attribute filter consults only the include set once one exists), so
`--exclude-all-properties --include-property foo` keeps `foo`, and
`--include-property a --include-property b --exclude-property b` keeps both.
Without an include list, `--exclude-all-properties` keeps nothing and
`--exclude-property` drops what it names.

**Interactions**: a column another knob reads — `--sort-key`, `--class-rank`,
`--magnitude-ladder` / `--entry-zoom`, `--accumulate-attribute`, `--filter` —
must stay included; excluding it is rejected with the knob's name, because
the knobs evaluate over the projected batches and would otherwise go silently
inert. `export-pmtiles` takes the same three flags and applies them to the
tiles only, matched on the property names the tiles publish (the overview
file is untouched); there, the `--feature-order` column is the one that must
stay — and `tiles` rejects the same pairing up front, since its selection is
applied at convert and the column would be gone before export could sort on
it. An explicit `--include-property coalesced_count` on export wins over the
1-everywhere withholding (#379): the column is in the file, and naming it is
not a typo.

---

## Performance profiles: `--profile`, `--in-flight-batches`, `--read-workers`

Like the [memory / streaming knobs](#memory--streaming-knobs---no-streaming---read-batch-size),
these never change the output's content: **the produced file is byte-identical
across every profile, `--in-flight-batches` value, `--read-workers` value,
and thread count.** They control only how fast the conversion runs and how
much memory it uses while running.

The rewritten pass-2 engine reads the input Parquet **once** and pipelines
Parquet read/decode with per-feature simplification fanned out across **all
cores**. This replaces the old pass 2, which re-read the whole input once per
level (15 reads on a 15-level plan) and simplified effectively on a single
core. The win is largest on vertex-heavy duplicating-mode conversions and on
partitioning mode, which previously spent ~73% of wall time re-reading the
input (#212 / #213).

The one remaining choice is **where each output level's rows live between the
compute stage and the write stage**:

- **`speed`** buffers each level's rows in RAM, then writes them. Fastest — no
  temp I/O — but peak RAM grows with total *output* size. Best when the output
  comfortably fits: duplicating mode, or partitioning on inputs well under
  available RAM.
- **`bounded`** spills each level's rows to temporary **Arrow IPC** files and
  streams them back at write time, capping peak RAM regardless of output size.
  Slightly slower (temp read/write) but memory-safe for very large / wide
  inputs. Best for partitioning mode on multi-GB inputs, or memory-constrained
  machines.
- **`auto`** (default) picks from the workload: it estimates the buffered
  output as `buffered rows × per-row cost` and spills (`bounded`) when the
  estimate exceeds a fraction (0.6) of *available* RAM, keeping RAM (`speed`)
  otherwise. The per-row cost is derived from pass 1's measured average
  encoded-geometry size for **this input** (geometry weight dominates a
  buffered row and varies ~400× across datasets — points vs. boundary
  polygons), with a deliberate high bias toward the near-free spill path;
  calibrated constants (~8 KiB/row duplicating, ~16 KiB/row partitioning)
  are the fallback when nothing was scanned. Partitioning additionally keeps
  its historical 2M-buffered-row spill floor. The decision (measured average,
  estimate, budget) is logged; `TYLERTOO_AUTO_MEM_LIMIT_BYTES` overrides the
  detected available RAM. The probe is **container-aware** (#481): cgroup v2
  (`memory.max` and `memory.high`, minimum along the cgroup path) and cgroup v1
  (`memory.limit_in_bytes`) limits are respected, the binding cgroup's
  non-reclaimable current usage is subtracted so the figure is *headroom*
  rather than a ceiling the run may already be sitting near, and the budget is
  `min(cgroup headroom, machine available)` — so a Slurm / Docker / k8s job
  inside a 160 GiB cgroup on a 2 TB node no longer sizes against the node.
  When the cgroup limit is the binding constraint, one info line says so
  (`memory budget from cgroup limit: N GiB (machine has M GiB)`).
  Note that `bounded` (and an `auto` that picks it) spills to the process temp
  directory, which on many Slurm and Kubernetes nodes is a tmpfs `/tmp` — RAM
  charged to the very same cgroup, so the spill does not relieve the limit.
  Point `TMPDIR` (or `--spill-dir`) at real disk there.

The profile also governs the **pass-1 level-assignment grids** (#306). Level
assignment builds one cell-winner grid per coarse level, concurrently across
cores (#264); on large simple-geometry layers the concurrently-live grids are
the whole-convert RSS peak (germany-segments: 5.9 GiB; ~24 GiB at Brazil
scale — see the `[rss]` phase logs). Under `bounded` and `auto` the grids'
footprints are estimated up front and the levels are built in
**memory-budgeted waves** against the same fraction-of-available-RAM budget
(and the same `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override): when the estimate
exceeds the budget, levels are grouped so only one wave's grids are live at a
time — trading cross-level parallelism for a bounded peak, degrading in the
limit to a serial per-level build rather than an OOM. A one-line
`[assign] winner grids …` log reports the split when it happens; on a roomy
box the plan is a single wave and nothing changes. `speed` opts out
(unbounded grids, maximum parallelism). As with everything in this section,
the wave schedule never changes the output.

| Knob | Default | Units | Direction |
|------|---------|-------|-----------|
| `--profile speed\|bounded\|auto` | `auto` | preset | `speed` = fastest, most RAM; `bounded` = capped RAM, temp I/O |
| `--in-flight-batches N` | `4` | read batches in flight | **bigger = more overlap + core use, more memory** |
| `--read-workers N\|auto` | `auto` | pass-2 reader threads | **more = more read throughput, more resident batches** |

**`--in-flight-batches`** is the primary read/compute-overlap knob: it sets how
many Arrow read batches may be moving through the pipeline at once (the
bounded-channel depth) — both passes use it: pass 1's chunked scan and pass 2's
per-level fan-out. RAISE it for more read/compute overlap and better core
utilization when a few long-pole geometries otherwise stall the pipeline; each
extra in-flight batch costs proportionally more peak RAM (`N × read_batch_size`
rows resident, per pass — passes 1 and 2 never run concurrently, so this does
not double). `--read-batch-size` (above) remains the rows-per-batch knob;
`--in-flight-batches` is how many such batches coexist. Since #494 it is not
the only resident-batch term: pass 2's readers hold `--read-workers` × their
queue depth on top of it, and under `bounded` each level's spill writer holds
up to three more (two queued plus the one it is encoding).

**`--read-workers`** splits the pass-2 *read itself* across threads (#494).
Parquet row groups are independently readable, so several threads can decode
disjoint runs of them at once; an in-order merge then reassembles the batches
into exactly the sequence a single reader would have produced, which is why the
output is byte-identical for every value (`crates/cli/tests/thread_count_determinism.rs`
checks `1` against `2` and `4` directly). `auto` takes a quarter of the
machine's cores, capped at 4 — a reader thread decompresses and decodes, so it
competes with the pool doing the simplification it is feeding, and past a
handful of streams the device queue is saturated anyway. An explicit value is
honoured up to a ceiling of **2× the machine's cores** (at least 4); above that
the CLI rejects it and the library clamps with a warning, because every worker
is a thread plus its own read-ahead queue.

Two things bound the win, both worth knowing before reaching for a bigger
number:

- **Row-group size.** A worker buffers its run ahead of the merge, so a run has
  to fit in that worker's queue; a row group larger than the queue makes the
  worker park mid-run and the reads serialize again (a `[convert] pass 2 read:`
  debug line says so when it happens). Inputs written by `gpio` have
  well-sized row groups; a single-row-group file cannot be split at all and
  reads sequentially whatever you ask for.
- **The memory budget.** The read-ahead is sized against the same
  fraction-of-available-RAM budget the pass-2 sink uses (10% of it), so a small
  box, or a wide/vertex-heavy input, quietly gets fewer workers rather than a
  larger resident set. Under an explicit `--profile bounded` a worker's queue
  is additionally capped at half its usual maximum depth.

  That bound is a **model**, not a measurement: the budget is charged
  `workers × depth × read_batch_size × per_row`, and `per_row` is the sink's
  own estimate — a fixed ~4 KiB non-geometry term plus twice pass 1's measured
  per-row geometry bytes. A schema whose non-geometry columns cost far more
  than that term is priced too cheaply and the read-ahead can exceed its
  nominal share. On a run sized to the last hundred MB, measure; on a wide
  schema, `--read-workers 1` removes the term entirely.

Remote inputs always read sequentially: a remote source's parts share one
in-memory chunk cache that concurrent readers would evict out from under each
other. **Staging does not change this.** A remote convert's pass-0 staging
fills that same per-part cache and its disk spill; it does not turn the source
into a local one, so the pass-2 read stays sequential for the whole run. If you
want the parallel read on remote data, land the file locally yourself (`gpio`,
`aws s3 cp`) and convert from the local path.

⚠️ **musl binaries v0.7.1 and earlier — prefer `--profile bounded` on large
duplicating runs.** The `x86_64-unknown-linux-musl` release binary up to and
including v0.7.1 could abort with `memory allocation of N bytes failed` part
way through pass 2 of a `speed`/`auto` run — musl's `mallocng` failing a small
allocation under heavy multi-threaded fragmentation while tens of GB of
headroom remained ([#480](https://github.com/geoparquet-io/tylertoo/issues/480)).
`--profile bounded` avoids the in-RAM level buffering that triggers it. From
the release after v0.7.1 the musl binary ships with mimalloc as its global
allocator and no longer needs the workaround; other platforms were never
affected.

⚠️ **Interaction — `speed` + partitioning on a multi-GB input is the
memory-risky quadrant.** `speed` buffers whole output levels in RAM, and
partitioning's output can approach input size; on a multi-GB input that can
exhaust memory. `auto` (the default) already routes partitioning and any
over-budget run to `bounded` — only an explicit `--profile speed` overrides
that. If you force `speed` on a large partitioning run, watch peak RSS.

---

## Reusing a plan: `--save-plan`, `--plan`

The streaming pipeline runs in two passes. **Pass 1** streams the whole input
to decide which level every row enters at — the *winner table* — and then
runs the level assignment and the density budget over it. **Pass 2** writes.
On a large input, pass 1 plus the assignment is most of the wall time, and it
depends on nothing the write side does.

`--save-plan PATH` persists that result; `--plan PATH` replays it instead of
recomputing it.

```bash
# Once: scan, assign, and keep the plan.
tylertoo overview roads.parquet roads.parquet.overview \
  --min-zoom 0 --max-zoom 14 --save-plan roads.plan

# Again, tuning only the write side — pass 1 and the assignment are skipped.
tylertoo overview roads.parquet roads-tuned.overview \
  --min-zoom 0 --max-zoom 14 --plan roads.plan \
  --profile bounded --row-group-size 50000
```

The artifact is a magic + checksum header followed by one Arrow IPC file,
sized by input **rows** rather than input bytes: one winner byte per row plus
the small per-level side tables. A 28 MB / 24k-feature polygon file yields a
49 KB plan. Line coalescing is the exception — the plan carries the collected
line geometries as WKB, so a line-heavy input produces a proportionally
larger artifact.

**The file is checksummed.** A plan is meant to be copied to other machines,
so `--plan` verifies an xxh3-64 over the whole payload before decoding any of
it. A truncated, edited, or bit-rotted plan is one named error —

```
--plan /mnt/shared/roads.plan: is corrupt: the payload hashes to
1f3c...  but the header records 90ab.... The file was truncated, edited, or
damaged in transit — re-create it with --save-plan.
```

— rather than an arrow-internal panic. Every other structural problem (a
plan that is not a plan, a foreign format version, an impossible level count
or cluster stride, an unknown geometry-kind code, row-indexed sections of
disagreeing length) is likewise an error naming `--plan` and the path.

The checksum proves the file is the one that was written; it proves nothing
about whether the file makes sense. So the plan's row domain is also compared
against the rows this run will actually read — the sum over the row groups
`--bbox` / `--filter` selected — every time, pruned or not.

**The plan is verified, not trusted.** It stores a fingerprint: the tylertoo
version, every thinning-relevant flag, and each input's identity. A mismatch
is a hard error naming the field:

```
--plan: saved plan does not match this run: input "roads.parquet" mtime was
"1790242802390408452" when the plan was saved but is "1790244114398193000"
now. Re-run without --plan (add --save-plan to write a fresh one).
```

**What "input identity" pins, exactly.** For every part — local file or
remote object alike — the path/URL, the byte size, the **row count** and the
**row-group count** read from the parquet footer, plus the row groups
`--bbox`/`--filter` pruned to. A **local** part additionally pins its mtime.

| | local file | remote object |
|---|---|---|
| path / URL | ✅ | ✅ |
| byte size | ✅ (`stat`) | ✅ (Content-Length) |
| row count | ✅ (footer) | ✅ (footer) |
| row-group count + pruned selection | ✅ | ✅ |
| mtime | ✅ | ❌ |
| content hash / ETag | ❌ | ❌ |

The row count is the load-bearing one: the winner table is one byte per input
row, addressed by row *position*, so an input swapped under a saved plan
either produces a silently wrong pyramid (fewer rows) or indexes out of
bounds (more rows). Pinning the footer row count turns both into a named
error, for remote inputs as well as local ones. tylertoo prints a one-line
warning naming what is and is not pinned whenever a part is remote.

Neither a local mtime+size nor a remote size+row-count is a content hash:
`cp -p` / `rsync -a` preserve mtime and size, and an object can be rewritten
in place with the same size and row count. Treat the fingerprint as a strong
staleness check, not a cryptographic seal.

Deliberately **not** fingerprinted, so one plan replays across them:
`--profile`, `--row-group-size`, `--row-group-size-policy`,
`--full-column-stats`, `--read-batch-size`, `--in-flight-batches`,
`--spill-dir`, `--cogp-compat`. Everything that changes *which* features land
at *which* level is fingerprinted, and output with `--plan` is byte-identical
to the run that saved it.

⚠️ **Why this matters for sharded builds.** The assignment is not a
per-feature function: the density budget water-fills a 128 × GSD super-cell
budget over *every* candidate of a level, the level walk carries a running
kept count coarse → fine, `--magnitude-ladder` dense-ranks the *global*
distinct values of its column, and the automatic class ranking picks its
column from a global vocabulary scan. A shard that recomputed the assignment
over its own subset would reach a different answer, and the shards' pyramids
would disagree. Compute the assignment once, save the plan, and give every
shard the same one.

That is exactly what `tiles --shard` does, and it refuses to run without
`--plan` rather than let the mistake through. A shard also *narrows* the
plan's row-group selection — it reads only the groups whose bbox reaches its
tile range — so the fingerprint compares that one term as a subset relation
and the plan's row-indexed tables are re-addressed onto the shard's shorter
row stream before pass 2. See [Sharded builds across a
fleet](diving-deeper/sharded-builds.md) for the whole recipe.

Both flags require the streaming pipeline (they are its pass-1 stage), they
are mutually exclusive, and they are available on `overview` and `tiles`.

**Both paths are checked before anything is scanned.** `--save-plan` is
written only once pass 1 *and* the assignment are complete, so pointing it at
an unmounted volume used to cost the entire scan and then leave you with no
plan **and** no overview. Its parent directory must therefore exist and be
writable at option-validation time — and if a plan is already sitting at that
path, it must itself be writable, since it is about to be clobbered. `--plan`
must be a readable convert plan (magic bytes checked, with a future format
version named as such) — same fail-fast contract as `--spill-dir`.
An existing plan at `--save-plan PATH` is **overwritten**, with a log line;
that matches how `overview` and `tiles` treat their own outputs (only
`pyramid`, which merges several archives, gates overwrites behind
`-f/--force`).
Not yet exposed in the Python bindings.

---

## Worked scenarios

| Symptom | Fix |
|---------|-----|
| Coarse roads look like sparse disconnected dashes | LOWER `--line-thinning` (e.g. 2 → 1) and/or `--line-visibility`; or RAISE `--gsd-base` |
| Coarse roads are all there but jagged / over-smoothed | LOWER `--simplify-factor` (e.g. 1.0 → 0.5) |
| Wrong roads survive (residential instead of highways) at coarse zoom | add `--class-rank road_class:…` or rely on auto-detect (don't pass `--no-auto-rank`) |
| Coarse level files are too large / slow | RAISE the thinning factors and/or `--simplify-factor`; or LOWER `--gsd-base` |
| Small buildings vanish too early | LOWER `--polygon-visibility`; or `--collapse` to keep them as points |
| Country-scale view of a dense building/parcel layer is empty | `--polygon-visibility 0 --collapse` + a circle layer for points ([dot-fill recipe](#country-scale-dot-fill-for-dense-polygon-layers)); the `500K` tile cap that keeps coarse dot tiles sane is on by default (#280) |
| Whole map uniformly too sparse or too dense | move `--gsd-base` (up = denser, down = sparser) instead of tuning each family |
| Mid zooms have far more features than tippecanoe / duplicating files too large | RAISE `--drop-rate` (density budget); or `--no-density-drop` to turn it off |
| Density cut is stripping sparse rural areas to keep cities | RAISE `--drop-gamma` (sparse-area protection) |
| Dense point data renders as a misleadingly sparse dot field at coarse zooms | `--cluster` and style the symbol size by `point_count` |
| Need per-cluster totals/averages of a numeric column | `--accumulate-attribute col:sum` / `col:mean` (with `--cluster`) |
| Every remote query fetches a huge footer before any data | default already suppresses string/geometry stats; do NOT pass `--full-column-stats` |
| Need server-side row-group skipping on a property predicate | pass `--full-column-stats` (bigger footer, gains column pruning) |
| Tiny viewports over high-latency storage fetch too much | LOWER `--row-group-size` for tighter bbox pruning |
| Conversion runs out of memory / swaps on a big file | streaming is already the default; LOWER `--read-batch-size`; make sure `--no-streaming` is NOT set |
| Conversion is slow / uses only one core | the engine now reads the input once and parallelizes simplification across all cores; RAISE `--in-flight-batches` for more read/compute overlap |
| Conversion runs out of memory in `speed` profile | `--profile bounded` (spills each level to temp files, caps RAM); `--profile auto` picks this automatically for large partitioning runs |
| `memory allocation of N bytes failed` on a v0.7.1-or-earlier musl binary, with RAM to spare | `--profile bounded` — musl `mallocng` fragmentation, fixed after v0.7.1 by shipping mimalloc ([#480](https://github.com/geoparquet-io/tylertoo/issues/480)) |

See `corpus/SWEEPS.md` for an empirical `--line-thinning` ×
`--simplify-factor` sweep on Portland roads, and the Q2 section there for the
`--drop-rate` calibration (before/after ratios vs tippecanoe).
