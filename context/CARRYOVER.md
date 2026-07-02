# Codebase Audit & Carry-Over Map (Task G2)

Status: draft, 2026-07-02. Branch `feat/geoparquet-overviews`.
Companion to `context/OVERVIEWS_PLAN.md` and `context/OVERVIEWS_SPEC.md` (G1).

This document audits every module in `crates/core/src/` and maps each to a
**fate** for the new GeoParquet multi-resolution overviews pipeline:

- **REUSE** — usable as-is (or with trivial rename of "zoom"→"level").
- **ADAPT** — core logic is valuable but carries tile-space assumptions
  (0–4096 pixel extent, per-`TileCoord` transforms, per-tile budgets) that
  must be stripped and replaced with world-space / per-level equivalents.
- **SHELVE** — inherently tile/MVT/PMTiles-specific; kept intact for the
  future `gpq-tiles serve` MVT bridge (Plan E1), not imported into the
  overview path.
- **DEAD** — candidate for removal (none recommended yet; the tile pipeline
  is being shelved, not deleted).

**Overview pipeline shape** (for reference): read gpio-sorted GeoParquet →
per-level grid cell-winner thinning → per-level geometry simplification in
**world space** (tolerance from level GSD) → level-banded GeoParquet writer.
No tile clipping. No MVT encoding. No PMTiles.

Compile status: **`cargo check --package gpq-tiles-core` passes clean** (no
warnings blocking). The two `#[ignore]`d tests (`covering.rs`,
`pipeline.rs`) are large-file tests needing multi-GB downloads, **not**
broken tests. `DIVERGENCE FROM TIPPECANOE` comments are design docs, not bug
markers.

---

## Summary table

| Module | Status | Fate | One-line note |
|---|---|---|---|
| lib.rs | working | ADAPT | Public API + `Converter` (tile path). Add `overview` module + `OverviewConfig`; keep tile API for serve. |
| tile.rs | working | ADAPT/SHELVE | `TileCoord`/tiling = shelve; `TileBounds` (plain lat/lng bbox) reused widely — keep. |
| world_coord.rs | working | ADAPT | lng/lat↔u32 projection + `WorldBounds` reusable; `to_tile*`/extent methods shelve. |
| spatial_index.rs | working | REUSE | Hilbert/Z-order encode + `sort_features`; generic spatial ordering. |
| wkb.rs | working | REUSE | Geometry↔WKB + property (de)serialize for disk spill; no tile logic. |
| compression.rs | working | REUSE | ZSTD/gzip byte codec; ZSTD used by overview writer. |
| memory.rs | working | REUSE | `MemoryTracker`/`RssTracker`; streaming budget for V4. |
| quality.rs | working | REUSE | GeoParquet input assessment + WGS84/CRS check; metadata-only. |
| property_filter.rs | working | REUSE | Attribute include/exclude; generic column selection. |
| sampling.rs | working | REUSE | `BoundedSampler` percentile estimation; per-level threshold picking. |
| gap_density.rs | working | REUSE | Hilbert-gap thinning in world coords; per-level density selection. |
| accumulator.rs | working | REUSE | Property aggregation for merging cell losers into winner. |
| simplify.rs | working | ADAPT | **All fns route through tile-local pixel space** — needs true world-space RDP + GSD tolerance. Core carry-over target for P2. |
| validate.rs | working | ADAPT | f64 `validate_*` reusable (post-simplify degeneracy); `world_coord_in_tile*` half is tile-pixel. |
| feature_drop.rs | working | ADAPT | Point-thinning `retention_rate`/hash reusable; polygon/line/grid drops are per-tile pixel budgets. |
| clustering.rs | working | ADAPT | World-space Hilbert clustering; only `cluster_gap` gap formula is zoom/tile-phrased. (V5) |
| coalesce.rs | working | ADAPT | `coalesce_geometries` (Multi* merge) reusable; `SpatialGrid`/density-targeting tile-based. (V5) |
| external_sort.rs | working | ADAPT | k-way merge engine reusable; `TileFeatureRecord` schema tile-keyed → re-key by `(level,cell,hilbert)`. |
| sutherland_hodgman.rs | working | SHELVE | Rectangle clip; generic algo but typed to tile bounds. serve bridge. |
| ioverlay_clip.rs | working | SHELVE | Robust polygon∩rectangle (i_overlay); serve bridge. |
| clip.rs | working | SHELVE | Per-tile clip dispatcher (SH + ioverlay fallback). serve bridge. |
| hierarchical_clip.rs | working (has TODO) | SHELVE | Cross-zoom parent/child clip reuse; deeply tile-centric. serve bridge. |
| mvt.rs | working | SHELVE | MVT encoding in 0–4096 pixel space. serve bridge (E1). |
| pmtiles_writer.rs | working | SHELVE | PMTiles v3 archive writer. serve bridge. |
| dedup.rs | working | SHELVE | XXH3 tile-blob dedup; PMTiles run-length semantics. serve bridge. |
| covering.rs | working (ignored file-test) | REUSE | GeoParquet 1.1 bbox covering parse + row-group bbox pruning; reader needs it. |
| batch_processor.rs | working | ADAPT | GeoParquet **reader** (row-group streaming). Reuse read path; drop tile-clip callback shapes. |
| pipeline.rs | working but stuck | SHELVE | 236KB tile-generation orchestrator (external-sort + clip + MVT + budgets). The stuck subsystem. serve bridge only. |
| adaptive.rs | working | SHELVE | Per-zoom MVT tile-size retry thresholds; not needed single-pass. |
| golden.rs | test-only | SHELVE | Tile golden tests (IoU vs tippecanoe). serve bridge. |
| integration_tests.rs | test-only | SHELVE | End-to-end tile pipeline tests. |

