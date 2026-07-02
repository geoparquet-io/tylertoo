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

## Worked scenarios

| Symptom | Fix |
|---------|-----|
| Coarse roads look like sparse disconnected dashes | LOWER `--line-thinning` (e.g. 2 → 1) and/or `--line-visibility`; or RAISE `--gsd-base` |
| Coarse roads are all there but jagged / over-smoothed | LOWER `--simplify-factor` (e.g. 1.0 → 0.5) |
| Wrong roads survive (residential instead of highways) at coarse zoom | add `--class-rank road_class:…` or rely on auto-detect (don't pass `--no-auto-rank`) |
| Coarse level files are too large / slow | RAISE the thinning factors and/or `--simplify-factor`; or LOWER `--gsd-base` |
| Small buildings vanish too early | LOWER `--polygon-visibility`; or `--collapse` to keep them as points |
| Whole map uniformly too sparse or too dense | move `--gsd-base` (up = denser, down = sparser) instead of tuning each family |

See `corpus/SWEEP_NOTES.md` for an empirical `--line-thinning` ×
`--simplify-factor` sweep on Portland roads.
