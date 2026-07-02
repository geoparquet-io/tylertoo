# Corpus metrics — definitions for V2 (quality) and V3 (benchmarks)

Stable definitions so the V2/V3 agents implement against a fixed
target. Every metric is computed per **corpus dataset** (see
`manifest.json`) and, where relevant, per **overview level**.

Terminology: an overview file has ordered **levels** coarse→fine.
Each level has a ground sample distance `gsd(z)` and (in duplicating
mode) is a self-contained generalized copy of the surviving features.
The "canonical" level holds the unmodified input geometry.

---

## 1. Per-level structural metrics (V2)

Computed by scanning the overview file's `level` column + footer.

| Metric | Definition | How to compute |
|--------|------------|----------------|
| `features_per_level[z]` | count of features present at level z | `SELECT level, count(*) GROUP BY level` (DuckDB) |
| `vertices_per_level[z]` | total coordinate count at level z | sum of `ST_NPoints(geometry)` per level |
| `mean_vertices_per_feature[z]` | vertices / features at z | derived |
| `level_band_bytes[z]` | on-disk compressed bytes of level z's row-group band | sum of row-group `total_compressed_size` for the level's RG range (parquet footer) |
| `dropped_fraction[z]` | 1 − features_per_level[z] / features_per_level[canonical] | derived |
| `vertex_reduction[z]` | 1 − vertices_per_level[z] / vertices_per_level[canonical] | derived |

Density histograms (for the human render gate):
- Per level, bin features into a fixed web-mercator grid at that
  level's zoom; report the distribution of features-per-cell
  (min/median/p95/max). Flags over-dense cells the overview failed
  to thin.

### Canonical-fidelity check (also a V1 correctness gate)
- `WHERE level = <canonical>` row count **must equal** input row
  count, and geometry/attributes must be value-equal to the input.
  Report as a boolean per dataset plus first mismatching feature id.

---

## 2. Comparison vs tippecanoe zoom outputs (V2)

The tippecanoe PMTiles goldens (`data/goldens/tippecanoe/<id>.pmtiles`,
flags in the sibling `.flags.txt`) are the baseline. For each zoom z
in the tippecanoe range, compare against our overview level whose
`gsd` maps to the same z (mapping per `OVERVIEWS_SPEC`,
`gsd(z) = 40075016.69 / 1024 / 2^z`):

| Metric | tippecanoe side | overview side |
|--------|-----------------|---------------|
| features at z | count features across all tiles at z (dedup by feature id where possible; note tippecanoe splits features across tiles) | `features_per_level[z]` |
| feature-count ratio | — | overview / tippecanoe |
| visual parity | rendered PNG per z (see V2 render script) | rendered PNG of level z |

Notes for the implementer:
- tippecanoe **clips** features to tile boundaries, so a single
  input feature can appear in many tiles; when counting "features at
  z" from PMTiles, dedup on the source feature id (`-l data`,
  preserve id) or count distinct ids, else the comparison is unfair
  to the overview (which stores whole features).
- Use the recorded flags verbatim; do not re-derive them. Different
  flags => different baseline.
- Report per-zoom count ratio as the primary regression-tracked
  number; render side-by-side PNGs for the human gate.

---

## 3. Storage metrics (V3)

Per dataset, compare four artifacts:

1. overview GeoParquet (our output)
2. plain gpio file (`data/gpio/<id>.parquet`)
3. gpio + PMTiles (tippecanoe golden)
4. COGP (`data/goldens/cogp/<id>.parquet`, if built)

| Metric | Definition |
|--------|------------|
| `total_bytes` | file size on disk |
| `bytes_per_feature` | total_bytes / canonical feature count |
| `overview_overhead` | overview_bytes / gpio_bytes − 1 (cost of embedding all levels) |
| `level_band_bytes[z]` | as in §1 (enables "pay for what you read") |

---

## 4. Access metrics — bytes-fetched-per-viewport (V3)

The headline benchmark: how many **bytes** and **HTTP requests** it
takes to render a viewport, reading only the needed level band ∩
bbox-pruned row groups. Measure over HTTP range requests
(localhost first, then real S3/R2), for three canonical viewports:

| Viewport | Definition | Target zoom | Example bbox |
|----------|------------|-------------|--------------|
| world    | whole-dataset extent | coarsest level | full dataset bbox |
| regional | ~one metro / admin unit | mid level | dataset-specific (e.g. one county) |
| street   | ~1-2 km² | canonical / finest | small bbox inside the densest area |

For each (dataset × viewport × artifact) record:

| Metric | Definition |
|--------|------------|
| `bytes_fetched` | total bytes pulled over the wire to satisfy the viewport (sum of HTTP range-request response bodies) |
| `request_count` | number of HTTP range requests issued |
| `wall_time_ms` | end-to-end time to first renderable feature set |
| `features_returned` | features decoded for the viewport |
| `bytes_per_feature_fetched` | bytes_fetched / features_returned |

Methodology (must be identical across artifacts for fairness):
- Fix the three viewport bboxes **per dataset** in a small config so
  runs are reproducible; derive them from the dataset bbox (world =
  full; regional = centered 1/8 linear extent; street = centered
  fixed 0.02° box over the densest cell from §1 histograms).
- Overview: footer → select level band by target zoom's gsd →
  bbox-prune row groups via covering stats → issue range requests
  for surviving row groups only. Count exactly those bytes/requests.
- PMTiles: resolve the tiles covering the viewport bbox at the
  target zoom → range-request those tile blobs. Count bytes/requests.
- COGP: prefix-read per its level layout for the target zoom.
- Warm vs cold: report cold (empty cache) numbers as primary; a
  warm pass optional. Pin the object (no CDN variance) or use a
  local range-serving stub for localhost runs.
- Repeat N=5, report median + p95 for wall time; bytes/requests are
  deterministic given fixed viewports.

---

## 5. Write metrics (V3)

Per dataset, overview conversion vs tippecanoe on the same input:

| Metric | Definition |
|--------|------------|
| `convert_wall_time_s` | end-to-end conversion time |
| `peak_rss_mb` | peak resident memory (`/usr/bin/time -v` or dhat/heaptrack) |
| `throughput_features_per_s` | canonical features / convert_wall_time_s |

Compare against tippecanoe building its PMTiles golden from the same
gpio input, using the recorded flags.

---

## 6. Reporting

- Emit one machine-readable results file per run
  (`benchmarks/overview/results-<date>.json`) keyed by
  `dataset_id → {structural, storage, access, write}` so numbers are
  regression-tracked over time.
- V2 additionally emits the side-by-side PNG grid for the human gate.
- Always record: overview file version, Overture release, tool
  versions (tippecanoe, cogp, duckdb, gpio), and host (localhost vs
  S3/R2) alongside every result.