---

## Module-by-module

### lib.rs — ADAPT
Crate root: declares all modules, defines the public `Error`/`Result`,
`Config`, `DropDensity`, and the `Converter` struct whose `convert(input,
output)` runs the full GeoParquet→PMTiles tile pipeline
(`generate_tiles_with_bounds` → `PmtilesWriter`). Re-exports the tile-path
public API (accumulator, clustering, coalesce, covering, pmtiles writer,
simplify, processing-mode). **Fate:** add `pub mod overview;` and an
`OverviewConfig` + entry function (`convert_to_overview`); leave the existing
tile `Converter` intact for the serve bridge. No tile-space logic lives here
directly.

### tile.rs — ADAPT (split)
Web-Mercator tile math: `TileCoord{x,y,z}` (`bounds`, `parent`, `children`),
`lng_lat_to_tile`, `tile_bounds`, `tiles_for_bbox`, and the plain
`TileBounds{lng_min,lat_min,lng_max,lat_max}` lat/lng bbox value type. The
`TileCoord`/tiling functions are **SHELVE**, but `TileBounds` is a generic
bbox depended on across the crate (covering, quality, batch_processor) — keep
it (possibly re-home as a neutral `Bounds`). The overview grid math wants
level-GSD → world-cell size, which is new code, not `TileCoord`.

### world_coord.rs — ADAPT (split)
Tippecanoe-parity 32-bit integer world coordinates (zoom-0 world spans
[0,2^32), NW origin, Y southward, `MAX_LATITUDE` 85.05°). `WorldCoord`,
`lng_lat_to_world`/`world_to_lng_lat`, and `WorldBounds`
(contains/intersects/width/height) are generic coordinate-quantization
primitives — **reusable** for a projected-space grid. The tile-oriented
methods (`to_tile`, `to_tile_local(tile,extent)`, `from_tile_with_buffer`)
are tile-space and shelve. Note the world grid here is Web-Mercator-pixel
based; overview GSD is defined in meters (cogp convention) — reconcile in P1.

### spatial_index.rs — REUSE
Space-filling-curve ordering: `encode_hilbert`/`encode_zorder` (+ decode) on
u32 world coords, `lng_lat_to_world_coords`, `sort_by_spatial_index`,
`sort_geometries`, `sort_features` (over `FeatureRecord`). Pure spatial
ordering, no tile output assumptions. Overview path uses it to (a) verify /
fall-back-sort the "Hilbert order within level" layout rule, and (b) as the
stable tiebreaker in cell-winner priority. Depends on `world_coord` u32
projection.

