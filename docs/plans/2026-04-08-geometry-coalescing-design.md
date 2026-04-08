# Geometry Coalescing Design

**Issue:** #26  
**Date:** 2026-04-08  
**Status:** Draft

## Context

When tiles contain too many features, we need to reduce complexity without losing data. Unlike dropping (data loss) or clustering (changes geometry), **coalescing merges geometries into Multi* types** while preserving all coordinates.

Tippecanoe coalesces reactively when tiles exceed 500KB. We're taking a **GeoParquet-native predictive approach** that leverages row group metadata to estimate density upfront.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **Trigger** | Predictive (GeoParquet-native) | Avoid retry loops; single-pass processing |
| **Lookup** | Spatial grid within tile | O(1) with spatial grouping |
| **Parallelism** | Row-group pre-coalesce | Reduce features early; parallel per row group |
| **Threshold** | Geographic density + 90th percentile | Data-driven; no magic numbers |
| **Grid size** | Adaptive by density | Optimal quality/performance tradeoff |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Phase 1: Metadata Scan (fast, no decompression)                │
│                                                                 │
│  For each row group:                                            │
│    density[i] = num_rows / bbox.area()                          │
│                                                                 │
│  threshold = percentile(densities, 0.90)                        │
└─────────────────────────────────────────────────────────────────┘
                               ↓
┌─────────────────────────────────────────────────────────────────┐
│  Phase 2: Row-Group Pre-Coalesce (parallel per row group)       │
│                                                                 │
│  For each row group where density > threshold:                  │
│    1. Read features                                             │
│    2. Bucket by target tile(s)                                  │
│    3. For each tile bucket:                                     │
│       - Create spatial grid (adaptive: 4×4 to 8×8)              │
│       - Assign features to grid cells by centroid               │
│       - Coalesce features within each cell                      │
│    4. Emit reduced feature set                                  │
└─────────────────────────────────────────────────────────────────┘
                               ↓
┌─────────────────────────────────────────────────────────────────┐
│  Phase 3: Tile Assembly (existing pipeline)                     │
│                                                                 │
│  Merge pre-coalesced chunks from all row groups                 │
│  Encode to MVT                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Threshold Calculation

```rust
/// Calculate adaptive coalesce threshold from row group metadata
fn calculate_coalesce_threshold(file: &ParquetFile) -> Option<f64> {
    let densities: Vec<f64> = file.row_groups()
        .filter_map(|rg| {
            let bbox = rg.geo_metadata()?.covering_bbox()?;
            let area = bbox.geodesic_area_km2();
            if area < f64::EPSILON { return None; }
            Some(rg.num_rows() as f64 / area)
        })
        .collect();
    
    if densities.is_empty() { return None; }
    
    Some(percentile(&densities, 0.90))
}
```

**Why 90th percentile:**
- Conservative: only coalesce the top 10% densest row groups
- Matches tippecanoe's "as-needed" philosophy
- Sparse regions remain untouched

## Spatial Grid

```rust
struct SpatialGrid {
    /// Grid cells, indexed by [row][col]
    cells: Vec<Vec<GridCell>>,
    /// Grid dimensions (adaptive: 4-8 based on density)
    size: usize,
    /// Tile bounds for cell assignment
    bounds: TileBounds,
}

struct GridCell {
    /// One accumulator per geometry type
    points: Option<AccumulatedFeature>,
    lines: Option<AccumulatedFeature>,
    polygons: Option<AccumulatedFeature>,
}

impl SpatialGrid {
    fn new(density: f64, bounds: TileBounds) -> Self {
        // Adaptive grid size based on density
        let size = if density > HIGH_DENSITY_THRESHOLD { 8 } else { 4 };
        Self { cells: vec![vec![GridCell::default(); size]; size], size, bounds }
    }
    
    fn assign_cell(&self, centroid: Coord) -> (usize, usize) {
        let x = ((centroid.x - self.bounds.min_x) / self.bounds.width() * self.size as f64) as usize;
        let y = ((centroid.y - self.bounds.min_y) / self.bounds.height() * self.size as f64) as usize;
        (x.min(self.size - 1), y.min(self.size - 1))
    }
}
```

**Adaptive sizing:**
- Low density (< threshold): 4×4 grid (16 cells max)
- High density (> 2× threshold): 8×8 grid (64 cells max)

## Geometry Coalescing

