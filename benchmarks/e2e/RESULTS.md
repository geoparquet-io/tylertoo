# e2e GeoParquet → PMTiles: tylertoo vs pinned tippecanoe — measured

Method, settings-parity mapping and the full asymmetry list are in
[README.md](README.md). **Read that before quoting anything here.**

Every timing, memory, size and tile-count table below is printed by
`format_results.py` from [`results.json`](results.json) and pasted verbatim —
no number is retyped. The two exceptions are labelled where they appear: the
per-zoom *feature* counts in §2 come from `render_parity.py` (PMTiles decoding,
which `tylertoo stats` does not do), and the variance table in §7 is derived
from the `wall_all_s` arrays in the same `results.json`.

Issue: [#447](https://github.com/geoparquet-io/tylertoo/issues/447).

## Provenance

- **Machine** — Apple M3 Pro, 12 cores, 18 GB RAM, macOS-15.6-arm64-arm-64bit
- **tylertoo** — tylertoo 0.11.0 @ `044360d` (release build)
- **tippecanoe** — tippecanoe v2.79.0, tag `2.79.0`, commit `68ab8dcc229f95b8b25877697d5e8d66783af503`, built from source by `setup_tippecanoe.sh`
- **Settings** — z0–z14, tile buffer 8, per-tile cap 500,000 bytes
- **Method** — median of 3 timed runs after one discarded warm-up; warm page cache; no other load on the machine
- **Recorded** — 2026-09-26T20:41:10Z → `results.json`

A laptop is not a cluster. These numbers are a floor for reproducibility, not
a claim about server-class hardware; `--dataset ID=PATH` runs the identical
harness on a bigger file when someone has one.

## What to quote

> On a 17,465-feature MultiPolygon admin dataset, at output parity, tylertoo
> produced the same z0–z14 PMTiles archive **≈7.5× faster** than tippecanoe
> 2.79.0 reading FlatGeobuf (2.67 s vs 20.05 s), in an archive 22% smaller,
> at roughly 1.8× tippecanoe's peak RSS. Counting the GeoParquet→FlatGeobuf
> conversion a tippecanoe user has to run, and with tylertoo at its own
> defaults (which emit a thinned map — see below), the end-to-end figure is
> **≈10–12×**.

The range on the defaults figure is not hedging: two runs of this harness on
this machine an hour apart gave 10.4× and 12.5× for the same dataset and
flags. Quote the band, not the better end. §7 has the variance data.

## 1. Headline (both tools at their own defaults)

| dataset | features | tylertoo (parquet→pmtiles) | tippecanoe e2e (convert+tile) | tippecanoe tile-only (fgb in) | e2e | tile-only |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 17,465 | 1.61 s | 20.24 s | 20.05 s | **12.5×** | **12.4×** |
| `open-buildings` | 1,000 | 0.03 s | 0.28 s | 0.22 s | **9.3×** | **7.2×** |
| `road-detections` | 1,000 | 0.04 s | 0.28 s | 0.21 s | **6.8×** | **5.3×** |

**`madagascar-adm4` is the only row that measures tiling.** The other two are
1,000-feature fixtures where both tools spend most of their wall time on
process start-up and pyramid scaffolding; they are the geometry-class smoke
test (Polygon, LineString) and nothing more. Do not average these rows.

**This table is not an output-parity comparison.** See §2.

## 2. Output parity — the number to quote to a skeptic

At defaults the two tools do not draw the same map. tylertoo's density budget
thins features at coarse and mid zooms; tippecanoe drops only points, so on a
contiguous polygon coverage it emits every feature at every zoom. Measured
with `render_parity.py` (distinct feature ids per zoom, so cross-tile
duplication does not inflate either side):

| zoom | tylertoo, defaults | tylertoo, quality-matched | tippecanoe |
|---|---|---|---|
| z4 | 254 | 5,786 | 6,102 |
| z8 | **865** | 17,406 | **17,465** |
| z11 | 3,888 | 17,465 | 17,465 |

_(from `render_parity.py --zooms 4,8,11`, not from `results.json` — counting
features means decoding tiles.)_

At z8 tylertoo's default archive carries 865 of 17,465 admin-4 polygons. That
is not a subtle difference — rendered side by side, the default output is a
scatter of disconnected polygons where tippecanoe's is a complete
choropleth. A speed ratio against that is a ratio against less work.

So the harness also runs tylertoo with the thinning ladder switched off
(`--verbatim --simplify-factor 1.0`, which keeps simplification because
tippecanoe simplifies too). That lands within 1–2 tiles of tippecanoe's count
at every zoom and within 0.3% of its feature count at z8:

| dataset | tylertoo default | tylertoo quality-matched | tippecanoe tile-only | tile-only speedup, matched | tylertoo tiles (default → matched) | tippecanoe tiles | archive, matched | archive ratio |
|---|---|---|---|---|---|---|---|---|
| `madagascar-adm4` | 1.61 s | 2.67 s | 20.05 s | **7.5×** | 146,924 → 153,052 | 153,047 | 118.0 MB | 0.78× |
| `open-buildings` | 0.03 s | 0.03 s | 0.22 s | **7.1×** | 82 → 82 | 90 | 0.1 MB | 0.70× |
| `road-detections` | 0.04 s | 0.04 s | 0.21 s | **5.6×** | 78 → 79 | 82 | 0.2 MB | 0.80× |

**≈7.5× at output parity on the one dataset large enough to mean anything**,
with a 22% smaller archive. Turning the ladder off costs tylertoo 66% more
wall time (1.61 s → 2.67 s) and buys the missing 4.0% of tiles and ~16,600
missing mid-zoom features.

Regenerate the renders (they are not committed):

```bash
python3 run_e2e.py --only madagascar-adm4 --quality-matched \
    --work /tmp/e2e --keep
uv run --with pmtiles --with mapbox-vector-tile --with matplotlib \
    python3 render_parity.py /tmp/e2e/madagascar-adm4.tylertoo-qm.pmtiles \
        /tmp/e2e/madagascar-adm4.tippecanoe-fgb.pmtiles --zooms 4,8,11 \
        --out /tmp/parity-qm
```

Visual note, by eye at z4/z8/z11: the quality-matched output is
indistinguishable from tippecanoe's in coverage and boundary structure.
tylertoo's rings are slightly smoother at the same zoom — its RDP tolerance is
metres-of-GSD where tippecanoe's is tile units — which is consistent with the
smaller archive and is the remaining, unclosable simplification asymmetry.

## 3. Where tippecanoe's end-to-end time actually goes

| dataset | parquet in | fgb out | conversion (fastest tool) | tool | conversion share of tippecanoe e2e | ldGeoJSON out | ldGeoJSON convert | tippecanoe -P on ldGeoJSON |
|---|---|---|---|---|---|---|---|---|
| `madagascar-adm4` | 27.1 MB | 43.2 MB | 0.19 s | `ogr2ogr` | 1% | 75.0 MB | 2.07 s | 23.59 s |
| `open-buildings` | 0.1 MB | 0.3 MB | 0.06 s | `ogr2ogr` | 22% | 0.4 MB | 0.06 s | 0.21 s |
| `road-detections` | 0.1 MB | 0.2 MB | 0.06 s | `ogr2ogr` | 23% | 0.3 MB | 0.07 s | 0.21 s |

**The input-format advantage is small on this dataset, and saying so matters.**
On `madagascar-adm4` the GeoParquet→FlatGeobuf conversion is 1% of
tippecanoe's end-to-end wall; the e2e and tile-only ratios are 12.5× and
12.4×. The native-parquet read is a real product advantage — no extra file,
no 43 MB of intermediate on disk, row-group and bbox pushdown that a
FlatGeobuf stream cannot offer — but on this fixture it is **not** where the
speed comes from. On the two tiny fixtures it is 22–23% of the e2e time,
which is mostly `ogr2ogr` start-up, not conversion work.

The ldGeoJSON leg is evidence for the claim that FlatGeobuf is tippecanoe's
best input here: text is 75 MB against 43 MB of FlatGeobuf, and even with
`-P` (parallel parse) tippecanoe is slower reading it (23.59 s vs 20.05 s)
on top of a 10× more expensive conversion (2.07 s vs 0.19 s). Benchmarking
tippecanoe from GeoJSON would have flattered tylertoo, so the headline does
not.

`gpio convert flatgeobuf` **failed** on `madagascar-adm4`
(`IO Error: ICreateFeature: NULL geometry not supported with spatial index`)
even though the parquet has no null, empty or invalid geometry — so `ogr2ogr`
supplied that input. On the small fixtures gpio succeeded but was slower than
`ogr2ogr` (Python interpreter start-up), and the harness always takes the
faster converter so the baseline is never penalised for our tool choice. gpio
remains the recommended GeoParquet preprocessor; this is a bug report, not a
recommendation change.

## 4. Memory and archive size

| dataset | tylertoo peak RSS | tippecanoe peak RSS | converter peak RSS | tylertoo archive | tippecanoe archive | archive ratio |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 760 MB | 413 MB | 199 MB | 99.6 MB | 151.0 MB | 0.66× |
| `open-buildings` | 28 MB | 139 MB | 54 MB | 0.1 MB | 0.2 MB | 0.61× |
| `road-detections` | 32 MB | 141 MB | 53 MB | 0.1 MB | 0.2 MB | 0.57× |

**tylertoo loses the memory column on the only dataset that matters** — 760 MB
against tippecanoe's 413 MB on `madagascar-adm4`, 1.8×. tylertoo's export
concurrency is `--partition-wave auto`, which sizes itself against available
RAM, so this figure moves with machine state (an earlier run of the same
harness, same machine, without `--quality-matched`, recorded 457 MB). Pin it
with an explicit `--partition-wave N` before drawing conclusions. On the tiny
fixtures tylertoo wins the column, 28 MB to 139 MB, because tippecanoe
allocates its working set up front.

Archive sizes are tylertoo's, consistently: 0.57–0.66× tippecanoe at defaults,
0.70–0.80× quality-matched.

## 5. Where tylertoo's own time goes

| dataset | convert (parquet read + ladder) | export (MVT + archive) | row groups read/total | tiles | tile features | oversized tiles |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 0.37 s | 1.23 s | 1/1 | 146,924 | 353,062 | 0 |
| `open-buildings` | 0.01 s | 0.02 s | 1/1 | 82 | 2,339 | 0 |
| `road-detections` | 0.01 s | 0.02 s | 1/1 | 78 | 3,787 | 0 |

The parquet read and the whole generalization ladder are 23% of tylertoo's
wall on `madagascar-adm4`; MVT encoding and the archive write are the rest.
These fixtures are single-row-group, so row-group pushdown contributes nothing
here — one more reason the input-format advantage is understated by this
corpus rather than overstated.

## 6. Per-zoom tile counts

### `madagascar-adm4`

| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 0 | 1 | — | 0.14 MB |
| z1 | 1 | 1 | 0.00 MB | 0.36 MB |
| z2 | 1 | 1 | 0.02 MB | 0.38 MB |
| z3 | 2 | 2 | 0.03 MB | 0.52 MB |
| z4 | 4 | 4 | 0.03 MB | 0.66 MB |
| z5 | 4 | 4 | 0.03 MB | 0.61 MB |
| z6 | 6 | 7 | 0.04 MB | 0.82 MB |
| z7 | 15 | 16 | 0.09 MB | 2.08 MB |
| z8 | 42 | 44 | 0.17 MB | 2.44 MB |
| z9 | 138 | 149 | 0.36 MB | 3.19 MB |
| z10 | 487 | 524 | 0.82 MB | 4.41 MB |
| z11 | 1,705 | 1,943 | 2.09 MB | 6.91 MB |
| z12 | 6,037 | 7,440 | 6.27 MB | 13.68 MB |
| z13 | 24,489 | 28,920 | 22.10 MB | 35.11 MB |
| z14 | 113,993 | 113,991 | 93.19 MB | 110.20 MB |
| **total** | **146,924** | **153,047** | | |

Read the byte columns, not just the tile columns: at z1–z8 tylertoo's stored
bytes are 10–25× smaller than tippecanoe's for nearly the same tile count.
That is the thinning of §2 showing up as size. At z14, where tylertoo emits
every feature, the two agree to two tiles and tylertoo's bytes are 15%
smaller — the simplification difference alone.

tylertoo emits no z0 tile: the visibility gate generalizes every feature away
at that GSD, and the archive declares the zoom rather than writing an empty
tile. tippecanoe writes one.

### `open-buildings`

| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z5 | 0 | 1 | — | 0.00 MB |
| z6 | 0 | 1 | — | 0.00 MB |
| z7 | 0 | 1 | — | 0.00 MB |
| z8 | 0 | 1 | — | 0.00 MB |
| z9 | 2 | 2 | 0.00 MB | 0.01 MB |
| z10 | 2 | 2 | 0.00 MB | 0.02 MB |
| z11 | 2 | 3 | 0.01 MB | 0.02 MB |
| z12 | 4 | 7 | 0.01 MB | 0.03 MB |
| z13 | 20 | 20 | 0.02 MB | 0.04 MB |
| z14 | 52 | 52 | 0.05 MB | 0.05 MB |
| **total** | **82** | **90** | | |

### `road-detections`

| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 0 | 1 | — | 0.00 MB |
| z1 | 0 | 1 | — | 0.00 MB |
| z2 | 0 | 1 | — | 0.00 MB |
| z3 | 0 | 1 | — | 0.00 MB |
| z4 | 0 | 1 | — | 0.00 MB |
| z5 | 1 | 1 | 0.00 MB | 0.01 MB |
| z6 | 1 | 1 | 0.00 MB | 0.01 MB |
| z7 | 2 | 2 | 0.00 MB | 0.01 MB |
| z8 | 2 | 2 | 0.00 MB | 0.01 MB |
| z9 | 2 | 2 | 0.01 MB | 0.02 MB |
| z10 | 2 | 2 | 0.01 MB | 0.02 MB |
| z11 | 5 | 5 | 0.01 MB | 0.03 MB |
| z12 | 7 | 7 | 0.01 MB | 0.03 MB |
| z13 | 15 | 15 | 0.02 MB | 0.03 MB |
| z14 | 41 | 40 | 0.04 MB | 0.04 MB |
| **total** | **78** | **82** | | |

## 7. Run-to-run variance

Within a single run (3 timed repeats after a warm-up), on `madagascar-adm4`:

| measurement | median | min | max | spread |
|---|---|---|---|---|
| tylertoo, defaults | 1.61 s | 1.60 s | 1.80 s | 13% |
| tylertoo, quality-matched | 2.67 s | 2.56 s | 3.41 s | 33% |
| tippecanoe, fgb in | 20.05 s | 19.70 s | 22.81 s | 16% |
| tippecanoe -P, ldGeoJSON in | 23.59 s | 22.41 s | 31.73 s | 42% |
| ogr2ogr parquet→fgb | 0.19 s | 0.17 s | 0.19 s | 8% |

Between two full runs an hour apart on the same idle laptop, tylertoo's
`madagascar-adm4` median moved 2.34 s → 1.61 s and the defaults ratio moved
10.4× → 12.5× (tippecanoe moved 23.93 s → 20.05 s over the same pair).
**A single-digit ratio measured on a laptop carries roughly ±20%.** That is
why "What to quote" above gives a band, and why no row here should be quoted
to two significant figures.

## 8. What this does not measure

- **Anything above 17k features.** The largest committed fixture is 28 MB.
  Tippecanoe's relative cost is known to change with density (the archived
  Moldova row in [`../overview/RESULTS.md`](../overview/RESULTS.md) had the
  old in-memory pipeline *losing* 3.6×). A cluster-scale run using
  `--dataset` is the missing half of this issue.
- **Points.** No point dataset is in the committed set, so tylertoo's
  clustering and tippecanoe's `-r` dot-dropping — the place where both tools'
  thinning is *supposed* to fire — are untested here.
- **Cold cache.** macOS cannot drop caches without root; everything is warm
  for both tools.
- **Remote input.** `--bbox`/`--filter` row-group pushdown over `s3://` is
  tylertoo's largest structural advantage over a GeoJSON or FlatGeobuf
  stream, and none of it is exercised: these fixtures are local and
  single-row-group.
- **Multi-core scaling.** Both tools were given all 12 cores; neither was
  pinned or swept.
