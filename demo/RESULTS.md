# Results — Germany buildings, GeoParquet → PMTiles

Head-to-head on **Overture Germany buildings — 59,032,924 features**, native
gpq-tiles vs the geoparquet-io recommended pipeline (GeoJSON → tippecanoe).

## Setup

- **Input:** one gpio-optimized GeoParquet (Hilbert-sorted + bbox, 6.6 GB) —
  the *same* file feeds both pipelines.
- **Machine:** 16 cores.
- **Versions:** tippecanoe v2.49.0, geoparquet-io 1.1.0b1, gpq-tiles built from
  the #256 branch (simple-clip fast path default-on).
- **Fairness:** matched across pipelines — zoom `z0–14`, layer `buildings`, and
  a 500K per-tile cap (tippecanoe's default; gpq-tiles pinned via
  `--max-tile-size 500K`).

## Numbers

| pipeline | wall time | peak RSS | intermediate on disk | PMTiles out | tiles |
|---|---|---|---|---|---|
| **gpq-tiles** (default) | **13m 11s** | 14.8 GiB | **none** | 4.69 GB | 267,421 |
| **gpq-tiles** (tuned) | 18m 20s | 17.3 GiB | **none** | 6.28 GB | 267,917 |
| gpio → tippecanoe (`-P`, documented) | **failed at 57m 30s** | 7.2 GiB | 19.1 GB GeoJSON | aborted (89 MB, z0–8 only) | — |

Commands:

```bash
# gpq-tiles — native, no intermediate
gpq-tiles tiles germany-buildings.parquet out.pmtiles \
  --min-zoom 0 --max-zoom 14 --layer-name buildings --max-tile-size 500K
# tuned adds:  --polygon-visibility 2.0 --collapse --drop-rate 1.3 --profile bounded

# incumbent — GeoParquet → GeoJSON stream → tippecanoe
gpio convert geojson germany-buildings.parquet \
  | tippecanoe -P -Z0 -z14 -l buildings -o out.pmtiles
```

## What the numbers say

**1. Native beats the round-trip by an order of magnitude.** gpq-tiles turns the
GeoParquet into a complete z0–14 PMTiles archive in **13m 11s** with **no
intermediate file**. The incumbent has to serialize the data to GeoJSON first.

**2. Just *generating* the intermediate costs more than the whole native run.**
`gpio convert geojson` took **32m 44s** to emit a **19.1 GB** GeoJSON stream —
**2.5× gpq-tiles' entire end-to-end time** — before tippecanoe writes a single
tile.

**3. The documented incumbent one-liner fails on this data.** With the flags
from the geoparquet-io docs (`tippecanoe -P`), tippecanoe hit dense building
tiles over the 500K limit at z8 and **aborted** (exit 100, *"tiles only complete
through zoom 8"*). It only finishes if you know to add `--drop-densest-as-needed`
(tippecanoe's drop-to-fit flag). gpq-tiles applies a drop-to-fit budget **by
default** and completed all 15 zoom levels unattended.

> The fair rerun with `--drop-densest-as-needed` was started but not run to
> completion. Its read phase alone ingests ~30k features/s — again ~33 min just
> to consume the GeoJSON stream, before tiling. Rather than project a completion
> time into the table, that cell is left as the measured failure of the
> documented command.

**4. gpq-tiles default vs tuned is a *quality* knob, not a speed one.** The tuned
run is deliberately larger and slower: `--polygon-visibility 2.0` (below the 4.0
default) and `--drop-rate 1.3` (below 1.65) keep *more* features at coarse zoom
for better fill when zoomed out (the "4–12× more coarse-zoom survivors" story).
It is not a performance setting — compare it to default for map appearance, not
throughput.

## Methodology

- Wall time: `/usr/bin/time -v` for the single-process gpq-tiles runs; pipeline
  wall clock for the streamed gpio → tippecanoe run.
- Peak RSS: `time -v` maximum-RSS for gpq-tiles; a 0.5 s process-group sampler
  (summing gpio + tippecanoe) for the streamed pipeline.
- Tile count: `addressed_tiles_count` from each PMTiles v3 header.
- Intermediate size: bytes counted in-stream (`tee >(wc -c)`), never written to
  disk.
- Clipping and simplification differ from tippecanoe by design (see
  [`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md)); this compares
  *pipelines and their output*, not byte-identical tiling.

The tuned archive is hosted on Source Cooperative and rendered live on the docs
site — see [`docs/demo.md`](../docs/demo.md) and the viewer at
[`docs/demo/viewer.html`](../docs/demo/viewer.html).
