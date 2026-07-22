# Demo: Brazil 2025 field predictions, filtered from a 630 GiB collection

A tiling run that starts from the **global**
[Fields of The World](https://fieldsofthe.world/) predictions collection on
[Source Cooperative](https://source.coop/) — **629.6 GiB of GeoParquet, 8.2
billion rows** — and ends with a **z0–14 PMTiles pyramid of Brazil's 2025
growing-season field predictions**: `--filter` selects one class and one
vintage, `--bbox` selects the country, and a tuned representation renders
**dots zoomed out, polygons zoomed in** — with a per-field handoff so nothing
disappears mid-zoom — all in one archive.

There is no established tool that produces this archive. Tippecanoe — the
standard vector tiler — does not read GeoParquet, let alone filter a remote
collection *while* tiling it. tylertoo reads the cloud-native source, carves
the slice, and writes the pyramid in one workflow.

## Explore the tiles

43.9M field predictions, rendered live from the PMTiles archive on
[CARTO Dark Matter](https://carto.com/basemaps/). Zoomed out you see dots
(z0–5); from z6 the field polygons progressively take over — each field stays a
dot until it is large enough to draw as a polygon, so **the map never loses a
field as you zoom in**. Pan and zoom, or jump to one of Brazil's agricultural
heartlands with the buttons.

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
| **convert** (remote read + filter of 40.7 GiB) | 1h 11m 56s | 9.6 GiB | 43.9M features filtered *during* the read → tuned overview GeoParquet (9.9 GB, 15 levels) |
| **export** | 10 m 52 s | **1.56 GiB** | **3.4 GiB** PMTiles, 1,649,201 tiles |
| **total** | — | — | z0–14, 117,659,919 tile-features, 0 oversized |

!!! note "How these tiles were tuned"
    The representation was tuned so **every field stays visible across zoom**
    (dots z0–5; from z6 each field stays a dot until it is large enough to draw
    as a ≥1 px polygon — no mid-zoom disappearance). The convert row above is the
    original measured remote run that carved the 43.9M-feature slice; the tuned
    tiling was then regenerated from that run's canonical (verbatim) geometry,
    which is output-identical to re-running the remote command with the tuned
    flags below.

```bash
# 1. Overview straight from the remote collection slice:
#    2025 vintage, fields only, Brazil bbox. Dots at z0-5; from z6 each field
#    is a representative point until it is large enough to be a >=1px polygon
#    (--collapse --polygon-visibility 0), so nothing disappears mid-zoom, and
#    --simplify-factor 4 aligns the dot->polygon handoff with pixel-visibility.
tylertoo overview --files-from brazil-2025-manifest.txt \
  --bbox="-74.1,-34.0,-34.7,5.4" \
  --filter "label = 'field' AND time >= '2025-01-01'" \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-5:point,6-14:geom" \
  --collapse --polygon-visibility 0 --simplify-factor 4 \
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
2. **One archive serves dots and polygons, and nothing disappears.** z0–5 are
   an explicit dot band (a point is always visible, so even z0 renders). From
   z6 the polygon band takes over, but a field only becomes a polygon once it
   is a ≥1 px shape (`--collapse --polygon-visibility 0 --simplify-factor 4`) —
   until then it stays a representative dot. So each field goes dot → polygon,
   never dot → gone: the visible feature count rises at every zoom. A two-line
   style split (`circle` + `fill`) renders both, coexisting through the mid
   zooms.
3. **Cloud-native at collection scale.** The run fetched 6.9% of the
   collection's bytes — the 52 files that could contain Brazil — once each
   (~1.0×, spilled locally so later passes never re-hit the network).
4. **Bounded memory at every stage.** Convert auto-selected spill mode and
   peaked at 9.6 GiB; export streamed all 15 levels at a peak of **1.56 GiB**.

![Representation handoff across zoom: the baseline switched abruptly from dots
to polygons at z7→z8 (leaving sub-pixel, invisible polygons); the tuned run
blends dots into polygons gradually so every field stays
visible.](demo/monotonic-handoff.png)

Full methodology, per-zoom breakdown, and the upstream findings the run
surfaced (INT96 timestamps, `"crs": null`, missing `covering` metadata) are in
the [demo directory on GitHub](https://github.com/geoparquet-io/tylertoo/tree/main/demo).

!!! note "Clipping/simplification differ from tippecanoe by design"
    This demonstrates the *native GeoParquet pipeline and its output*, not
    byte-identical tiling. See [Architecture](architecture.md) for the
    documented divergences.
