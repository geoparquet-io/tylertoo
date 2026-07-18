# Demo: Brazil 2025 field predictions, filtered from a 630 GiB collection

A tiling run that starts from the **global**
[Fields of The World](https://fieldsofthe.world/) predictions collection on
[Source Cooperative](https://source.coop/) — **629.6 GiB of GeoParquet, 8.2
billion rows** — and ends with a **z0–14 PMTiles pyramid of Brazil's 2025
growing-season field predictions**: `--filter` selects one class and one
vintage, `--bbox` selects the country, and `--representation "0-7:point"`
renders **dots zoomed out, polygons zoomed in**, all in one archive.

There is no established tool that produces this archive. Tippecanoe — the
standard vector tiler — does not read GeoParquet, let alone filter a remote
collection *while* tiling it. tylertoo reads the cloud-native source, carves
the slice, and writes the pyramid in one workflow.

## Explore the tiles

43.9M field predictions, rendered live from the PMTiles archive on
[CARTO Dark Matter](https://carto.com/basemaps/). Zoomed out you see the
centroid band (z0–7); from z8 the actual field polygons take over. Pan and
zoom, or jump to one of Brazil's agricultural heartlands with the buttons.

<iframe src="viewer.html" title="tylertoo Brazil 2025 field predictions PMTiles viewer"
        style="width:100%; height:560px; border:1px solid var(--md-default-fg-color--lightest); border-radius:8px;"
        loading="lazy"></iframe>

<a class="md-button" href="viewer.html">Open the map full-screen →</a>

Tiles are served from [Source Cooperative](https://source.coop/) with HTTP Range
+ CORS (`nlebovits/gpq-tiles-demo`).

## The measured run

Full remote → PMTiles round trip on a 16-core machine, zoom `z0–14`. The input
is the 52 part files of the global collection whose footers intersect Brazil
(40.7 GiB read over HTTPS); the other 948 files (589 GiB) were never touched.

| stage | wall | peak RSS | output |
|---|---|---|---|
| **convert** (incl. 40.7 GiB remote read) | 1h 11m 56s | 9.6 GiB | 14.1 GB overview GeoParquet (15 levels) |
| **export** | 11 m 44 s | **1.54 GiB** | **4.5 GiB** PMTiles, 1,647,927 tiles |
| **total** | **1 h 23 m 40 s** | — | z0–14, 116,504,741 tile-features, 0 oversized |

```bash
# 1. Overview straight from the remote collection slice:
#    2025 vintage, fields only, Brazil bbox, centroids at z0-7.
tylertoo overview --files-from brazil-2025-manifest.txt \
  --bbox="-74.1,-34.0,-34.7,5.4" \
  --filter "label = 'field' AND time >= '2025-01-01'" \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point" \
  brazil-2025-fields-ov-z14.parquet

# 2. Export the PMTiles archive.
tylertoo export-pmtiles brazil-2025-fields-ov-z14.parquet \
  brazil-2025-fields.pmtiles
```

## What the run shows

1. **The filter is the feature.** No curated "Brazil 2025 fields" extract
   exists — the slice lives interleaved with two other prediction classes and
   the 2024 vintage inside 1,000 Spark part files. The predicate
   `label = 'field' AND time >= '2025-01-01'` plus the bbox kept 43.9M of the
   426M rows scanned (10.3%), evaluated *during* the tiling read — no DuckDB
   pre-pass, no intermediate file.
2. **One archive serves dots and polygons.** The z0–7 band stores one
   representative point per surviving feature (a point is always visible, so
   even z0 renders — the previous polygon-only demo's z0 was empty); z8–14
   store the polygons. A two-line style split (`circle` + `fill`) renders
   both.
3. **Cloud-native at collection scale.** The run fetched 6.9% of the
   collection's bytes — the 52 files that could contain Brazil — once each
   (~1.0×, spilled locally so later passes never re-hit the network).
4. **Bounded memory at every stage.** Convert auto-selected spill mode (the
   in-RAM estimate for its 109M output rows was ~315 GiB) and peaked at
   9.6 GiB; export streamed all 15 levels at a peak of **1.54 GiB**.

Full methodology, per-zoom breakdown, and the upstream findings the run
surfaced (INT96 timestamps, `"crs": null`, missing `covering` metadata) are in
the [demo directory on GitHub](https://github.com/geoparquet-io/tylertoo/tree/main/demo).

!!! note "Clipping/simplification differ from tippecanoe by design"
    This demonstrates the *native GeoParquet pipeline and its output*, not
    byte-identical tiling. See [Architecture](architecture.md) for the
    documented divergences.
