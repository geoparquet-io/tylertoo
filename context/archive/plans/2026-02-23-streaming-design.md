# Streaming GeoParquet Tiling Design

**Issue:** #21 - feat: streaming processing for files larger than available memory
**Date:** 2026-02-23
**Status:** Draft

## Summary

Enable gpq-tiles to process GeoParquet files larger than available memory by streaming row groups instead of loading all geometries into a Vec.

## Core Insight

GeoParquet row groups are the natural streaming unit. Each row group (typically 50-100MB) has geographic locality when Hilbert-sorted, touching a bounded set of tiles. We process one row group at a time, bucket features by tile, encode, and flush.

**Key simplification:** If input files are poorly formatted, users can optimize with geoparquet-io tools first. gpq-tiles doesn't need to solve external sorting—it detects and warns.

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│   GeoParquet    │     │   Row Group      │     │  Tile Buckets   │
│   File          │────▶│   Iterator       │────▶│  (in memory)    │
│                 │     │                  │     │                 │
│  [RG1][RG2][RG3]│     │  • decode geoms  │     │  tile_a: [f1,f3]│
│                 │     │  • clip to tiles │     │  tile_b: [f2,f4]│
│                 │     │  • bucket        │     │  tile_c: [f5]   │
└─────────────────┘     └──────────────────┘     └────────┬────────┘
                                                         │
                        ┌──────────────────┐             │
                        │   PMTiles        │◀────────────┘
                        │   Writer         │   flush buckets
                        │                  │   after each RG
                        │  (accumulates)   │
                        └──────────────────┘
```

### Memory Model

```
peak_memory ≈ row_group_decoded + active_tile_buffers
           ≈ 100MB            + 50MB (typical)
           ≈ 150MB per row group being processed
```

Memory is bounded by the largest row group, not file size.

### Processing Loop

```rust
pub fn generate_tiles_streaming(
    input_path: &Path,
    config: &TilerConfig,
) -> Result<impl Iterator<Item = Result<GeneratedTile>>> {

    // 1. Assess file quality, emit warnings
    let quality = assess_quality(input_path)?;
    emit_quality_warnings(&quality);

    // 2. Get row group iterator (doesn't load data yet)
    let rg_iterator = row_group_iterator(input_path)?;

    // 3. Process each row group independently
    for row_group in rg_iterator {
        let features = decode_row_group(&row_group)?;

        // Bucket features by tile (all zoom levels)
        let mut tile_buckets: HashMap<TileCoord, Vec<ClippedFeature>> = HashMap::new();

        for (geom, properties) in features {
            for z in config.min_zoom..=config.max_zoom {
                for tile_coord in tiles_intersecting(&geom, z) {
                    let clipped = clip_and_simplify(&geom, &tile_coord, config)?;
                    if !should_drop(&clipped, &tile_coord, config) {
                        tile_buckets.entry(tile_coord)
                            .or_default()
                            .push(ClippedFeature { geom: clipped, properties });
                    }
                }
            }
        }

        // Encode and yield tiles for this row group
        for (coord, features) in tile_buckets {
            let mvt = encode_tile(&coord, &features, config)?;
            yield GeneratedTile { coord, data: mvt };
        }
    }
}
```

## File Quality Detection

Detect suboptimal files and warn users to optimize with geoparquet-io.

### Detection Cascade (cheap to expensive)

| Check | Cost | Threshold | Warning |
|-------|------|-----------|---------|
| Missing `geo` metadata | O(1) | Any | "File missing GeoParquet metadata" |
| No row group bboxes | O(1) | Any | "Row groups lack bbox - cannot skip spatially" |
| Few row groups for size | O(1) | >500MB with <5 RGs | "Large file with few row groups" |
| Row group bbox overlap | O(n_rg) | >20% overlap | "Row groups overlap significantly" |
| Not Hilbert-sorted | O(1000) | File >1GB, sampled | "File not spatially sorted" |

### Warning Output

```
⚠ Input file not optimized for streaming:
  • Row groups lack bounding box metadata
  • File appears unsorted (sampled 1000 features)

  For best performance, optimize with geoparquet-io:
    gpq optimize input.parquet -o optimized.parquet --hilbert

  Proceeding anyway (may use more memory)...
