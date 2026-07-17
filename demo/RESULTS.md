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
#    Brazil bbox, 2025 vintage, fields only, centroids at z0-7.
tylertoo overview \
  --files-from brazil-2025-manifest.txt \
  --bbox="-74.1,-34.0,-34.7,5.4" \
  --filter "label = 'field' AND time >= '2025-01-01'" \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point" \
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
| **convert** (incl. 40.7 GiB remote read) | 1 h 11 m 56 s | 9.6 GiB | `brazil-2025-fields-ov-z14.parquet` — 14.1 GB, 43.9 M feats → 109.3 M rows × 15 levels |
| **export-pmtiles** | 11 m 44 s | **1.54 GiB** | `brazil-2025-fields.pmtiles` — **4.5 GiB**, 1,647,927 tiles |
| **total** | **1 h 23 m 40 s** | — | z0–14, 116,504,741 tile-features, 0 oversized |

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

`--representation "0-7:point"` makes z0–7 **representative points** (one
vertex per feature) and z8–14 full polygons — "dots zoomed out, polygons
zoomed in" in a single archive:

```
level  zoom  features     vertices       what
    0     0        634          634      points
    1     1      2,295        2,295      points
    2     2      8,053        8,053      points
    3     3     28,456       28,456      points
    4     4     99,797       99,797      points
    5     5    335,189      335,189      points
    6     6    799,158      799,158      points
    7     7  1,318,610    1,318,610      points
    8     8  1,672,801   19,355,924      polygons
    9     9  3,210,003   44,569,519      polygons
   10    10  5,655,496   96,611,166      polygons
   11    11  9,599,791  205,429,019      polygons
   12    12 16,045,516  455,049,658      polygons
   13    13 26,608,484 1,212,022,634     polygons
   14    14 43,903,999 1,580,669,610     polygons (canonical, verbatim)
```

A point is always visible, so the point band bypasses the polygon visibility
gate — z0 renders 634 dots where the previous polygon-only demo's z0 was
empty.

## What the numbers say

**1. The filter is the feature.** No curated "Brazil 2025 fields" file
exists. The slice lives inside a 630 GiB, 8.2-billion-row global collection,
interleaved with two other classes and a second vintage. `--filter` (new in
#321, timestamp support added here) + `--bbox` carve it out *during* the
tiling read — no DuckDB pre-pass, no intermediate extract, no download of the
other 948 files.

**2. One archive serves dots and polygons.** `--representation "0-7:point"`
(new in #322) writes centroids for the coarse band and polygons from z8 up.
Renderers get the graduated-dot overview and the parcel-level detail from the
same PMTiles file, with a two-line style split (`circle` layer for points,
`fill` for polygons).

**3. Cloud-native end to end, at collection scale.** The read touched 6.9%
of the collection's bytes (43.75 GB of 630 GiB) — the 52 files whose footers
said they could contain Brazil. Row-group *pruning inside* those files did not
fire because the FTW files carry no GeoParquet 1.1 `covering` metadata (see
Findings); with it, the transfer would drop another ~18%.

**4. Bounded memory at every stage.** Convert auto-selected spill mode (its
in-RAM estimate for 109 M buffered output rows was ~315 GiB); peak RSS stayed
at 9.6 GiB against 54 GiB of RAM. Export streamed all 15 levels at a peak of
**1.54 GiB**.

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
