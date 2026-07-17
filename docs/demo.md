# Demo: Brazil field boundaries, cloud-native GeoParquet → PMTiles

A full-country tiling run of the native **GeoParquet → PMTiles** path on
**[Fields of The World](https://fieldsofthe.world/) field boundaries for
Brazil — 55,499,514 features** — read *directly* from GeoParquet on
[Source Cooperative](https://source.coop/) and tiled to a complete **z1–14**
pyramid, hosted and rendered below.

There is no established tool that produces this archive. Tippecanoe — the
standard vector tiler — does not read GeoParquet, and the documented workaround
(serialize everything to GeoJSON first) does not survive 55 million features.
tylertoo reads the cloud-native source and writes the pyramid in one workflow.

## Explore the tiles

All 55.5M field boundaries, rendered live from the PMTiles archive on
[CARTO Dark Matter](https://carto.com/basemaps/). Pan and zoom (z1–14), or jump
to one of Brazil's agricultural heartlands with the buttons.

<iframe src="viewer.html" title="tylertoo Brazil field boundaries PMTiles viewer"
        style="width:100%; height:560px; border:1px solid var(--md-default-fg-color--lightest); border-radius:8px;"
        loading="lazy"></iframe>

<a class="md-button" href="viewer.html">Open the map full-screen →</a>

Tiles are served from [Source Cooperative](https://source.coop/) with HTTP Range
+ CORS (`nlebovits/gpq-tiles-demo`).

## The measured run

Full remote → PMTiles round trip on a 16-core machine, `--partition-wave auto`,
zoom `z1–14`. The input is 27 per-state GeoParquet files (6.1 GiB) read over
HTTPS — nothing downloaded by hand first.

| stage | wall | peak RSS | output |
|---|---|---|---|
| **convert** (incl. 6.1 GiB remote read) | 23m 25s | 23.9 GiB | 18 GB overview GeoParquet (14 levels) |
| **export** (`--partition-wave auto`) | 21m 07s | **1.74 GiB** | **8.4 GiB** PMTiles, 1,075,458 tiles |
| **total** | **44m 41s** | — | z1–14, 147M tile-features, 0 oversized |

```bash
# 1. Build the multi-resolution overview straight from remote GeoParquet.
tylertoo overview --files-from brazil-manifest.txt \
  --min-zoom 1 --max-zoom 14 brazil-ov-z14.parquet

# 2. Export the PMTiles archive.
tylertoo export-pmtiles brazil-ov-z14.parquet \
  brazil-field-boundaries.pmtiles --partition-wave auto
```

## What the run shows

1. **No existing tool takes this path.** Tippecanoe does not ingest GeoParquet;
   the documented geoparquet-io route is `gpio convert geojson … | tippecanoe`,
   which serializes the entire dataset to GeoJSON before tiling. tylertoo reads
   the GeoParquet — local *or* remote — with no GeoJSON in the loop.
2. **The GeoJSON detour doesn't survive this scale.** On the comparable Germany
   buildings run (59M features), *generating the intermediate alone* took
   32m 44s and 19.1 GB on disk before tippecanoe wrote a tile — and the
   documented one-liner then aborted on dense tiles. Brazil is the same order of
   magnitude; the intermediate would be larger.
3. **Cloud-native source, cloud-native read.** The FTW predictions live as
   GeoParquet on Source Cooperative. tylertoo read all 27 state files over HTTPS
   (a full-country run fetches the object once, ~1×, spilling locally so later
   passes never re-hit the network; a regional `--bbox` fetches only the
   covering row groups). No "download, then convert, then tile" preamble.
4. **Deep pyramid, flat memory.** The archive spans z1–14 — one whole-country
   tile up to 754k tiles at z14 — and export peaked at **1.74 GiB**. At z14 the
   features spread across 754k tiles (~80 per tile), so no single tile
   concentrates the dataset and memory stays flat regardless of depth.

Full methodology, per-zoom breakdown, and hosting instructions are in the
[demo directory on GitHub](https://github.com/geoparquet-io/tylertoo/tree/main/demo).

!!! note "Clipping/simplification differ from tippecanoe by design"
    This demonstrates the *native GeoParquet pipeline and its output*, not
    byte-identical tiling. See [Architecture](architecture.md) for the
    documented divergences.