### wkb.rs — REUSE
`geometry_to_wkb`/`wkb_to_geometry` + `PropertyValue` /
`serialize_properties`/`deserialize_properties` for spilling features to temp
files under memory pressure. No tile/MVT coupling. Reused by the streaming
overview refactor (V4) for disk-backed cell-winner tables.

### compression.rs — REUSE
`Compression{None,Gzip,Brotli,Zstd}` + `compress(data, kind)`. The numeric
codes match PMTiles v3 but the byte-in/byte-out interface is generic. Overview
writer uses **ZSTD** for column data (spec G1). Reuse directly.

### memory.rs — REUSE
`estimate_geometry_size`, `MemoryTracker` (budget/current/peak),
`RssTracker` (actual RSS sampling; the #152 fix), `MemoryStats`. Generic
streaming instrumentation; directly reused for the V4 bounded-memory target
(O(row group + winner tables)).

### quality.rs — REUSE
Input GeoParquet assessment: `assess_quality`, `extract_crs`,
`validate_wgs84`, `emit_quality_warnings`, `MIN_RECOMMENDED_ROWS_PER_GROUP`.
Inspects geo metadata / row-group bboxes / Hilbert-sortedness / CRS. Metadata
only, no geometry processing. The overview converter should call this on input
(the "assume gpio-sorted" contract check). One `TODO` (row-group bbox overlap
check unimplemented) — cosmetic.

### property_filter.rs — REUSE
`PropertyFilter{None,Include,Exclude,ExcludeAll}` + `should_include`/
`filter_fields`. Pure attribute-name selection. Reused if the overview
converter offers column pruning (though spec default preserves input schema).

### sampling.rs — REUSE
Generic `BoundedSampler<T: Ord+Copy>` with tippecanoe incremental-halving
(100K cap) percentile threshold selection; aliases `GapSampler`,
`ExtentSampler`. No geometry/tile logic. Reused to pick per-level
gap/density thresholds over huge feature counts (P1 / V4).

### gap_density.rs — REUSE
Tippecanoe `manage_gap` Hilbert-gap density selection operating purely on
**world-coordinate** Hilbert indices: `GapBasedSelector`,
`select_features_by_gap(features, target_count, gamma)`, `choose_mingap`,
`percentile`, `scale_for_zoom` (= 2^(32−z) normalization). This is
distribution-preserving feature thinning that already works in world space —
a strong candidate for per-level thinning (alternative/complement to grid
cell-winner). Just rename "zoom"→"level".

### accumulator.rs — REUSE
`AccumulatorOp{Sum,Product,Mean,Max,Min,Concat,Comma,Count}` +
`AccumulatorConfig::accumulate(target, source)` over `PropertyValue` maps.
Pure property math. Reusable to merge dropped cell-losers' attributes into
the surviving winner (V5 aggregation), or left unused for v1 (spec: canonical
level geometry/attrs byte-equal to input).

### simplify.rs — ADAPT (primary P2 target)
Douglas-Peucker simplification. **Every public function ultimately transforms
to tile-local pixel space, simplifies with a pixel tolerance, and transforms
back** — including the deceptively named `simplify_world_linestring`/
`simplify_world_ring`, which take a `TileCoord`+`extent`+`pixel_tolerance` and
call `world_coords_to_tile_linestring` internally. Public API:
`simplify_for_zoom` (deprecated, degree tolerance), `simplify_in_tile_coords`,
`simplify_coalesced_linestring`, `remove_noop_multilinestring`,
`is_on_tile_boundary`, `simplify_world_*` / `*_preserve_boundaries`,
`simplify_geometry_for_tile` (the unified entry, re-exported from lib),
`world_simplified_vertex_count`, `simplify_to_tile_coords`.
**Tile-space assumptions to strip for overviews:** (1) the `TileCoord`+extent
pixel transform — overview simplification must run RDP directly on the input
CRS geometry with `tolerance = GSD(level)` (world units); (2) the tile-boundary
preservation logic (`is_on_tile_boundary`, `*_preserve_boundaries`) is
meaningless without tile seams and should be dropped for the level path; (3)
per-feature, no `TileBounds` context. **Carry over:** the RDP call itself
(geo's `.simplify`), the ring-closure/degenerate guards, and the multi-ring
dispatch — wrapped in a new `overview::simplify(geom, tolerance_world)` that
takes no tile. Depends on `tile`, `world_coord`, `validate`.

### validate.rs — ADAPT (split)
Degenerate-geometry detection after simplification. The **f64 side**
(`is_valid_geometry`, `validate_geometry`, `validate_{linestring,polygon,...}`,
`filter_valid_geometry`, `MIN_POLYGON_RING_POINTS` etc.) is geometry-generic
and **reusable** to drop features that collapse after world-space simplify /
below a level visibility gate. The **WorldCoord side** (`world_coord_in_tile*`,
`validate_world_ring(coords, tile, extent)`, `is_degenerate_in_tile`) computes
degeneracy in tile-local pixels and is tile-specific — shelve or repoint at a
per-level world resolution.

### feature_drop.rs — ADAPT (mixed)
Tippecanoe feature dropping. **Reusable:** point-thinning `retention_rate`
(1/2.5 per level), `should_drop_point`/`should_drop_multipoint`,
`POINT_DROP_FACTOR` and the deterministic-hash selection — level-generic.
**Tile-specific (strip):** all "tiny" polygon/line tests
(`should_drop_tiny_polygon`, `polygon_area_in_tile_coords`,
`polygon_pixel_area_world`, `linestring_pixel_area_world`, the WorldCoord
`*_world` block) scale area/length to a specific `TileCoord`+`extent` in
pixels; `DensityDropper` grids a single tile's 0..extent. For overviews these
become **per-level world-resolution thresholds** (drop below GSD-derived
visibility gate) and a **global per-level world grid** (the cell-winner grid),
not per-tile pixel budgets. `TinyPolygonAccumulator` is tile-encode-time and
shelves.

### clustering.rs — ADAPT (V5)
Tippecanoe point clustering by Hilbert proximity with incremental-centroid
averaging: `ClusterConfig{distance,max_zoom}`, `PointClusterer::cluster(points,
zoom)`, optional `AccumulatorConfig`. Works in world/Hilbert space (not tile
pixels); only `cluster_gap = ((2^(32−z)/256)*distance)^2` is phrased in
tile/zoom terms. Substitute a per-level world-distance threshold and it fits
per-level point aggregation for the coarse levels (Plan V5 quality ladder).

### coalesce.rs — ADAPT (V5, split)
Predictive geometry coalescing. `coalesce_geometries(target, source)` (merges
same-family geoms into Multi* types) and `GridSize` are **fully reusable** for
merging grid-cell losers. `SpatialGrid::assign_cell(geom)` bins by centroid
within a `TileBounds` — reusable if pointed at a world/level bbox. The
density-targeting layer (`calculate_coalesce_targets`,
`CoalesceTargets::should_coalesce(rg, zoom)`, `estimate_tile_density` via
`covering_tiles`) is tile-covering-count based and would need per-level
world-cell density reworking.

### external_sort.rs — ADAPT
Disk-backed external merge sort (wraps the `extsort` crate — the #147
performance revert restored this). The **sort engine** (`TileFeatureSorter`,
`SortedRecordIterator`, `Sortable` k-way merge, memory-bounded) is reusable.
`TileFeatureRecord` is tile-keyed (`tile_id`=PMTiles Hilbert ID, z/x/y). For
overviews, replace with a record keyed by `(level, grid_cell_id, hilbert)` to
group per-level cell-winner candidates; keep the merge machinery. Needed for
the V4 bounded-memory streaming pass.

### sutherland_hodgman.rs — SHELVE
O(n) polygon/ring clip against an axis-aligned rectangle (the four tile
edges). `clip_polygon_sh`/`clip_multipolygon_sh` (f64) and `*_world` (i64/i128
intermediates, u32 clamp, Y-southward). The algorithm is generic
"polygon∩rectangle" but the API is typed to tile bounds. No clipping in the
overview path → shelve for serve bridge; could be lifted if a future feature
needs bbox clipping.

### ioverlay_clip.rs — SHELVE
Robust polygon∩rectangle via `i_overlay` (Vatti sweep-line), the fallback for
cases SH mishandles (self-intersection, U-splits, holes crossing exterior).
`clip_polygon_ioverlay`/`clip_multipolygon_ioverlay`. Operates in f64; only
tile coupling is the clip box from `TileBounds`. Shelve (serve bridge).

### clip.rs — SHELVE
Per-tile clip dispatcher: `clip_geometry(geom, bounds, buffer)` (hybrid
SH-with-ioverlay-fallback), buffer-pixel conversions, and a WorldCoord-native
set (`clip_polygon_world`, `clip_point_world`). This is the tile
clipping-to-tile-bounds machinery — **explicitly excluded** from the overview
row model. Shelve (serve bridge E1).

### hierarchical_clip.rs — SHELVE
Cross-zoom clip reuse (`clip(clip(g,parent),child)==clip(g,child)`) plus
bbox→zoom-range visibility math (`zoom_range_for_bbox`, `min/max_zoom_for_bbox`
assuming 4096-px tiles) and `WorldClippedGeometry` (with `to_bytes`/`from_bytes`
for external sort). Deeply tile-centric; one `TODO` (linestring clipping in
WorldCoord space unimplemented). Shelve. **Note:** the `bbox→visibility zoom`
heuristic is conceptually reusable for deciding a feature's *min level* in the
overview assignment engine (P1) — lift the math, not the module.

### mvt.rs — SHELVE
Mapbox Vector Tile encoding (zigzag+delta+command, layer/feature builders) in
0–extent (4096) tile-local integer pixel space. `TileBuilder`, `LayerBuilder`,
`encode_geometry`, `geo_to_tile_coords`. No MVT in the overview path. **This is
the core of the future serve bridge (E1).** Shelve intact. (Has uncommitted
edits from the simplification branch per plan precondition.)

### pmtiles_writer.rs — SHELVE
PMTiles v3 archive writer (`PmtilesWriter`, `StreamingPmtilesWriter`, header /
directory / tile-id / dedup). Tile output only. Shelve (serve bridge).

### dedup.rs — SHELVE
XXH3 tile-blob deduplication with PMTiles run-length referencing
(`TileHasher`, `DeduplicationCache`). Tied to per-tile blob storage. Shelve.

### covering.rs — REUSE
GeoParquet 1.1 **bbox covering** support: `parse_covering_metadata`,
`find_bbox_column_indices`, `extract_row_group_bounds`, `RowGroupBounds`,
`CoveringSpec`, `tile_to_bounds`, `parse_bounds`. Used by `batch_processor` to
spatially prune row groups. **Directly reused by the overview reader (P4)** for
"level band ∩ bbox-pruned row groups" and by the writer's understanding of the
covering column. The one ignored test just needs a local multi-GB file.

### batch_processor.rs — ADAPT (the reader)
The GeoParquet **reader**. Streams by row group using the raw `parquet` crate
(`ParquetRecordBatchReaderBuilder`, `SerializedFileReader`, `ProjectionMask`,
`RowFilter`/`ArrowPredicateFn`) and decodes the geometry column via `geoarrow`
0.4 (`from_arrow_array` → `geo::Geometry`). Key entry points:
`process_geometries_by_row_group`, `process_features_parallel_filtered`
(geometry+`PropertyValue` properties), `read_features_from_row_group`,
`calculate_bbox`, `get_row_group_count`, `extract_field_metadata`,
`resolve_parquet_files`, `SpatialFilterConfig` (covering-based pruning).
**Reuse the read/streaming machinery**; the callback shapes are geared toward
"emit geometries for tiling" but are generic enough — the overview pipeline
needs the same "stream row groups → (geometry, properties)" and should reuse
`read_features_from_row_group` / `process_features_parallel_filtered`. No tile
math lives here beyond `TileBounds` for spatial filtering.

### pipeline.rs — SHELVE (the stuck subsystem)
236KB orchestrator of the whole tile pipeline: `TilerConfig` (all the tile
knobs), `generate_tiles_to_writer` / `generate_tiles_with_bounds` /
`generate_tiles_streaming_with_stats`, `encode_tile_from_raw`,
`encode_tile_with_adaptive_retry`, `ProcessingMode`/`auto_processing_mode`,
external-sort Phase 1/2/3 wiring, per-tile budgets, clustering/coalesce/drop
integration, progress callbacks. **This is where the tile pipeline is stuck**
(see next section). The overview pipeline should **not** import it; it belongs
to the serve bridge. New `overview/` code orchestrates read→assign→simplify→
write independently, reusing only the leaf utilities above (reader,
spatial_index, simplify-core, memory, covering).

### adaptive.rs — SHELVE
`AdaptiveTargets` per-zoom mingap/minextent thresholds for the
encode→measure→retry loop that services MVT tile-size overflow. A single-pass
overview thinning (pick retention fraction per level up front) doesn't need
reactive retry. Shelve; revisit only if per-level adaptive density feedback is
wanted later.

### golden.rs / integration_tests.rs — SHELVE (test-only)
`#[cfg(test)]` modules exercising the tile pipeline end-to-end (golden IoU vs
tippecanoe; full parquet→pmtiles integration). Kept for the serve bridge;
overview tests are new (P1–P5 add their own).

---

## Why the tile pipeline is stuck

`cargo check` is green and there is no single crashing bug — the pipeline
*works* on small inputs. It is **stuck on performance and memory at scale**,
plus a couple of correctness gaps, concentrated in three subsystems. The
overview path avoids all three by construction (no clip, no per-tile budgets,
no MVT re-encode, single geometry copy per level).

**1. External-sort + per-tile fan-out memory/perf (the big one).**
The geometry-centric algorithm writes one `(tile_id, feature)` record **per
tile each feature touches**, external-sorts by `tile_id`, then encodes. This
generates enormous intermediate volume for large/low-zoom inputs and thrashes.
The issue trail is long and circular:
- #124 *Epic: Streaming pipeline memory architecture* spawning #121
  (disk-backed GeometryStore), #122 (TileRef lightweight sorting), #123 (lazy
  clipping), #130 (dual-mode in-memory vs bucketed).
- #114 "Replace external sort with tile-based bucketing" → PR #144 → **#147
  "Revert to extsort crate to fix 7x performance regression from PR #144"**.
  A bucketing rewrite made it *7× slower* and was reverted; `external_sort.rs`
  is back on the `extsort` crate. This is a repeated-fix-then-revert thread —
  the strongest "stuck" signal.
- #32 (5.3GB peak RSS for 3.3GB input), #33 (Antarctica/world-spanning
  geometries touch ~every tile → pathological fan-out), #152 (memory tracking
  measured throughput not RSS — you couldn't even *see* the real memory).
