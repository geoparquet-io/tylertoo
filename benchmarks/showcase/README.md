# Overture buildings showcase (#810, track D)

One machine, one region of global Overture buildings `2026-10-21.0`, three
ways to a PMTiles archive:

| row | what runs | timed steps |
|---|---|---|
| tylertoo, `s3://` in place | `tylertoo tiles s3://…/type=building/ --bbox=<region>` (anonymous) | tile |
| tylertoo, local extract | gpio extract of the bbox, then `tylertoo tiles <extract>` | extract, tile |
| tippecanoe 2.79.0 | gpio extract, `gpio convert flatgeobuf`, then tippecanoe | extract, convert, tile |

Both tools use the same settings, following `benchmarks/e2e/run_e2e.py`:
- z0–14, tile buffer 8, a 500,000-byte compressed tile cap on both tools,
  and no raw cap on tylertoo.
- The same three properties: `class`, `subtype` and `height`.
- tippecanoe also gets the corpus `polygons-*` flags
  (`--simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed`).

## Files

| file | what it is |
|---|---|
| `showcase.py` | `regions`, `calibrate`, `extract` and `collect` (below). |
| `manifest.toml` | The `ovt-s3` and `ovt-local` cells in `run_manifest.py`'s format. `showcase.py extract` writes it. The committed copy is pinned to the smoke bbox. |
| `tippecanoe_shim.sh` | tippecanoe behind tylertoo's `tiles` argv, so `run_manifest.py` can time it as a `--bin`. It prints `showcase: fgb_convert_s=… tippecanoe_s=… fgb_bytes=…` and hard-links the archive into `$SHOWCASE_KEEP`. |
| `overture-buildings-2026-10-21.0.tsv` | All 512 parts: key, bytes, ETag, rows, and bbox (the union of the part's row groups). Totals: 276,943,764,116 bytes and 2,533,842,612 rows, listed and footer-read 2026-11-06. There is no mirror: a part whose ETag has changed is a different input. |
| `regions.tsv` | The candidate regions: bbox, estimated buildings and estimated bytes. Regenerate with `showcase.py regions`. |
| `test_showcase.py` | Tests for the projection, region and provenance logic and for the shim. |

`regions.tsv` estimates buildings from row-group bbox statistics, footers
only. Each row group's rows are taken as uniform over its bbox. A state's
bbox also takes in its neighbors' buildings, so the region is the bbox, not
the state.

## Runbook

Run this on a quiet machine. `run_manifest.py` waits for its load gate.
The tools are:

- `TT=target/release/tylertoo`, a release build of the commit under test
- `TC=/home/ubuntu/tylertoo-track-666/.tools/tippecanoe-2.79.0/bin/tippecanoe`,
  which must print `tippecanoe v2.79.0`
- gpio comes from `uvx --from geoparquet-io==1.7.0 gpio`

```bash
PY="uv run --no-project --python 3.12 python -I"
DATA=~/bench-data/showcase

# 1. Calibrate. This times both chains on the smoke bbox (about 1,000
#    buildings: the fixed costs, such as gpio's and tylertoo's scan of all
#    512 footers) and on Connecticut's bbox (about 1.8M), then prints a
#    projection for every candidate and the recommended region.
$PY benchmarks/showcase/showcase.py calibrate --tylertoo $TT

# 2. Extract the region (timed) and pin it: manifest.toml gets the
#    extract's sha256 and the region's bbox. Commit the manifest.
$PY benchmarks/showcase/showcase.py extract <region>

# 3. The timed runs. The results-directory names are what `collect` reads.
RM="$PY benchmarks/run_manifest.py --manifest benchmarks/showcase/manifest.toml run --data $DATA"
$RM --name <region>-s3    --cells ovt-s3    --bin tylertoo=$TT
$RM --name <region>-local --cells ovt-local --bin tylertoo=$TT            # 3 rounds
$RM --name <region>-tc    --cells ovt-local --pairs 1 --warmup 0 \
    --bin tippecanoe=benchmarks/showcase/tippecanoe_shim.sh \
    --bin-env "tippecanoe:TIPPECANOE_BIN=$TC" \
    --bin-env "tippecanoe:TIPPECANOE_FLAGS=--simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed" \
    --bin-env "tippecanoe:SHOWCASE_KEEP=$DATA/keep"

# 4. Keep a tylertoo archive for `stats --json`. run_manifest deletes its
#    outputs; tylertoo is deterministic, so this untimed build's sha256 must
#    equal the timed runs' (collect records both).
# (the ovt-s3 cell's args and flags from manifest.toml, output $DATA/keep/tylertoo.pmtiles)
AWS_SKIP_SIGNATURE=true AWS_REGION=us-west-2 $TT tiles s3://overturemaps-us-west-2/release/2026-10-21.0/theme=buildings/type=building/ \
    $DATA/keep/tylertoo.pmtiles --min-zoom 0 --max-zoom 14 <ovt-s3 flags>

# 5. The results JSON, with provenance.
$PY benchmarks/showcase/showcase.py collect <region> --tylertoo $TT \
    --archive tylertoo=$DATA/keep/tylertoo.pmtiles --archive tippecanoe=$DATA/keep/tippecanoe.pmtiles
uv run python scripts/gen_bench_docs.py   # if a doc region renders this file
```

Run every step on the same disk. Step 3 checks `min_free_gb`, and each
cell stops at `scratch_gib` (45 GiB).

## Choosing the size

`calibrate` fits a line through its two points for every measured
quantity. The quantities are the extract, conversion and tile times, and
each chain's peak scratch disk. A line separates the fixed cost, which is
the footer scan of all 512 parts and does not grow with the region, from
the per-building cost. The slope is never negative. `calibrate` then
projects each line to every candidate's estimated buildings and adds a
margin:

- tylertoo × 1.2
- tippecanoe × 1.3, because its sort is n log n and the projection is linear

The recommended region is the largest candidate within all of these
limits:

- tylertoo at 60 minutes or less, for both the `s3://` row and the local
  row
- tippecanoe end to end (extract + FlatGeobuf + tile) at 90 minutes or less
- each tool's working files at 40 GB or less. For tylertoo, that is the
  larger of the `s3://` run's staging and the extract plus the local run.
  For tippecanoe, it is the extract plus the FlatGeobuf, temp files and
  archive.

The smoke and calibration bboxes are never recommended. The recommendation
and every projected number are written to
`<work>/projection.json`. Record the chosen region and its projection here
when the run is done.

Chosen region: _not yet calibrated_ (the maintainer runs step 1).

## Results

`collect` writes `benchmarks/results/overture-buildings-<region>.json`.
Its shape is gen_bench_docs.py's `vida-head-to-head` schema plus the
fields the module docstring lists. It records:

- **provenance**: commit, tylertoo version, host, nproc, kernel, allocator,
  tool versions (tippecanoe, its commit, gpio) and date. Its inputs are the
  listing (release, bbox, the sha256 of the parts TSV, and the URL, ETag and
  bytes of every part the bbox touches) and the extract (sha256, bytes, and
  the gpio command line).
- **rows**: per row, the timed runs' wall times, the conversion time, peak
  RSS (the sampler's `true_peak_mib` when the profile has it), effective
  cores, archive bytes and sha256, and peak scratch disk (sampled every
  ~2 s).
  - Only the `s3://` row records bytes fetched (`pass0.bytes_fetched`).
  - The tippecanoe row splits its time into steps: extract, convert and
    tile.
- **archive**: `tylertoo stats --json` of the kept archives (per-zoom
  tiles, sizes and p99), the largest tile, and tiles over the cap among
  the 1000 largest.

## Tests

```bash
uv run --no-project --with pytest pytest benchmarks/showcase/test_showcase.py
```

Each test was shown to fail on an injected regression. All 13 injections
turned the suite red:

- the tippecanoe margin dropped
- the smallest region recommended
- the disk limit ignored
- a falling slope allowed
- the fixed cost lost
- the tippecanoe row's wall double-counting the conversion
- the extract left out of tippecanoe's end-to-end time
- every part listed instead of the touched ones
- touching bboxes not intersecting
- signed S3 reads
- `pass0` not read
- the shim ignoring `--max-zoom`
- the FlatGeobuf left behind in the shim
