# `geo:overviews` — COG-style overviews inside GeoParquet

*One-pager for the GeoParquet spec maintainers. Draft 2026-07-03 — for
Nissim's review before sending. Working code, spec, and benchmarks:
https://github.com/geoparquet-io/gpq-tiles/pull/168*

## The gap

GeoParquet has no multi-resolution story. COG solved this for raster
with embedded overviews; for vector, the ecosystem answer is a second,
derived artifact (PMTiles) that drifts from the source of truth, or
dynamic tiling with compute in the loop. Brandon Liu named the problem
("Where is COG for Vector?", 2023); Chris has floated
overviews-in-the-format at least twice since 2022. Nobody has shipped
it in the open — ESRI just shipped a proprietary version (June 2026
"Spatially Optimized Parquet": per-scale geometry columns + a closed
index, JS-SDK-only), which validates demand and raises the cost of the
open ecosystem not having an answer.

## The proposal

A GeoParquet file that carries its own zoom pyramid and remains 100%
valid GeoParquet 1.1 (2.0-ready):

- **Levels as rows.** Each overview level is a generalized copy of the
  dataset (thinned, simplified, rank-aware — tippecanoe-grade
  generalization, which is the hard part and is implemented). The
  finest level is the source data, value-identical.
- **Layout.** Levels ordered coarse→fine; each level ends on a
  row-group boundary; Hilbert-sorted within levels; standard bbox
  covering. A renderer range-requests one small contiguous level band
  (+ footer); an analyst adds `WHERE level = <canonical>` — one
  predicate, exact data.
- **Metadata.** One footer key, `geo:overviews` (JSON: levels →
  row-group ranges, GSD/zoom, mode, canonical pointer), designed to
  merge verbatim into the `geo` metadata as an official extension —
  the `covering` path into 1.1. Two modes: `duplicating` (COG
  semantics, self-contained levels) and `partitioning`
  (feature-once prefix reads; concept-compatible with Kanahiro's
  COGP draft, optional interop key).
- **Works today, no reader changes:** DuckDB/GDAL/pyarrow read it now;
  level selection is a plain column predicate; pruning uses ordinary
  row-group stats.

## Evidence (all reproducible in-repo; 4 real datasets incl. 632k
Overture-derived parcels)

- **Storage vs the status quo it replaces** (gpio source + tippecanoe
  PMTiles): points −27%, lines −10%, polygons −3% — one exact,
  queryable file instead of two artifacts. Partitioning mode: +4–47%
  over plain source, at or below cogp-rs on like inputs.
- **Conversion:** 18–24× faster than the GeoJSON→tippecanoe pipeline
  on metro datasets (native GeoParquet read).
- **Access (honest):** for pure map rendering, MVT still fetches
  1.1–14× fewer bytes per viewport (quantized, property-pruned vs
  exact + full attributes). Answer in the same tool:
  `export-pmtiles` derives a PMTiles from the overview file in
  seconds (Portland z2–14: 5s, 244 MB RSS) — the tileset becomes a
  cheap projection of the canonical file, not a drifting sibling.
- Validator (`gpq-tiles validate`) enforces every layout/metadata
  conformance rule; correctness suite passes on all datasets ×
  both modes incl. byte-determinism and canonical fidelity.

## The ask

1. Is `geo:overviews` → `geo.overviews` the right incubation → merge
   path, and is the key name right?
2. Feedback on the two-mode design and the canonical-level analysis
   convention (the one deliberate cost: naive `SELECT count(*)`
   over-counts unless filtered — mitigations documented).
3. If the shape looks right: where should the spec text live while it
   incubates (gpio org? cloudnativegeo?), and what would you want to
   see before an extension proposal is worth opening against
   opengeospatial/geoparquet?

*Spec: `context/OVERVIEWS_SPEC.md` · Benchmarks + method:
`benchmarks/overview/RESULTS.md` · Tuning: `docs/OVERVIEW_TUNING.md`*