- ARCHITECTURE.md's own "small row groups are pathologically slow" warning is
  a symptom of per-row-group + per-tile overhead.
→ **Risk carried by:** `external_sort.rs` record schema, `pipeline.rs` Phase
1 fan-out, `hierarchical_clip.rs`. The overview path emits **one row per
feature per level** (bounded, level-major) and never fans a feature across a
tile grid — this class of problem does not arise.

**2. Per-tile clipping cost & robustness.**
Clipping is the hot inner loop and a recurring robustness sink: #39 (evaluate
faster clipping), #117 (skip clip for contained features), #38 (avoid
redundant clip across zooms → the hierarchical_clip machinery), #94 (wire
wagyu for edge cases), #69 (self-intersecting polygon clipping robustness),
#75 (random artefacts during tiling), #83 (invalid MVT geometry — all-zero
deltas). The SH-with-ioverlay-fallback hybrid exists precisely because pure SH
mishandles self-intersections/U-shapes. → **Risk carried by:** `clip.rs`,
`sutherland_hodgman.rs`, `ioverlay_clip.rs`, `hierarchical_clip.rs`. The
overview row model does **no clipping** — features are stored whole per level.
This entire risk surface is excluded.

**3. Per-tile budgets / density dropping correctness.**
The adaptive tile-size machinery and density dropping have correctness holes:
**#145 "`--drop-densest-as-needed` has no effect (features not sorted by
Hilbert index within tiles)"** — a real behavioral bug where the flag silently
did nothing; plus #149 (encoding bottleneck 129 tiles/sec on adm2), #132/#134
(iterative threshold work), #71 ("investigate regression"). Density/size
dropping is entangled with tile encoding order and the adaptive retry loop
(`adaptive.rs` + `encode_tile_with_adaptive_retry`). → **Risk carried by:**
`feature_drop.rs` (per-tile pixel budgets), `adaptive.rs`, the retry loop in
`pipeline.rs`. The overview path thins **once per level** with a global
world-space grid cell-winner rule (deterministic, no per-tile budget, no
retry) — it deliberately does not import the adaptive/per-tile-budget model.
When lifting `feature_drop`/`gap_density` thinning, take the world-space,
Hilbert-index logic and **not** the tile-pixel-budget logic, and re-verify the
Hilbert-sort precondition that #145 exposed.

