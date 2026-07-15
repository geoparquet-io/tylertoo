# Demo: Germany buildings, GeoParquet → PMTiles

A real, end-to-end run of the native **GeoParquet → PMTiles** path on
**Overture Germany buildings — 59,032,924 features** — measured head-to-head
against the current geoparquet-io recommended pipeline (GeoJSON → tippecanoe),
with the resulting tiles hosted and rendered below.

## Explore the tiles

All 59M building footprints, rendered live from the PMTiles archive on
[CARTO Dark Matter](https://carto.com/basemaps/). Pan and zoom (z2–14), or jump
to a city with the buttons.

<iframe src="viewer.html" title="gpq-tiles Germany buildings PMTiles viewer"
        style="width:100%; height:560px; border:1px solid var(--md-default-fg-color--lightest); border-radius:8px;"
        loading="lazy"></iframe>

<a class="md-button" href="viewer.html">Open the map full-screen →</a>

Tiles are served from [Source Cooperative](https://source.coop/) with HTTP Range
+ CORS (`nlebovits/gpq-tiles-demo`).

## Measured results

Same gpio-optimized GeoParquet feeds both pipelines. 16-core machine; zoom
`z0–14`, layer `buildings`, 500K per-tile cap matched across pipelines.

| pipeline | wall time | peak RSS | intermediate on disk | PMTiles out | tiles |
|---|---|---|---|---|---|
| **gpq-tiles** (default) | **13m 11s** | 14.8 GiB | **none** | 4.69 GB | 267,421 |
| **gpq-tiles** (tuned) | 18m 20s | 17.3 GiB | **none** | 6.28 GB | 267,917 |
| gpio → tippecanoe (`-P`, documented) | **failed at 57m 30s** | 7.2 GiB | 19.1 GB GeoJSON | aborted (z0–8 only) | — |

```bash
# gpq-tiles — native, one step, no intermediate
gpq-tiles tiles germany-buildings.parquet out.pmtiles \
  --min-zoom 0 --max-zoom 14 --layer-name buildings --max-tile-size 500K
# tuned adds:  --polygon-visibility 2.0 --collapse --drop-rate 1.3 --profile bounded

# incumbent — GeoParquet → GeoJSON stream → tippecanoe
gpio convert geojson germany-buildings.parquet \
  | tippecanoe -P -Z0 -z14 -l buildings -o out.pmtiles
```

## What the numbers say

1. **Native beats the round-trip by an order of magnitude.** gpq-tiles produces
   a complete z0–14 archive in **13m 11s** with **no intermediate file**.
2. **Generating the intermediate alone costs more than the whole native run.**
   `gpio convert geojson` took **32m 44s** to emit a **19.1 GB** GeoJSON stream —
   2.5× gpq-tiles' entire end-to-end time — before tippecanoe writes a tile.
3. **The documented incumbent one-liner fails on this data.** With `tippecanoe -P`
   as documented, tippecanoe hit dense building tiles over the 500K limit at z8
   and **aborted** (exit 100). It only finishes if you add
   `--drop-densest-as-needed`; gpq-tiles applies a drop-to-fit budget by default.
4. **default vs tuned is a quality knob, not speed.** The tuned run is
   deliberately larger and slower — `--polygon-visibility 2.0` and
   `--drop-rate 1.3` keep *more* features at coarse zoom for better fill (the
   swipe above). It is not a performance setting. (Since #259,
   `--polygon-visibility 2.0` **is** the shipped default, and the
   recommended country-view recipe is `--polygon-visibility 0 --collapse` —
   see [Overview tuning](OVERVIEW_TUNING.md).)

Full methodology, fairness notes, and hosting instructions are in the
[demo directory on GitHub](https://github.com/geoparquet-io/gpq-tiles/tree/main/demo).

!!! note "Clipping/simplification differ from tippecanoe by design"
    This compares *pipelines and their output*, not byte-identical tiling. See
    [Architecture](architecture.md) for the documented divergences.