```

Warnings are advisory. Use `--quiet` to suppress.

## Edge Cases

### Partially-Sorted Files

Files may be Hilbert-sorted within row groups but not across them. If row groups have non-overlapping bboxes (per GeoParquet best practices), process them in sequence without global sort.

### Row Group Bbox Overlap

When row groups overlap, features may be processed multiple times. Accept minor duplication—existing XXH3 tile deduplication handles identical tiles. Warn if overlap exceeds 20%.

### Features Spanning Multiple Tiles

Clip during streaming: as each feature streams in, clip to all intersecting tiles immediately. Buffer partial tiles until row group completes. No temp files needed.

### Cross-Zoom Dependencies

Feature dropping (tiny polygon, line, point, density) is per-feature and per-tile—no cross-zoom state required. Processing "all zooms per row group" produces identical output to current "all features per zoom" approach.

## Arrow/GeoArrow Reality Check

**The benefit of Arrow is columnar I/O and batch-scoped memory, not zero-copy geometry processing.**

Conversion to `geo::Geometry` is unavoidable because:
- `geo::BooleanOps` (clipping) requires owned `geo::Polygon`
- `geo::Simplify` requires owned `geo::LineString`
- No zero-copy clipping libraries exist for GeoArrow (yet)

What we DO get:
- Columnar decoding (only geometry column parsed)
- Row-group streaming (memory = O(row_group), not O(file))
- No double-copy (Arrow → geo directly, not Arrow → WKB → geo)

## Testing Strategy

### Golden Test: Streaming Equivalence

```rust
#[test]
fn streaming_matches_non_streaming_output() {
    let input = "tests/fixtures/realdata/open-buildings.parquet";
    let config = TilerConfig::new(0, 10);

    let non_streaming: Vec<_> = generate_tiles(input, &config).collect();
    let streaming: Vec<_> = generate_tiles_streaming(input, &config).collect();

    assert_eq!(non_streaming.len(), streaming.len());
    for (a, b) in non_streaming.iter().zip(streaming.iter()) {
        assert_eq!(a.coord, b.coord);
        assert_eq!(a.data, b.data);
    }
}
```

### Edge Case Tests

| Test | Purpose |
|------|---------|
| `single_row_group_file` | Streaming works with 1 row group |
| `many_small_row_groups` | Correct tile merging across row groups |
| `feature_spans_row_groups` | Bbox overlap handling |
| `unsorted_file_warning` | Warning emitted for non-Hilbert input |
| `missing_geo_metadata_warning` | Warning for files without geo extension |
| `memory_bounded` | 1GB+ file stays under memory threshold (ignored by default) |

### Required Test Fixtures

Create manually during implementation (checked into repo):

| Fixture | Purpose | How to Create |
|---------|---------|---------------|
| `multi-rowgroup-small.parquet` | Small file with many row groups | Re-export `open-buildings.parquet` with `row_group_size=50` rows using DuckDB or PyArrow |
| `unsorted.parquet` | Shuffled row order | Load `open-buildings.parquet`, shuffle rows, write back |
| `no-geo-metadata.parquet` | Stripped geo extension | Copy parquet without geo key-value metadata |

**One-time creation script** (run once, commit fixtures):

```python
# scripts/create_streaming_fixtures.py
import pyarrow.parquet as pq
import pyarrow as pa
import random

# 1. Multi-row-group: force tiny row groups
table = pq.read_table("tests/fixtures/realdata/open-buildings.parquet")
pq.write_table(table, "tests/fixtures/streaming/multi-rowgroup-small.parquet",
               row_group_size=50)

# 2. Unsorted: shuffle row order
indices = list(range(table.num_rows))
random.seed(42)  # deterministic
random.shuffle(indices)
shuffled = table.take(indices)
pq.write_table(shuffled, "tests/fixtures/streaming/unsorted.parquet")

# 3. No geo metadata: strip geo extension
metadata = table.schema.metadata or {}
stripped_metadata = {k: v for k, v in metadata.items() if b"geo" not in k.lower()}
new_schema = table.schema.with_metadata(stripped_metadata)
stripped = table.cast(new_schema)
pq.write_table(stripped, "tests/fixtures/streaming/no-geo-metadata.parquet")
```

Run once with `uv run python scripts/create_streaming_fixtures.py`, commit the output files.

### Large File Test (download on demand)

For memory-bounded testing, use FieldMaps global admin boundaries (~2.7GB):

```
URL: https://data.fieldmaps.io/edge-matched/humanitarian/intl/adm3_polygons.parquet
Size: ~2.7GB
```

Test is `#[ignore]` by default. Run manually:

```bash
# Download once (cached in tests/fixtures/large/)
curl -o tests/fixtures/large/adm3_polygons.parquet \
  https://data.fieldmaps.io/edge-matched/humanitarian/intl/adm3_polygons.parquet

# Run the ignored test
cargo test --package gpq-tiles-core test_large_file_memory_bounded -- --ignored
```

**Do not commit this file** - it's 2.7GB. Add `tests/fixtures/large/` to `.gitignore`.

## Documentation Updates

### CLAUDE.md

1. Revise "Arrow/GeoArrow is the Data Layer" to explain the actual tradeoff
2. Add note about uv for Python tooling
3. Add streaming architecture reference

### ARCHITECTURE.md

1. Add "Streaming Processing" section
2. Document file quality detection heuristics
3. Note geoparquet-io integration for optimization warnings

### README.md

1. Link to geoparquet-io for file optimization
2. Document `--quiet` flag
3. Add "Best Practices" section recommending Hilbert-sorted input

## Implementation Phases

1. **File quality detection** - `assess_quality()` function with warnings
2. **Row group iterator** - Parquet reader that yields row groups
3. **Streaming pipeline** - `generate_tiles_streaming()` with tile bucketing
4. **Tile merging** - PMTiles writer accumulates across row groups
5. **Tests** - Golden equivalence + edge cases
6. **Documentation** - CLAUDE.md, ARCHITECTURE.md, README.md updates
7. **Benchmarks** - Memory profiling for 100MB+ files

## References

- [GeoParquet Best Practices](https://geoparquet.io/concepts/best-practices/)
- [Planetiler Architecture](https://github.com/onthegomap/planetiler/blob/main/ARCHITECTURE.md)
- [Tippecanoe](https://github.com/felt/tippecanoe) - quadkey indexing, radix sort
- Issue #21 - Original feature request