Net: the stuck-ness is **scale performance + clip robustness + tile-budget
correctness**, all localized to `pipeline.rs`, the clip family, `external_sort`,
and `adaptive.rs`. The overview design (whole features, level-major rows,
single simplification pass, no clip/MVT) routes around every one of them.

---

## Reader / writer infrastructure

**Reading (exists, solid).** GeoParquet reading lives in `batch_processor.rs`
(with `covering.rs` for spatial pruning and `quality.rs` for input
assessment). It is **streaming, row-group-oriented**, using:
- `parquet` **v55** directly: `ParquetRecordBatchReaderBuilder`,
  `SerializedFileReader`, `ProjectionMask` (project only the geometry column),
  `RowFilter`/`ArrowPredicateFn` (push-down bbox filter).
- `geoarrow` **0.4** / `geoarrow-array` **0.4**: `from_arrow_array` decodes
  the geometry column directly to `geo::Geometry` (no WKB round-trip).
- `arrow-array`/`arrow-schema` **55**.
Row-group iteration is explicit (`process_geometries_by_row_group`,
`read_features_from_row_group`); memory is O(row group). `covering.rs` parses
GeoParquet 1.1 bbox covering + per-row-group bbox stats for pruning. **The
overview reader (P4) reuses this wholesale.**

