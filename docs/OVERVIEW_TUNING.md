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
| `--point-thinning` | `4.0` | × GSD (cell size) | **bigger = sparser** |
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

See `corpus/SWEEP_NOTES.md` for an empirical `--line-thinning` ×
`--simplify-factor` sweep on Portland roads, and the Q2 section there for the
`--drop-rate` calibration (before/after ratios vs tippecanoe).
