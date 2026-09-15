# Results — Brazil 2025 field predictions, filtered from a 630 GiB global collection

A full-pyramid tiling run that starts from the **global** [Fields of The
World](https://fieldsofthe.world/) predictions collection on
[Source Cooperative](https://source.coop/) — **629.6 GiB of GeoParquet, 8.2
billion rows in 1,000 Spark part files** — and produces a **z0–14 PMTiles
pyramid of Brazil's 2025 growing-season field predictions** in one native
workflow: remote read, attribute + time filter, spatial subset, centroid
low-zoom band, polygon high-zoom band.

The previous demo tiled a curated per-country file set. This one showcases the
harder, more realistic shape of the problem: the data you want is a **slice of
a huge cloud-native collection** — one class (`label = 'field'`), one vintage
(the 2025 prediction run), one country's extent — and no curated extract
exists. tylertoo carves the slice and builds the pyramid in a single command
pair, without downloading the collection or materializing an intermediate.

## Setup

- **Input:** the FTW predictions `results/` collection — 1,000
  `part-*.snappy.parquet` files (629.6 GiB, 8,217,195,679 rows) written by
  Spark. Columns: `geometry`, `time` (prediction vintage; INT96 timestamp),
  `label` (`field` / `field_boundaries` / `non_field_background`), `bbox`.
  52 files intersect the Brazil bbox; those URLs are listed in
  [`brazil-2025-manifest.txt`](./brazil-2025-manifest.txt).
- **Machine:** 16 cores, 54 GiB RAM.
- **Build:** tylertoo 0.6.0 (release) + this PR's timestamp-filter and
  null-CRS support.
- **Workflow:** `overview` (build the multi-resolution overview straight from
  the remote slice) → `export-pmtiles`.

Commands:

```bash
# 1. Multi-resolution overview from the remote collection slice:
#    Brazil bbox, 2025 vintage, fields only. Dots z0-5; from z6 each field
#    stays a representative point until it is a >=1px polygon
#    (--collapse --polygon-visibility 0), and --simplify-factor 4 aligns that
#    dot->polygon handoff with pixel-visibility so nothing disappears mid-zoom.
tylertoo overview \
  --files-from brazil-2025-manifest.txt \
  --bbox="-74.1,-34.0,-34.7,5.4" \
  --filter "label = 'field' AND time >= '2025-01-01'" \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-5:point,6-14:geom" \
  --collapse --polygon-visibility 0 --simplify-factor 4 \
  --report convert-report.json \
  brazil-2025-fields-ov-z14.parquet

# 2. Export the PMTiles archive.
tylertoo export-pmtiles brazil-2025-fields-ov-z14.parquet \
  brazil-2025-fields.pmtiles \
  --report export-report.json
```

## Numbers

| stage | wall | peak RSS | output |
|---|---|---|---|
| **convert** (remote read + filter of 40.7 GiB) | 1 h 11 m 56 s | 9.6 GiB | 43.9 M features filtered *during* the read → tuned overview (`brazil-2025-fields-ov-z14.parquet` — 9.9 GB, 110.7 M rows × 15 levels) |
| **export-pmtiles** | 10 m 52 s | **1.56 GiB** | `brazil-2025-fields.pmtiles` — **3.4 GiB**, 1,649,201 tiles |
| **total** | — | — | z0–14, 117,659,919 tile-features, 0 oversized |

> **Tuning note.** These figures are for the tuned representation (dots z0–5;
> from z6 each field stays a dot until it is a ≥1 px polygon), which fixes a
> mid-zoom disappearance in the first cut of this demo (see *Monotonic
> representation* below). The `convert` row is the original measured remote run
> that carved the 43.9 M-feature slice; because Source Cooperative was
> intermittently closing connections during the update, the tuned tiling was
> regenerated from that run's canonical (verbatim) geometry — output-identical
> to re-running the remote command with the tuned flags. `export` and the
> per-level counts below are from the tuned run.

Convert detail:

- **Selectivity:** 426,316,880 rows scanned in the 52 covering files →
  **43,903,999 features kept** (10.3%) by
  `label = 'field' AND time >= '2025-01-01'` + the Brazil bbox. The other
  ~382 M rows are the 2024 vintage, the `field_boundaries` /
  `non_field_background` classes, and out-of-bbox neighbors.
- **Remote read:** 457 range requests, 43.75 GB fetched (≈1.0× the 52
  objects' 43.75 GB — the local spill keeps passes 2+ off the network). The
  other 948 part files (589 GiB) were never touched.
- **Convert wall breakdown:** 43 m staging download (network-bound at
  ~16 MB/s) + 28 m 43 s scan/assign/write.

### Per-level overview breakdown

The tuned representation makes z0–5 an explicit **dot** band and z6–14 a
collapse-filled polygon band: within z6–14 each field is written as a
representative **point** until it grows to a ≥1 px polygon
(`--collapse --polygon-visibility 0 --simplify-factor 4`). Dots and polygons
therefore coexist through the mid zooms, and the per-level count rises
monotonically — no field ever disappears as you zoom in:

```
level  zoom  features         dots      polygons
    0     0        634          634             0
    1     1      2,295        2,295             0
    2     2      8,053        8,053             0
    3     3     28,456       28,456             0
    4     4     99,797       99,797             0
    5     5    335,189      335,189             0
    6     6    799,158      781,050        18,108
    7     7  1,318,610    1,212,990       105,620
    8     8  2,175,707    1,748,444       427,263
    9     9  3,589,917    2,460,828     1,129,089
   10    10  5,923,362    3,426,984     2,496,378
   11    11  9,773,548    4,383,733     5,389,815
   12    12 16,126,354    4,939,065    11,187,289
   13    13 26,608,484    4,570,385    22,038,099
   14    14 43,903,999            0    43,903,999   (canonical, verbatim)
```

Every written polygon is ≥1 px at its level (0 sub-pixel polygons at every
zoom), so the *visible* feature count equals the total at every level. The dot
population peaks around z11–z12 and then recedes as fields become large enough
to render as polygons — the smooth per-field dot→polygon handoff. z0 still
renders 634 dots where the original polygon-only demo's z0 was empty.

### Monotonic representation (the fix)

The first cut of this demo used `--representation "0-7:point"`: dots z0–7, then
a hard switch to polygons at z8. Because a typical field is ~150 m across and
does not become a ≥1 px polygon until ~z10–z11, that switch dropped medium
fields at z8 (the visibility gate culled everything below ~306 m) and they only
reappeared several zooms deeper — "fields go away when you zoom in, then come
back." The tuned config removes the cliff two ways: `--polygon-visibility 0`
keeps every field eligible (nothing is gated out), and `--collapse` +
`--simplify-factor 4` keep a field as a visible dot until it is a ≥1 px polygon.
A field's lifecycle is therefore *absent → dot → polygon → verbatim*, never
reversing.

**Fine-zoom dots and the LOD style.** Brazil's fields are small — median bbox
diagonal ≈ 53 m — so a large share stay **sub-pixel** (a field is only a ≥1 px
polygon once it clears ~38 m at z12, ~76 m at z11). Keeping every field visible
across zoom (no disappearance) therefore *requires* those still-sub-pixel fields
to render as dots well into the mid zooms — ~30 % of z12 features are such dots.
That is correct in the tiles (each becomes a polygon by z13–z14), but a constant
dot symbol would swamp the fine-zoom view, so the demo style **tapers the dot
size and opacity from ~z10** — the polygons (fields past 1 px) become the map and
the remaining tiny-field dots recede to a faint stipple. This is presentation
only; the tiles are unchanged.

## What the numbers say

**1. The filter is the feature.** No curated "Brazil 2025 fields" file
exists. The slice lives inside a 630 GiB, 8.2-billion-row global collection,
interleaved with two other classes and a second vintage. `--filter` (new in
#321, timestamp support added here) + `--bbox` carve it out *during* the
tiling read — no DuckDB pre-pass, no intermediate extract, no download of the
other 948 files.

**2. One archive serves dots and polygons, and nothing disappears.** A tuned
representation (`--representation "0-5:point,6-14:geom" --collapse
--polygon-visibility 0 --simplify-factor 4`) writes dots at z0–5 and, from z6,
keeps each field a dot until it is large enough to draw as a ≥1 px polygon.
Renderers get the graduated-dot overview and the parcel-level detail from the
same PMTiles file, with a two-line style split (`circle` for points, `fill` for
polygons), and the visible feature count rises at every zoom.

**3. Cloud-native end to end, at collection scale.** The read touched 6.9%
of the collection's bytes (43.75 GB of 630 GiB) — the 52 files whose footers
said they could contain Brazil. Row-group *pruning inside* those files did not
fire because the FTW files carry no GeoParquet 1.1 `covering` metadata (see
Findings); with it, the transfer would drop another ~18%.

**4. Bounded memory at every stage.** Convert auto-selected spill mode (its
in-RAM estimate for 109 M buffered output rows was ~315 GiB); peak RSS stayed
at 9.6 GiB against 54 GiB of RAM. Export streamed all 15 levels at a peak of
**1.56 GiB**.

## Findings (upstream + tooling)

Running this surfaced four things worth recording:

1. **`--filter` lacked timestamp support** — the `results/` collection keys
   vintage on a `time` timestamp column, which the filter (built in #321
   against numeric/string/boolean columns) could not compare. This PR adds
   timestamp columns (all four Arrow units, datetime string literals, exact
   i128 comparison, INT64-stats pushdown).
2. **Spark writes `"crs": null`** — per the GeoParquet spec an *omitted* crs
   defaults to OGC:CRS84 while an explicit *null* means "no CRS assigned",
   and tylertoo refused the file. Real writers (Spark/Sedona, hence FTW) emit
   null on plain lon/lat data, so tylertoo now assumes CRS84 with a warning
   when the declared bbox is plausible in degrees. *Upstream note:* FTW's
   writer should omit the key (or write the CRS84 PROJJSON) instead of null.
3. **No `covering` metadata → no row-group pruning.** The files carry tight
   per-row-group `bbox` column statistics that would let `--bbox` skip ~18%
   of the transfer at the parquet-footer level, but without the GeoParquet
   1.1 `covering` key tylertoo cannot trust the column and reads every row
   group of the covering files (the exact per-feature filter still applies).
   *Upstream note:* adding `covering` to the FTW geo metadata is a one-line
   writer change that would make every bbox consumer cheaper.
4. **INT96 timestamps defeat statistics pushdown.** Spark's legacy INT96
   encoding exposes no usable min/max, so the `time >= '2025-01-01'`
   predicate filters exactly but prunes nothing. INT64 timestamps (Spark
   `outputTimestampType=TIMESTAMP_MICROS`) would let the vintage predicate
   skip row groups too.

## Methodology

- Wall time: `/usr/bin/time -v` per stage; total is start-to-finish wall.
- Peak RSS: `time -v` maximum-RSS per stage.
- Filter correctness: the canonical level's 43,903,999 features were
  cross-checked against DuckDB
  (`label='field' AND year(time)=2025` + bbox predicate over the same 52
  files); a single-file smoke run matched DuckDB exactly (1,811,936 = 1,811,936).
- Tile / feature counts: the `export-pmtiles` JSON report and the PMTiles v3
  header.
- Clipping and simplification differ from tippecanoe by design (see
  [`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md)); this demonstrates
  a *pipeline and its output*, not byte-identical tiling.

The archive is hosted on Source Cooperative and rendered live on the docs
site — see [`docs/demo.md`](../docs/demo.md) and the viewer at
[`docs/demo/viewer.html`](../docs/demo/viewer.html).