**Writing GeoParquet (does not exist yet — confirmed).** A repo-wide search
for `ArrowWriter` / parquet writer usage returns **nothing**. The only writer
in the codebase is `pmtiles_writer.rs` (PMTiles archives). So the overview
writer (P3) is **greenfield**, but the dependency stack fully supports it:

- **`geoparquet` v0.4** ships a writer: `writer::GeoParquetRecordBatchEncoder`
  (`try_new(schema, options)`, `encode_record_batch(batch)`,
  `into_keyvalue()` / `into_geoparquet_metadata()`) plus
  `GeoParquetWriterOptionsBuilder` supporting **covering generation**
  (`set_generate_covering(true)`, `set_column_covering_name`) and encoding
  choice (`GeoParquetWriterEncoding`). Pattern: encode each `RecordBatch`
  through the encoder, write with the raw `parquet::arrow::ArrowWriter`, then
  append the `geo` metadata via `ArrowWriter::append_key_value_metadata(
  encoder.into_keyvalue())` at close.
- **Custom footer KV metadata** (our `overview` spec key, alongside `geo`):
  supported — `ArrowWriter::append_key_value_metadata(KeyValue)` /
  `WriterProperties::key_value_metadata`. (The geoparquet crate itself uses
  exactly this to write `geo`.)
