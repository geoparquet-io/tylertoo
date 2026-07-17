# Results — Brazil field boundaries, cloud-native GeoParquet → PMTiles

A full-country tiling run on **[Fields of The World](https://fieldsofthe.world/)
field boundaries for Brazil — 55,499,514 features** — read *directly* from
GeoParquet on [Source Cooperative](https://source.coop/) and tiled to a complete
**z1–14** PMTiles pyramid.

This is not a head-to-head speed table like the [Germany buildings
demo](../docs/demo.md). It is a **capability** result: there is no established
tool that produces this archive, because the standard vector tiler
(tippecanoe) does not read GeoParquet, and the documented workaround does not
survive this scale.

## Setup

- **Input:** 27 per-state GeoParquet files (`BR_AC.parquet` … `BR_TO.parquet`,
  6.1 GiB total) under the FTW predictions collection on Source Cooperative,
  read over HTTPS — nothing downloaded by hand first.
- **Machine:** 16 cores, 54 GiB RAM.
- **Build:** tylertoo 0.6.0 (release), `--partition-wave auto`.
- **Workflow:** the two-step overview path —
  `overview` (build a multi-resolution GeoParquet overview) →
  `export-pmtiles` (write the archive). The overview is a real, reusable
  artifact (18 GB GeoParquet), not a throwaway intermediate.

## Numbers

Full remote → PMTiles round trip: **44 min 41 s**, peak RSS **1.74 GiB** at
export, no OOM.

| stage | wall | peak RSS | output |
|---|---|---|---|
| **convert** (incl. 6.1 GiB remote read) | 23 m 25 s | 23.9 GiB¹ | `brazil-ov-z14.parquet` — 18 GB, 55.5M feats → 138M rows × 14 levels |
| **export-pmtiles** (`--partition-wave auto`) | 21 m 07 s | **1.74 GiB** | `brazil-field-boundaries.pmtiles` — **8.4 GiB**, 1,075,458 tiles |
| **total** | **44 m 41 s** | — | z1–14, 147,016,988 tile-features, 0 oversized |

¹ Convert peak measured by `/usr/bin/time -v` (Maximum resident set size). The
overview builder auto-selected **spill mode** here — its in-memory estimate for
holding all 138M output rows was ~456 GiB, far over budget, so output was
streamed to disk. Steady-state RSS sat far lower; 23.9 GiB is the transient peak.

### Per-zoom tile breakdown

```
  z 1:         1 tiles               2 feats
  z 2:         1 tiles             167 feats
  z 3:         3 tiles           2,004 feats
  z 4:         6 tiles          14,820 feats
  z 5:        13 tiles          99,497 feats
  z 6:        38 tiles         432,582 feats
  z 7:       119 tiles       1,286,509 feats
  z 8:       412 tiles       2,593,341 feats
  z 9:     1,461 tiles       4,517,577 feats
  z10:     5,237 tiles       7,615,854 feats
  z11:    18,730 tiles      12,745,628 feats
  z12:    66,511 tiles      21,348,208 feats
  z13:   228,808 tiles      35,843,132 feats
  z14:   754,118 tiles      60,517,667 feats
```

Commands:

```bash
# 1. Build the multi-resolution overview straight from remote GeoParquet.
#    (--files-from lists the 27 state URLs on Source Cooperative.)
tylertoo overview --files-from brazil-manifest.txt \
  --min-zoom 1 --max-zoom 14 \
  brazil-ov-z14.parquet

# 2. Export the PMTiles archive.
tylertoo export-pmtiles brazil-ov-z14.parquet \
  brazil-field-boundaries.pmtiles \
  --partition-wave auto
```

## What the numbers say

**1. No existing tool takes this path.** Tippecanoe is the de-facto standard for
building vector PMTiles, and it does not ingest GeoParquet. The documented
geoparquet-io route is `gpio convert geojson … | tippecanoe`, i.e. serialize the
whole dataset to GeoJSON first. tylertoo reads the GeoParquet — local *or*
remote — and writes the pyramid in one workflow, with no GeoJSON in the loop.

**2. The GeoJSON detour doesn't survive this scale.** On the comparably-sized
Germany buildings run (59M features), *generating the GeoJSON intermediate
alone* took 32 m 44 s and 19.1 GB on disk — before tippecanoe wrote a single
tile — and the documented one-liner then aborted on dense tiles. Brazil is the
same order of magnitude (55.5M features), and the intermediate would be larger,
not smaller.

**3. The source is cloud-native, and so is the read.** The FTW predictions live
as GeoParquet on Source Cooperative. tylertoo read all 27 state files over HTTPS
(a full-country run fetches the whole object once, ~1×, spilling locally so later
passes never re-hit the network; a regional `--bbox` fetches only the covering
row groups). There is no "download 6 GiB, then convert, then tile" preamble.

**4. Deep pyramid, bounded memory.** The archive spans z1–14 — from a single
whole-country tile up to 754k tiles at z14 — with export peaking at **1.74 GiB**.
At z14 the 55.5M features spread across 754k tiles (~80 features/tile), so no
single tile concentrates the dataset; memory stays flat regardless of depth.

## Methodology

- Wall time: `/usr/bin/time -v` per stage; total is start-to-finish wall clock.
- Peak RSS: `time -v` maximum-RSS per stage.
- Tile / feature counts: the `export-pmtiles` JSON report and the PMTiles v3
  header (`addressed tiles count`); cross-checked by decoding the z6 band back
  to GeoParquet (`tylertoo decode --zoom 6`) → 432,582 features from 38 tiles.
- Clipping and simplification differ from tippecanoe by design (see
  [`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md)); this demonstrates a
  *pipeline and its output*, not byte-identical tiling.

The archive is hosted on Source Cooperative and rendered live on the docs site —
see [`docs/demo.md`](../docs/demo.md) and the viewer at
[`docs/demo/viewer.html`](../docs/demo/viewer.html).
