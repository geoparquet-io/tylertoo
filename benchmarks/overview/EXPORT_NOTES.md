# `export-pmtiles` — E0 export notes

Batch PMTiles export **from** an overview GeoParquet file
(`gpq-tiles export-pmtiles <overviews.parquet> <out.pmtiles>`). This is the
replacement for the shelved tile pipeline, not a revival of it: the overview
file already holds thinned / simplified / ranked / Hilbert-ordered features per
level, so export is mechanical and single-pass — no global external sort, no
per-tile budget retry loop, no adaptive re-encode.

## Pipeline (per zoom)

1. Resolve each overview **level** to a Web Mercator **zoom** (explicit
   `levels[].zoom`, else `round(zoom_for_gsd(gsd))`).
2. Stream the level band via `OverviewReader::read_level(level, bbox=None)`. The
   reader already applies the mode: `duplicating` = the level's own row-group
   band; `partitioning` = the prefix `0..=level`. Export treats both identically.
3. Assign each feature to the tile(s) it intersects (`tiles_for_bbox`), clip to
   tile bounds + pixel buffer (shelved `clip_geometry`, SH + ioverlay fallback),
   MVT-encode (shelved `mvt.rs`), write via `StreamingPmtilesWriter`.

PMTiles header min/max zoom are set from the level zoom range; zooms outside the
range are simply not written (renderers overzoom from maxzoom).

**Memory ceiling:** per-zoom `O(F_level + border-duplicated copies)`, built into
an in-memory `BTreeMap<tile, Vec<feature>>` that is drained into the writer and
dropped before the next zoom. No cross-zoom accumulation.

## End-to-end validation

Input: `corpus/data/overviews/lines-portland-medium.dup.parquet`
(duplicating, 13 levels z2–z14, 295,881 canonical line features, 106 MB).

| metric | value |
|---|---|
| output | 46.0 MB PMTiles (gzip) |
| zooms | z2–z14 (13) |
| tiles | 1,325 |
| wall time | 5.05 s (release) |
| peak RSS | 244 MB |
| oversized tiles | 0 (no `--tile-size-limit`) |

Verified with `pmtiles show` (header: MVT, z2–z14, Portland bounds, 1325 tiles,
clustered) and `tippecanoe-decode` on sampled tiles:

- `z6/10/22` → 853 features (== the whole z6 level, single tile).
- `z8/40/91` → 13,453 features.
- `z11/326/732` → 34,155 features.

### Per-zoom totals vs the overview report (border duplication)

The overview *format* stores each feature once per level. The PMTiles *export*
necessarily reintroduces classic MVT semantics: a feature spanning a tile seam
is clipped into — and appears in — every tile it touches. Export per-zoom
totals therefore meet-or-exceed the overview level counts, the excess being
border duplication. It is 0% while a level fits in one tile and grows with the
tile count.

| zoom | overview feats | tiles | export feats | border dup |
|---|---|---|---|---|
| 2 | 4 | 1 | 4 | +0 (0.0%) |
| 3 | 14 | 1 | 14 | +0 (0.0%) |
| 4 | 45 | 1 | 45 | +0 (0.0%) |
| 5 | 191 | 1 | 191 | +0 (0.0%) |
| 6 | 853 | 1 | 853 | +0 (0.0%) |
| 7 | 3,636 | 1 | 3,636 | +0 (0.0%) |
| 8 | 14,032 | 2 | 14,112 | +80 (0.6%) |
| 9 | 46,600 | 4 | 46,866 | +266 (0.6%) |
| 10 | 120,294 | 9 | 121,181 | +887 (0.7%) |
| 11 | 193,691 | 25 | 195,683 | +1,992 (1.0%) |
| 12 | 242,545 | 76 | 247,419 | +4,874 (2.0%) |
| 13 | 275,143 | 264 | 285,635 | +10,492 (3.8%) |
| 14 | 295,881 | 939 | 317,077 | +21,196 (7.2%) |

## Safety valve

There is exactly one, and it is optional. With `--tile-size-limit BYTES`, a tile
whose encoded MVT exceeds the limit gets a **single, non-iterative** drop pass:
its lowest-priority features are dropped and the tile is re-encoded once. There
is no retry loop. Priority ranks by geometry size (coordinate count) descending —
the assignment sort key is not recoverable per row (features carry an arbitrary
property schema; the sort key is not persisted), matching the task's "else size"
branch. Oversized-tile counts are recorded per zoom and in the report. On the
Portland run with no limit, 0 tiles were oversized.

## Deviations from the shelved modules

- **`mvt.rs` needs geographic (lon/lat) coordinates.** `geo_to_tile_coords`
  projects from `TileBounds` (degrees). The exporter therefore reprojects an
  EPSG:3857 overview to EPSG:4326 on read (inverse Web Mercator) before tiling;
  4326 (the GeoParquet default, incl. a null CRS) passes through. No other
  friction — `LayerBuilder::add_feature` + `TileBuilder` + `encode_to_vec` and
  `StreamingPmtilesWriter` were reused verbatim.
- **Tile ordering:** `StreamingPmtilesWriter` re-sorts entries by PMTiles tile
  id internally, so the `BTreeMap<(x,y)>` grouping order is not load-bearing; it
  is used for determinism and a bounded per-zoom working set, not to satisfy a
  writer ordering contract.