- **Row-group boundary control per level** (spec: each level ends on an RG
  boundary): `ArrowWriter::flush()` forces a row-group boundary on demand; plus
  `WriterProperties::max_row_group_size`. Write a level's batches, `flush()` at
  the level boundary, proceed to the next level.
- **ZSTD + no dictionary for geometry/bbox** (spec): `WriterProperties` sets
  compression and per-column dictionary/encoding.
- **`level` INT32 column**: just an ordinary Arrow column in the schema.
- **bbox covering struct column**: generated by the geoparquet encoder;
  per-row-group bbox stats come free from parquet native column statistics.

**Conclusion for P3:** feasible with the pinned stack (geoparquet 0.4 +
parquet 55 + geoarrow 0.4), no new dependencies. The main net-new work is
orchestration: buffer/emit level-major batches, drive covering + KV metadata,
and enforce RG-at-level-boundary via `flush()`. Caveat: the encoder's covering
is file-level metadata; the reader's row-group pruning relies on the bbox
column's native parquet statistics (already how `covering.rs` prunes today).

---

## Proposed module layout

New feature under a dedicated subtree; **no logic added to `pipeline.rs`**.

```
crates/core/src/overview/
├── mod.rs        # OverviewConfig, convert_to_overview(input,output,cfg),
│                 # Level/LevelParams (gsd, zoom?, mode), GSD↔zoom mapping.
├── level.rs      # Level model + GSD table (cogp convention:
│                 #   gsd(z)=40_075_016.69/1024/2^z), world-cell grid math,
│                 # bbox→min-level heuristic (lifted from hierarchical_clip).
├── assign.rs     # P1: grid cell-winner thinning per level. Pure fn
│                 #   (features, level_params) -> assignments (+min-level).
│                 # Reuses spatial_index (Hilbert tiebreak), optionally
│                 # gap_density/sampling for target fractions.
├── simplify.rs   # P2: world-space RDP wrapper (tolerance = gsd(level)),
│                 # degenerate/visibility guards. Reuses geo .simplify +
│                 # the ring/multi dispatch & validity guards lifted from
│                 # the crate's simplify.rs/validate.rs (f64 side).
├── writer.rs     # P3: level-banded GeoParquet writer.
│                 #   geoparquet::writer encoder + parquet ArrowWriter,
│                 #   flush() at level boundaries, `level` column, covering,
│                 #   `overview` footer KV, ZSTD.
└── reader.rs     # P4: parse footer -> select level by GSD/zoom ->
                  #   covering-prune row groups -> stream batches.
                  #   Reuses batch_processor read path + covering.rs.
```

