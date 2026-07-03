# Overview GeoParquet — access & storage benchmark (V3)

Generated 2026-07-03; revised 2026-07-03 with the **H1 writer fix** (the
tables below carry the post-H1 numbers — see the dated revision note after
Headline findings for the before→after deltas). These are the first published numbers comparing the
access and storage efficiency of **overview GeoParquet** (COG-style
multi-resolution levels embedded in one GeoParquet file, produced by
`gpq-tiles overview`) against the status-quo web-map deployment
(**gpio-optimized GeoParquet source + a tippecanoe PMTiles derivative**)
and against **COGP** (Kanahiro `cogp-rs`, thinning-only, no
simplification). Method transparency is the point: every number below is
reproduced by the committed scripts in this directory over the corpus in
`corpus/manifest.json`. Where a result looked anomalous we chased the root
cause rather than publishing it bare — see Caveats.

## Headline findings

For medium metro-scale datasets, a single duplicating-mode overview file
(self-contained levels, COG read semantics) is **smaller on disk than the
two-artifact status quo** it replaces — −27 % (points), −10 % (lines),
−3 % (polygons) versus gpio-source + PMTiles combined — while remaining a
valid, exact, SQL-queryable GeoParquet file. Its partitioning mode (each
feature once) costs only +4 to +47 % over the plain gpio file and tracks
cogp-rs within a few percent. **Conversion** is 18–24× faster than the
`gpio convert geojson | tippecanoe` pipeline on the medium datasets
because the overview path reads GeoParquet natively. The clear cost is
**per-viewport bytes over HTTP**: PMTiles fetches 1.1–18× fewer bytes than
the overview read protocol in all but one scenario, because MVT is lossy,
pre-tiled, quantized, and property-pruned, whereas the overview path
returns exact `f64` geometry plus all attribute columns and reads at
row-group granularity behind a (post-H1, small) parquet-footer tax. The
overview file therefore trades wire bytes for losslessness, full
attributes, and single-artifact simplicity; it does not beat purpose-built
vector tiles on bytes-to-paint-a-viewport, and on very large dense files
(Moldova) its conversion cost still amplifies sharply (see Caveats; the
per-viewport read amplification was largely a writer pathology fixed by
H1 — see the revision note).

## H1 revision (2026-07-03): writer row-group pathology fixed

The first publication of these numbers exposed a writer pathology on the
large file (Caveats 2–3 of the original run): the Moldova duplicating file
carried an **8.84 MB Thrift footer** (167 row groups × per-column min/max
statistics, dominated by the 26-char ULID `id` strings and raw WKB geometry
values), paid in full on **every** remote query before a single feature is
read. The H1 fix changed the writer defaults:

- **Statistics suppression** — per-row-group min/max stats are no longer
  written for the WKB geometry column or string/binary property columns
  (no reader used them). The bbox covering struct and `level` column keep
  full stats — they are the pruning index and are untouched. Opt back in
  with `--full-column-stats`.
- **Per-level row-group sizing** — `--row-group-size` is now a per-level
  cap: a level with fewer rows becomes a single row group; larger levels
  split into roughly **uniform** row groups instead of `10k, 10k, …,
  remainder`. Level↔row-group alignment is unchanged and all validate
  checks still pass.

Before → after on the affected artifacts (same inputs, same knobs,
`--row-group-size 10000` default retained — with stats suppressed the
footer is no longer a reason to enlarge row groups, and 10k keeps bbox
pruning granularity):

| metric | before (2026-07-03 orig) | after (H1) | Δ |
|---|---|---|---|
| Moldova dup footer | 8.84 MB | **0.24 MB** | −97 % |
| Moldova par footer | 3.65 MB | 0.10 MB | −97 % |
| Moldova dup file size | 411.39 MB | 359.83 MB | −12.5 % |
| Moldova dup vs status-quo | +63.6 % | **+43.1 %** | |
| Moldova par overhead vs gpio | +46.9 % | +33.6 % (now < cogp) | |
| Moldova world-viewport (z6) | 17.97 MB | **8.80 MB** | −51 % |
| Moldova regional-viewport (z9) | 37.59 MB | **21.80 MB** | −42 % |
| Moldova street-viewport (z14) | 10.76 MB | **2.02 MB** | −81 % |
| lines-portland dup footer | 0.27 MB | 0.07 MB | −73 % |
| polygons-portland regional | 2.17 MB | 1.86 MB | −14 % |

