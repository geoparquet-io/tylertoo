# tylertoo overview test corpus

Reproducible test datasets + golden references for evaluating
GeoParquet-embedded multi-resolution **overviews** (quality metrics in
`corpus/METRICS.md`, sweep decisions in `corpus/SWEEPS.md`, access/storage
benchmarks in `benchmarks/overview/`).

**This directory contains scripts and manifests only. No data files
are committed.** Everything under `corpus/data/` is gitignored and
rebuilt on demand from the recipes here.

## What the corpus is

One dataset per geometry **class** at small / medium / large
**scale tiers**, chosen to stress the overview generalization engine
where it matters:

| Class   | Why it's here |
|---------|---------------|
| point   | thinning behaviour (POIs); cell-winner selection |
| line    | **priority class** — worst case for thinning-only approaches (COGP), best case for our simplification (roads) |
| polygon | ring validity + coalescing under simplification (buildings) |
| monster | few features, enormous vertex counts (coastline, admin-0) — stresses per-level vertex reduction and row-group sizing |

For each dataset we also build a **gpio-optimized copy**
(Hilbert-sorted, bbox-covered GeoParquet 1.1) — this is the input
contract the overview converter (P5) assumes.

## Dataset composition

See `manifest.json` for the machine-readable source of truth
(ids, bboxes, expected counts, licenses, tippecanoe flags).

| id | class | tier | source | ~features | profile |
|----|-------|------|--------|-----------|---------|
| points-boise-small       | point   | small  | Overture places         | 37k (verified)   | default |
| points-nyc-medium        | point   | medium | Overture places         | 458k (verified)  | default |
| lines-boise-small        | line    | small  | Overture segments       | 152k (verified)  | default |
| lines-portland-medium    | line    | medium | Overture segments       | ~280k (est)      | default |
| polygons-portland-medium | polygon | medium | Overture buildings      | ~650k (est)      | default |
| monster-coastline        | monster | world  | Natural Earth 10m       | ~4.2k lines      | default |
| monster-admin            | monster | world  | Natural Earth 10m       | ~258 polygons    | default |
| lines-oregon-large       | line    | large  | Overture segments       | ~5M (est)        | `--large` |
| polygons-eurocrops-parcels | polygon | large | Source Cooperative    | unknown          | manual (human-confirm) |

Feature counts marked *verified* were counted against the pinned
Overture release on 2026-07-02. Others are estimates; the fetch
query pattern for every Overture theme was verified end-to-end.

## How to build it

```bash
cd corpus

# 1. download/build raw datasets (default = small+medium tiers)
./fetch.sh
#    add the large tier (state-scale roads, ~1 GB):
./fetch.sh --large

# 2. produce gpio-optimized copies (hilbert + bbox covering)
./optimize.sh

# 3. build golden references (tippecanoe + cogp-rs)
./goldens.sh
```

All scripts are idempotent (existing outputs skipped; `--force`
rebuilds) and use `set -euo pipefail`. They check for required
tools and fail with install instructions.

### Layout produced

```
corpus/
  manifest.json            dataset definitions (committed)
  METRICS.md               metric definitions for V2/V3 (committed)
  fetch.sh optimize.sh goldens.sh
  data/                    (gitignored)
    raw/<id>.parquet       as fetched
    gpio/<id>.parquet      hilbert-sorted, bbox-covered GeoParquet 1.1
    goldens/
      tippecanoe/<id>.pmtiles
      tippecanoe/<id>.flags.txt   exact flags used
      cogp/<id>.parquet           (if cogp installable)
```

## Disk requirements

| Profile | Raw | +gpio | +goldens | Total (approx) |
|---------|-----|-------|----------|----------------|
| default (small+medium) | ~0.3 GB | ~0.6 GB | ~0.9 GB | **< 1 GB** |
| `--large` adds          | ~0.9 GB | ~1.8 GB | ~2.5 GB | **~2.5 GB extra** |

Default stays well under the 2-3 GB budget. The large tier is opt-in.

## Required tools

| Tool | Used by | Install |
|------|---------|---------|
| duckdb | fetch | https://duckdb.org/docs/installation/ |
| jq | fetch, goldens | `apt-get install jq` / `brew install jq` |
| gpio (geoparquet-io) | fetch, optimize, goldens | `uv tool install geoparquet-io` |
| curl, unzip | fetch (Natural Earth) | OS package manager |
| tippecanoe | goldens | https://github.com/felt/tippecanoe |
| cargo | goldens (cogp, optional) | https://rustup.rs |

## Data sources & licenses

- **Overture Maps** (`s3://overturemaps-us-west-2`), release pinned
  in `manifest.json` (`2026-06-17.0`, verified 2026-07-02).
  Override with `GPQ_OVERTURE_RELEASE=<release>` or query the STAC
  catalog (`https://stac.overturemaps.org/catalog.json`) for latest.
  - places → CDLA-Permissive-2.0
  - transportation, buildings → ODbL-1.0 (contains OpenStreetMap)
- **Natural Earth** 10m (`naciscdn.org`) → Public Domain.
- **Source Cooperative / EuroCrops** → per-country open licenses;
  **not auto-fetched, needs human confirmation** (see below).

### Needs human confirmation

`polygons-eurocrops-parcels` is a documented candidate only. A
stable public **US county parcels** GeoParquet was not located
during corpus construction; EuroCrops (agricultural field
boundaries) is the closest verified-to-exist public parcels-like
dataset, but its exact object path on Source Cooperative was not
verified. Confirm a source before wiring it into `fetch.sh`.

## Golden references

- **tippecanoe** PMTiles per dataset give the visual + per-zoom
  feature-count baseline for V2 (quality) and V3 (storage/access
  comparison). Exact flags are recorded in
  `data/goldens/tippecanoe/<id>.flags.txt`.
- **cogp-rs** output is the thinning-parity baseline (COGP does
  thinning + layout, no simplification). cogp is not on crates.io;
  `goldens.sh` installs it via `cargo install --git` and skips
  gracefully if that fails.

See `METRICS.md` for exactly which numbers V2/V3 should compute.