Hook-in points:
- **`lib.rs`**: add `pub mod overview;`, re-export
  `overview::{OverviewConfig, convert_to_overview}` and reader/validate types.
  Leave the existing tile `Converter` and re-exports untouched (serve bridge).
- **CLI (`crates/cli/src/main.rs`)**: currently a single flat `Args` clap
  `Parser` (no subcommands, ~888 lines). Introduce a subcommand layer:
  `gpq-tiles overview <in> <out> [--min-zoom/--max-zoom | --gsd] [--mode
  duplicating|partitioning] [--sort-key] [--thinning/visibility factors]`
  (P5) and `gpq-tiles validate <file>` (P4), keeping the existing tile flags
  under a `tiles` subcommand (or default) for the serve bridge. This is the
  one place the flat-`Args` structure must be refactored.
- **Python (`crates/python`)**: E3, wraps `convert_to_overview`/reader — no
  change needed for Phase 1.

Shared leaf utilities the overview subtree depends on (all REUSE/ADAPT-core):
`batch_processor` (read), `covering` (prune), `spatial_index` (Hilbert),
`memory` (budget), `quality` (input check), `wkb` (V4 spill), `compression`
(ZSTD), plus `TileBounds`/`WorldBounds`/coordinate projection from
`tile`/`world_coord`, and the RDP/validity cores lifted from
`simplify`/`validate`.