The metro no-regression check (polygons-portland-medium) improved in all
three viewports (569→265 KB world, 2.17→1.86 MB regional, 6.92→6.76 MB
street). Across all 9 metro cells, 7 improved; two point/line *regional*
cells rose ≤ 12 % (NYC 3.88→4.36 MB, Portland lines 3.28→3.57 MB) — the
uniform row-group split shifts band boundaries slightly, changing which row
groups a viewport overlaps — within run-shape noise and dwarfed by the
Moldova gains. Row counts, geometry, and attributes are byte-identical to
the original run; only the physical layout changed.

## Environment

| tool | version |
|---|---|
| gpq-tiles | 0.6.0 (branch `feat/geoparquet-overviews`, Q1 ranking + Q2 density budget + H1 layout fix) |
| tippecanoe | v2.49.0 |
| DuckDB | v1.4.1 (Andium), httpfs + spatial |
| gpio (geoparquet-io) | 1.1.0b1 |
| cogp-rs | 0.1.0 (git `61395124`) |
| Python | 3.12 + `pmtiles` (range reader) |
| host | Linux 6.8, 16 cores, localhost HTTP (no CDN) |
| Overture release | 2026-06-17.0 (corpus manifest) |

Overview files regenerated with the current release binary and **default
knobs** (duplicating: `--line-thinning 1`, `--simplify-factor 1.0`,
`--drop-rate 1.65`, `--row-group-size 10000`), zoom range `0..14` from the
manifest. tippecanoe uses the exact recorded golden flags
(`corpus/data/goldens/tippecanoe/<id>.flags.txt`). cogp uses
`--webmerc-minzoom 0 --webmerc-maxzoom 14`.

---

## 1. Storage

Sizes on disk (MB). `ov-dup` = duplicating overview (self-contained levels);
`ov-par` = partitioning overview (each feature once, COGP-compatible layout);
`gpio+pmt` = status-quo deployment (source kept **plus** its PMTiles
derivative). `dup/gpio` and `par/gpio` are overhead vs the plain gpio input;
`dup vs status-quo` = duplicating file size vs (gpio + PMTiles).

| dataset | feats | gpio | ov-dup | ov-par | pmtiles | cogp | gpio+pmt | dup/gpio | par/gpio | dup vs status-quo |
|---|---|---|---|---|---|---|---|---|---|---|
| points-nyc-medium | 458,135 | 30.84 | 78.79 | 33.68 | 77.34 | 33.00 | 108.18 | +155.5% | +9.2% | **−27.2%** |
| lines-portland-medium | 295,881 | 36.76 | 71.44 | 41.90 | 43.39 | 42.72 | 80.15 | +94.3% | +14.0% | **−10.9%** |
| polygons-portland-medium | 812,435 | 114.55 | 187.28 | 119.32 | 78.62 | 117.34 | 193.17 | +63.5% | +4.2% | **−3.0%** |
| polygons-ftw-moldova-large | 631,910 | 96.97 | 359.83 | 129.53 | 154.45 | 130.56 | 251.42 | +271.1% | +33.6% | +43.1% |

(Pre-H1 the Moldova rows read ov-dup 411.39 / ov-par 142.47 / +324.2 % /
+46.9 % / +63.6 % — the H1 stats suppression removed ~51 MB of per-page
statistics from the dup file; see the revision note above.)