```rust
/// Coalesce source geometry into target, converting to Multi* as needed
pub fn coalesce_geometries(target: &mut Geometry, source: &Geometry) {
    match (target, source) {
        // Point + Point → MultiPoint
        (Geometry::Point(p1), Geometry::Point(p2)) => {
            *target = Geometry::MultiPoint(MultiPoint::new(vec![*p1, *p2]));
        }
        // MultiPoint + Point → MultiPoint (extended)
        (Geometry::MultiPoint(mp), Geometry::Point(p)) => {
            mp.0.push(*p);
        }
        // MultiPoint + MultiPoint → MultiPoint (merged)
        (Geometry::MultiPoint(mp1), Geometry::MultiPoint(mp2)) => {
            mp1.0.extend(mp2.0.iter().cloned());
        }
        // LineString + LineString → MultiLineString
        (Geometry::LineString(l1), Geometry::LineString(l2)) => {
            *target = Geometry::MultiLineString(MultiLineString::new(vec![
                l1.clone(), l2.clone()
            ]));
        }
        // ... similar for MultiLineString, Polygon, MultiPolygon
        _ => {
            // Mismatched types: don't coalesce (shouldn't happen with grid cell separation)
        }
    }
}
```

**Attribute handling:** Reuse existing `accumulator.rs` infrastructure.

## Integration with Existing Code

### Files to Modify

| File | Changes |
|------|---------|
| `crates/core/src/coalesce.rs` | **NEW** - Core coalescing logic |
| `crates/core/src/pipeline.rs` | Add `CoalesceConfig` to `TilerConfig` |
| `crates/core/src/batch_processor.rs` | Add row-group pre-coalesce pass |
| `crates/core/src/covering.rs` | Expose density calculation helper |
| `crates/cli/src/main.rs` | Add `--coalesce-densest-as-needed` flag |

### Reuse Existing Infrastructure

- `accumulator.rs` - Attribute aggregation (Sum, Mean, Max, etc.)
- `covering.rs` - Row group bbox extraction
- `spatial_index.rs` - Hilbert encoding for tile bucketing
- `gap_density.rs` - Percentile calculation utilities

## API

```rust
// TilerConfig extension
impl TilerConfig {
    /// Enable geometry coalescing for dense features
    pub fn with_coalesce_densest(mut self) -> Self {
        self.coalesce_config = Some(CoalesceConfig::default());
        self
    }
    
    /// Set custom density percentile threshold (default: 90)
    pub fn with_coalesce_percentile(mut self, percentile: u8) -> Self {
        self.coalesce_config.get_or_insert_default().percentile = percentile;
        self
    }
}

#[derive(Default)]
pub struct CoalesceConfig {
    /// Percentile threshold for density-based coalescing (default: 90)
    pub percentile: u8,
    /// Optional explicit density threshold (overrides percentile)
    pub density_threshold: Option<f64>,
    /// High-density grid size multiplier
    pub high_density_grid: usize,
}
```

## CLI

```bash
# Enable coalescing (uses adaptive threshold)
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed

# Custom percentile
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-percentile 75

# Combine with attribute accumulation
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed -aC sum:population
```

## Expected Performance

### Size Savings
- **30-60% tile size reduction** on dense datasets
- Top 10% densest row groups (at 90th percentile) typically contain 50%+ of features
- Multi* encoding is more efficient than repeated single geometries

### Speed Impact
- **Metadata scan:** ~100-500ms for large files (one-time)
- **Per row-group coalescing:** O(n) with O(1) grid lookup
- **Net effect:** Faster downstream (fewer features to encode)

### vs Tippecanoe
| Aspect | Tippecanoe | gpq-tiles |
|--------|------------|-----------|
| Trigger | Reactive (retry on size) | Predictive (metadata scan) |
| Passes | 1-3 per tile | 1 per tile |
| Lookup | O(n) linear search | O(1) spatial grid |
| Parallelism | Per-tile | Per-row-group + per-tile |

## Test Plan

1. **Unit tests** for `coalesce_geometries()` - all type combinations
2. **Integration tests** with known-dense GeoParquet files
3. **Comparison tests** vs tippecanoe output on same input
4. **Performance benchmarks** on Overture buildings dataset

## Tasks

- [ ] Create `crates/core/src/coalesce.rs` with core types
- [ ] Implement `calculate_coalesce_threshold()` using covering.rs
- [ ] Implement `SpatialGrid` with adaptive sizing
- [ ] Implement `coalesce_geometries()` for all type pairs
- [ ] Add `CoalesceConfig` to `TilerConfig`
- [ ] Integrate with `batch_processor.rs` row-group processing
- [ ] Add CLI flags
- [ ] Write comprehensive tests
- [ ] Benchmark against tippecanoe