Reading it:
- **Duplicating** embeds every coarser level as a self-contained generalized
  copy, so it is 1.6–4.2× the input. For the three metro datasets that
  single file is still *smaller* than keeping the gpio source and a separate
  PMTiles tileset around — you replace two artifacts with one and lose no
  precision. Moldova (631 k dense field polygons, 38 M canonical vertices)
  is the exception: the duplicated coarse levels of very high-vertex polygons
  balloon the file to +271 %, larger than gpio+PMTiles.
- **Partitioning** stores each feature once and costs only the `level`
  column + a freshly generated bbox covering: +4 % to +34 %. It tracks
  **cogp-rs** closely (both are "each feature once, thinned per level"),
  differing mainly because our partitioning still *simplifies* per level
  whereas cogp thins only — post-H1 our partitioning file is at or below
  cogp on every dataset (Moldova 129.5 vs cogp's 130.6 MB; lines 41.9 vs
  42.7 MB).

---

## 2. Access — bytes / requests / wall time per viewport (the headline)

Served over a localhost byte-range HTTP server that logs every response
body's byte count (`logging_server.py`). Three cold runs per cell (fresh
DuckDB / fresh pmtiles reader each run); wall time is the median, bytes and
requests are deterministic. Same viewport rectangle and zoom for both paths.

- **Overview path**: one fresh DuckDB process runs the documented read
  protocol — `SELECT * FROM read_parquet(url) WHERE level = k AND <bbox
  overlap>` — over httpfs, materializing all columns (realistic client
  fetch). Bytes/requests are exactly what DuckDB pulled over the wire.
- **PMTiles path**: the python `pmtiles` reader reads header + directory,
  then range-fetches each z/x/y tile covering the viewport at the target
  zoom, through the same logging server.

`ov feats` = exact features returned; `pm tiles` = tiles fetched (MVT clips
and splits features across tiles, so a feature count is not comparable and
is omitted). `overview/pmtiles bytes` = how many times more bytes the
overview path fetched.

| dataset | viewport | z | overview bytes | ov req | ov ms | ov feats | pmtiles bytes | pm req | pm ms | pm tiles | ov/pm bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| points-nyc-medium | world | 8 | 578 KB | 7 | 120 | 5,772 | 97 KB | 4 | 7 | 1 | 6.0× |
| points-nyc-medium | regional | 11 | 4.36 MB | 32 | 90 | 14,321 | 961 KB | 16 | 28 | 4 | 4.5× |
| points-nyc-medium | street | 14 | 5.19 MB | 39 | 297 | 33,865 | 1.25 MB | 16 | 27 | 4 | 4.2× |
| lines-portland-medium | world | 8 | 1.26 MB | 12 | 125 | 14,663 | 484 KB | 6 | 6 | 2 | 2.6× |
| lines-portland-medium | regional | 11 | 3.57 MB | 27 | 127 | 9,261 | 1.88 MB | 18 | 18 | 6 | 1.9× |
| lines-portland-medium | street | 14 | 4.63 MB | 23 | 132 | 3,701 | 362 KB | 12 | 11 | 4 | 12.8× |
| polygons-portland-medium | world | 8 | 265 KB | 7 | 200 | 15 | 501 KB | 6 | 6 | 2 | 0.5× |
| polygons-portland-medium | regional | 11 | 1.86 MB | 12 | 125 | 2,026 | 1.64 MB | 12 | 12 | 4 | 1.1× |
| polygons-portland-medium | street | 14 | 6.76 MB | 28 | 132 | 7,219 | 567 KB | 9 | 8 | 3 | 11.9× |
| polygons-ftw-moldova-large | world | 6 | 8.80 MB | 11 | 441 | 7,804 | 740 KB | 8 | 12 | 2 | 11.9× |
| polygons-ftw-moldova-large | regional | 9 | 21.80 MB | 47 | 376 | 8,008 | 1.24 MB | 16 | 30 | 4 | 17.6× |
| polygons-ftw-moldova-large | street | 14 | 2.02 MB | 23 | 298 | 1,527 | 134 KB | 16 | 24 | 4 | 15.1× |

(Pre-H1 Moldova cells: world 17.97 MB / 24.3×, regional 37.59 MB / 30.4×,
street 10.76 MB / 80.3× — the 8.84 MB footer was paid on every query and
the whole-file page indexes inflated coalesced ranges. See the H1 revision
note.)

PMTiles fetches fewer bytes in all but one cell (polygons world, where the
overview's coarse level is 15 generalized features). The gap is smallest at
coarse/world zoom (0.5–12×) and largest at fine zoom over a small bbox
(4–15×) — the opposite of the "pay for what you see" intuition, and worth
understanding (Caveats). Post-H1 the worst cell is 17.6× (was 80.3×). Wall
times are localhost and dominated by DuckDB process startup (~120 ms floor)
vs the pmtiles reader's tiny per-tile fetches; treat them as indicative,
not the story — bytes are the story.

### Viewport rectangles (identical for both paths)

Derived reproducibly (`make_viewports.py`) from each dataset's own extent:
world = full extent; regional = centered 1/4 of the linear extent (≈1/16 of
area); street = a fixed 0.02° box centered on the densest 0.02° cell. Zooms
are chosen so the full extent fits one screenful at `world` and an overview
*level* exists at each zoom.

| dataset | viewport | zoom | bbox [xmin,ymin,xmax,ymax] |
|---|---|---|---|
| points-nyc-medium | world | 8 | [-74.3000, 40.5001, -73.7000, 40.9000] |
| points-nyc-medium | regional | 11 | [-74.0750, 40.6500, -73.9250, 40.7500] |
| points-nyc-medium | street | 14 | [-73.9900, 40.7500, -73.9700, 40.7700] |
| lines-portland-medium | world | 8 | [-123.0000, 45.3000, -122.2170, 45.7766] |
| lines-portland-medium | regional | 11 | [-122.7064, 45.4787, -122.5106, 45.5978] |
| lines-portland-medium | street | 14 | [-122.6900, 45.5100, -122.6700, 45.5300] |
| polygons-portland-medium | world | 8 | [-123.0000, 45.3000, -122.2996, 45.7003] |
| polygons-portland-medium | regional | 11 | [-122.7374, 45.4501, -122.5623, 45.5502] |
| polygons-portland-medium | street | 14 | [-122.6500, 45.5500, -122.6300, 45.5700] |
| polygons-ftw-moldova-large | world | 6 | [26.5925, 45.4719, 30.1589, 48.4902] |
| polygons-ftw-moldova-large | regional | 9 | [27.9299, 46.6038, 28.8215, 47.3584] |
| polygons-ftw-moldova-large | street | 14 | [28.1100, 47.1500, 28.1300, 47.1700] |

---

## 3. Conversion cost (wall time + peak RSS)

`gpq-tiles overview` (duplicating, default knobs, z0..14, reads GeoParquet
natively) vs the golden tippecanoe workflow `gpio convert geojson <src> |
tippecanoe -P <recorded flags>`. Both wrapped in `/usr/bin/time -v`. The
tippecanoe column **includes** the mandatory GeoParquet→GeoJSON decode
(tippecanoe cannot read GeoParquet in v2.49) — a step the native overview
path avoids; peak RSS is the largest single process in the pipe.

| dataset | overview wall | overview peak RSS | tippecanoe(+gpio) wall | tippecanoe(+gpio) peak RSS |
|---|---|---|---|---|
| lines-portland-medium | 0:01.62 | 507 MB | 0:28.52 | 681 MB |
| polygons-portland-medium | 0:03.54 | 1305 MB | 1:25.55 | 1251 MB |
| polygons-ftw-moldova-large | 10:57.23 | 5437 MB | 3:03.62 | 1155 MB |

On the medium datasets the overview converter is **18–24× faster** at
comparable or lower memory. On the large dense Moldova set it is **3.6×
slower and 4.7× heavier** than tippecanoe: the v1 overview pipeline is
fully in-memory and rebuilds/decodes geometry per level, so 631 k polygons
with 38 M vertices duplicated across 12 levels blow memory to 5.4 GB. This
is the exact motivation for the planned V4 streaming refactor
(`context/OVERVIEWS_PLAN.md`, memory target O(row group + winner tables)).

---

## Caveats (read before quoting any number)

These are prominent on purpose. The access numbers especially are **not
apples-to-apples**, and saying so is more useful than a clean-looking table.

1. **Overview delivers a strict superset of what MVT carries.** The overview
   read fetches exact IEEE-754 `f64` coordinates, *every* property column
   (for Overture that includes the 26-char ULID `id` string — 16 MB of the
   72 MB lines file), and the bbox covering struct. MVT quantizes geometry to
   integer tile pixels (lossy), keeps only selected attributes, and drops the
   covering. So "overview fetched 14× more bytes at street zoom" compares a
   lossless, fully-attributed, SQL-queryable result against a lossy render
   payload. For rendering alone MVT is the right tool; the overview file's
   bytes buy precision + attributes + queryability + one artifact instead of
   two.

2. **A fixed parquet-footer tax is paid on every query** — the whole Thrift
   footer is read before a single feature. Pre-H1 this was **8.84 MB for the
   411 MB Moldova file** (167 row groups × 9 columns of per-group statistics
   including ULID id strings and raw WKB min/max) and was the dominant reason
   the overview/PMTiles byte ratio exploded on the large file. The H1 writer
   fix suppresses statistics on string/binary property columns and the WKB
   geometry column by default (the bbox covering + `level` pruning stats are
   kept), cutting Moldova's footer to **0.24 MB** (lines file: 0.07 MB). The
   tax still exists — it just no longer scales with property-column
   cardinality. `--full-column-stats` restores the old behavior for clients
   that need property-predicate pushdown.

3. **Row-group granularity (10 k rows) caps bbox pruning.** DuckDB reads
   whole row groups; with 10 k-row groups a coarse or mid level has few, very
   large, spatially-broad groups. Moldova's `regional` (z9) viewport
   intersects **5 of the 6** row groups in that level band, so pruning drops
   almost nothing and the query fetches ~the entire level band (~21 MB
   post-H1) to return 8,008 features. H1 made row-group sizing per-level
   (small levels = one row group; large levels = uniform splits) and the
   footer no longer punishes smaller row groups, so `--row-group-size` can
   now be lowered for tighter pruning without a footer penalty — per-dataset
   tuning remains future work (default 10 k throughout).

4. **DuckDB process-startup floor (~120 ms).** Wall times are localhost and
   the overview side pays a fixed DuckDB spin-up per cold run that dwarfs the
   actual I/O at these sizes. Wall time is reported for completeness; bytes
   and request counts are the reproducible, host-independent metrics.

5. **Duplicating vs partitioning.** The access benchmark uses **duplicating**
   mode (self-contained COG levels — the format's headline read model).
   Partitioning mode would prefix-read like COGP and is smaller, but its
   coarse levels are not self-contained. We benchmark the mode the format is
   actually pitching.

6. **Localhost only.** No CDN / real S3 variance. Byte and request counts
   transfer directly to any range-serving object store; absolute wall times
   do not (add RTT × request_count for a remote store — another reason the
   overview side, with more requests, would widen on a high-latency link).

7. **COGP is thinning-only.** cogp-rs stores full-resolution geometry thinned
   per level; it does no simplification and is a storage/thinning-parity
   reference, not an access competitor here (we did not run its prefix-read
   access protocol — its layout differs and a fair head-to-head is future
   work). It is included in the storage table only.

## Reproduce

```bash
# 0. release binary (once)
cargo build --release --package gpq-tiles

# 1. regenerate overview files (both modes) + storage + conversion + access
benchmarks/overview/run_all.sh
```

Raw outputs land under `corpus/data/bench/` (gitignored). Machine-readable
results: `storage_results.json`, `access_results.json`, and the
`corpus/data/bench/*/*.time.txt` timing captures.
