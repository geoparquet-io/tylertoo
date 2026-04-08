# Geometry Coalescing Design

**Issue:** #26  
**Date:** 2026-04-08  
**Status:** Draft  
**Updated:** 2026-04-08 (post-research)

## Context

When tiles contain too many features, we need to reduce complexity without losing data. Unlike dropping (data loss) or clustering (changes geometry), **coalescing merges geometries into Multi* types** while preserving all coordinates.

We're taking a **GeoParquet-native predictive approach** that leverages row group metadata to estimate density upfront, enabling selective coalescing only where needed.

## Why GeoParquet-Native (vs Tippecanoe)

**Research finding:** Tippecanoe's coalescing is reactive and uses linear scan (see `tile.cpp:1512-1567`):
1. Encode entire tile to MVT
2. Measure compressed size (default limit: 500KB)
3. If oversized: adjust mingap/minextent thresholds, retry
4. Repeat 1-10+ times per dense tile

| Aspect | Tippecanoe | gpq-tiles (this design) |
|--------|------------|-------------------------|
| **Trigger** | Post-encode size check | Pre-compute from metadata |
| **Lookup** | Linear scan O(n) | Spatial grid O(1) |
| **Grouping** | "Most recent same-type feature" | Hilbert-clustered spatial cells |
| **Passes per tile** | 1-10+ (retry loop) | 1 (single pass) |
| **Attribute ops** | sum/mean/max/min/concat | Same (reuse accumulator.rs) |

Traditional tile generators treat input as opaque features. They must read all geometries before knowing which tiles will be dense.

GeoParquet with gpio optimization provides metadata that enables **predictive** coalescing:

| Metadata | Available Without Decompression | What It Tells Us |
|----------|--------------------------------|------------------|
| Row group bbox | Parquet footer | Spatial extent of features |
| Row group row count | Parquet footer | Feature count |
| Hilbert sort order | Implicit from gpio | Spatial locality guaranteed |

**Key insight:** With Hilbert-sorted files, a row group's bbox + row count lets us estimate features-per-tile at any zoom level without reading geometry data.

**Precondition:** Input files MUST be optimized with `gpio sort hilbert --add-bbox`. This ensures:
- Row groups have bbox covering metadata
- Features are Hilbert-sorted (spatial locality)
- Row groups are spatially coherent

This enables:
1. **Upfront identification** of dense tiles before processing
2. **Selective coalescing** — only process tiles that need it
3. **Bounded memory** — stream row groups, never load entire file
4. **Zoom-aware thresholds** — coalesce aggressively at low zoom, lightly at high zoom

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **Trigger** | Predictive (zoom-aware tile density) | Leverage GeoParquet metadata; no retry loops |
| **Metric** | Features per tile (not per km²) | Directly measures what causes large tiles |
| **Lookup** | Spatial grid within tile | O(1) with spatial grouping |
| **Parallelism** | Per-row-group processing | Reduce features early; parallel per row group |
| **Threshold** | 90th percentile of tile densities | Data-driven; sparse regions untouched |
| **Grid size** | Adaptive by density | Optimal quality/performance tradeoff |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Phase 1: Metadata Scan (fast, no decompression)                │
│                                                                 │
│  For each zoom level z in [min_zoom, max_zoom]:                 │
│    For each row group rg:                                       │
│      tile_count = tiles_covering_bbox(rg.bbox, z)               │
│      density[rg, z] = rg.num_rows / tile_count                  │
│    threshold[z] = percentile(densities_at_z, 0.90)              │
│    dense_row_groups[z] = { rg | density[rg,z] > threshold[z] }  │
└─────────────────────────────────────────────────────────────────┘
                               ↓
┌─────────────────────────────────────────────────────────────────┐
│  Phase 2: Tile Generation (existing pipeline)                   │
│                                                                 │
│  For each row group:                                            │
│    Read features                                                │
│    Clip to tiles (hierarchical)                                 │
│    For tiles at zoom z where row_group ∈ dense_row_groups[z]:   │
│      → Route through coalescing path                            │
│    For other tiles:                                             │
│      → Standard encoding path                                   │
└─────────────────────────────────────────────────────────────────┘
                               ↓
┌─────────────────────────────────────────────────────────────────┐
│  Phase 3: Coalescing (only for marked tiles)                    │
│                                                                 │
│  Build SpatialGrid for tile                                     │
│  Assign features to cells by centroid                           │
│  Coalesce within cells (by geometry type)                       │
│  Merge attributes via AccumulatorConfig                         │
│  Encode to MVT                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Threshold Calculation

```rust
/// Estimate features-per-tile for a row group at a given zoom level
fn estimate_tile_density(bounds: &TileBounds, num_rows: usize, zoom: u8) -> f64 {
    let tile_count = covering_tiles(bounds, zoom).count().max(1);
    num_rows as f64 / tile_count as f64
}

/// Identify row groups that will produce dense tiles at each zoom level
fn calculate_coalesce_targets(
    file: &ParquetFile,
    min_zoom: u8,
    max_zoom: u8,
) -> CoalesceTargets {
    let row_group_bounds: Vec<Option<RowGroupBounds>> = extract_row_group_bounds(file);
    
    let mut targets = CoalesceTargets::new();
    
    for zoom in min_zoom..=max_zoom {
        let densities: Vec<(usize, f64)> = row_group_bounds.iter()
            .enumerate()
            .filter_map(|(idx, bounds)| {
                let bounds = bounds.as_ref()?;
                let density = estimate_tile_density(&bounds.bbox, bounds.num_rows, zoom);
                Some((idx, density))
            })
            .collect();
        
        // Require minimum row groups for stable percentile
        if densities.len() < 5 {
            continue;
        }
        
        let threshold = percentile(&densities.iter().map(|(_, d)| *d).collect::<Vec<_>>(), 0.90);
        
        for (rg_idx, density) in &densities {
            if *density > threshold {
                targets.mark_dense(*rg_idx, zoom, *density);
            }
        }
    }
    
    targets
}

/// Tracks which row groups need coalescing at which zoom levels
pub struct CoalesceTargets {
    /// Map of row_group_index -> set of zoom levels where it's dense
    dense_at: HashMap<usize, HashSet<u8>>,
    /// Density values for logging/debugging
    densities: HashMap<(usize, u8), f64>,
}

impl CoalesceTargets {
    pub fn should_coalesce(&self, row_group_idx: usize, zoom: u8) -> bool {
        self.dense_at
            .get(&row_group_idx)
            .map(|zooms| zooms.contains(&zoom))
            .unwrap_or(false)
    }
}
```

**Why zoom-aware:**
- A row group covering Manhattan produces ~100 features/tile at z14 (fine)
- Same row group produces ~10,000 features/tile at z4 (needs coalescing)
- Per-zoom thresholds coalesce only where needed

**Why 90th percentile:**
- Conservative: only coalesce the top 10% densest row groups per zoom
- Data-driven: threshold adapts to dataset characteristics
- Sparse regions remain untouched

## Spatial Grid

```rust
pub struct SpatialGrid {
    /// Grid cells, indexed by [row][col]
    cells: Vec<Vec<GridCell>>,
    /// Grid dimensions
    size: usize,
    /// Tile bounds for cell assignment
    bounds: TileBounds,
}

pub struct GridCell {
    /// One accumulator per geometry type
    points: Option<AccumulatedFeature>,
    lines: Option<AccumulatedFeature>,
    polygons: Option<AccumulatedFeature>,
}

impl SpatialGrid {
    pub fn new(estimated_features: f64, bounds: TileBounds, config: &GridSize) -> Self {
        let size = match config {
            GridSize::Fixed(n) => *n,
            GridSize::Adaptive { low, high, threshold } => {
                if estimated_features > *threshold { *high } else { *low }
            }
        };
        Self { 
            cells: vec![vec![GridCell::default(); size]; size], 
            size, 
            bounds 
        }
    }
    
    /// Assign geometry to cell. Returns None if centroid cannot be computed.
    /// 
    /// Edge cases handled:
    /// - Empty geometries → None (caller should filter)
    /// - Zero-area polygons → falls back to bounding_rect().center()
    /// - Degenerate linestrings → falls back to first coordinate
    pub fn assign_cell(&self, geom: &Geometry) -> Option<(usize, usize)> {
        // Primary: use centroid
        // Fallback: bounding rect center (handles degenerate cases)
        let center = geom.centroid()
            .or_else(|| geom.bounding_rect().map(|r| r.center()))?;
        
        let x = ((center.x() - self.bounds.min_x) / self.bounds.width() * self.size as f64) as usize;
        let y = ((center.y() - self.bounds.min_y) / self.bounds.height() * self.size as f64) as usize;
        Some((x.min(self.size - 1), y.min(self.size - 1)))
    }
}

pub enum GridSize {
    Fixed(usize),
    Adaptive { low: usize, high: usize, threshold: f64 },
}

impl Default for GridSize {
    fn default() -> Self {
        GridSize::Adaptive { low: 4, high: 8, threshold: 500.0 }
    }
}
```

**Adaptive sizing:**
- Low density (< 500 features/tile): 4×4 grid (16 cells)
- High density (≥ 500 features/tile): 8×8 grid (64 cells)

## Geometry Coalescing

```rust
/// Result of attempting to coalesce two geometries
pub enum CoalesceResult {
    /// Geometries were merged into target
    Merged,
    /// Type mismatch - source should be kept as separate feature
    TypeMismatch(Geometry),
}

/// Coalesce source geometry into target, converting to Multi* as needed
pub fn coalesce_geometries(target: &mut Geometry, source: Geometry) -> CoalesceResult {
    match (target, source) {
        // === Point variants ===
        (Geometry::Point(p1), Geometry::Point(p2)) => {
            *target = Geometry::MultiPoint(MultiPoint::new(vec![*p1, p2]));
            CoalesceResult::Merged
        }
        (Geometry::MultiPoint(mp), Geometry::Point(p)) => {
            mp.0.push(p);
            CoalesceResult::Merged
        }
        (Geometry::MultiPoint(mp1), Geometry::MultiPoint(mp2)) => {
            mp1.0.extend(mp2.0);
            CoalesceResult::Merged
        }
        
        // === LineString variants ===
        (Geometry::LineString(l1), Geometry::LineString(l2)) => {
            *target = Geometry::MultiLineString(MultiLineString::new(vec![
                l1.clone(), l2
            ]));
            CoalesceResult::Merged
        }
        (Geometry::MultiLineString(ml), Geometry::LineString(l)) => {
            ml.0.push(l);
            CoalesceResult::Merged
        }
        (Geometry::MultiLineString(ml1), Geometry::MultiLineString(ml2)) => {
            ml1.0.extend(ml2.0);
            CoalesceResult::Merged
        }
        
        // === Polygon variants ===
        (Geometry::Polygon(p1), Geometry::Polygon(p2)) => {
            *target = Geometry::MultiPolygon(MultiPolygon::new(vec![
                p1.clone(), p2
            ]));
            CoalesceResult::Merged
        }
        (Geometry::MultiPolygon(mp), Geometry::Polygon(p)) => {
            mp.0.push(p);
            CoalesceResult::Merged
        }
        (Geometry::MultiPolygon(mp1), Geometry::MultiPolygon(mp2)) => {
            mp1.0.extend(mp2.0);
            CoalesceResult::Merged
        }
        
        // === Convertible types: normalize then coalesce ===
        (target, Geometry::Line(l)) => {
            coalesce_geometries(target, Geometry::LineString(l.into()))
        }
        (target, Geometry::Rect(r)) => {
            coalesce_geometries(target, Geometry::Polygon(r.to_polygon()))
        }
        (target, Geometry::Triangle(t)) => {
            coalesce_geometries(target, Geometry::Polygon(t.to_polygon()))
        }
        
        // === GeometryCollection: flatten and coalesce each component ===
        (target, Geometry::GeometryCollection(gc)) => {
            let mut unmerged = Vec::new();
            for geom in gc.0 {
                if let CoalesceResult::TypeMismatch(g) = coalesce_geometries(target, geom) {
                    unmerged.push(g);
                }
            }
            if unmerged.is_empty() {
                CoalesceResult::Merged
            } else {
                CoalesceResult::TypeMismatch(Geometry::GeometryCollection(
                    GeometryCollection::new_from(unmerged)
                ))
            }
        }
        
        // === Type mismatch: return source for separate handling ===
        (_, source) => CoalesceResult::TypeMismatch(source),
    }
}
```

**Explicit type handling:**
- `Line` → convert to `LineString`, then coalesce
- `Rect` → convert to `Polygon`, then coalesce  
- `Triangle` → convert to `Polygon`, then coalesce
- `GeometryCollection` → flatten, coalesce components by type
- Type mismatch → return source as separate feature (no silent drops)

## Attribute Handling

Coalescing uses existing `accumulator.rs` infrastructure. **Important:** attributes without configured accumulators are DROPPED (matching tippecanoe behavior).

```rust
/// Attribute handling modes during coalescing
pub enum AttributeMode {
    /// Drop attributes without configured accumulators (default, tippecanoe-compatible)
    Drop,
    /// Keep first feature's value for unconfigured attributes
    KeepFirst,
    /// Error if any attribute lacks an accumulator config
    Strict,
}
```

**CLI usage:**
```bash
# Attributes without accumulators are dropped (default)
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed

# Keep first feature's attributes for unconfigured
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-attrs=keep-first

# Require all attributes to have accumulators
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-attrs=strict -aC sum:population -aC concat:name
```

## Integration with Existing Code

### Files to Modify

| File | Changes |
|------|---------|
| `crates/core/src/coalesce.rs` | **NEW** - `CoalesceTargets`, `SpatialGrid`, `coalesce_geometries()` |
| `crates/core/src/pipeline.rs` | Add `CoalesceConfig` to `TilerConfig`; route dense tiles through coalescing |
| `crates/core/src/gap_density.rs` | Add `percentile()` function |
| `crates/cli/src/main.rs` | Add `--coalesce-densest-as-needed` and related flags |

### Reuse Existing Infrastructure

| Module | Function | Status | Usage |
|--------|----------|--------|-------|
| `accumulator.rs` | `AccumulatorConfig::accumulate()` | ✅ EXISTS | Attribute merging |
| `covering.rs` | `extract_row_group_bounds()` | ✅ EXISTS | Row group bbox extraction |
| `covering.rs` | `tile_to_bounds()` | ✅ EXISTS | Tile coord → WGS84 bounds |
| `gap_density.rs` | `choose_mingap()` | ✅ EXISTS | Percentile-like calculation |
| `spatial_index.rs` | `encode_hilbert()` | ✅ EXISTS | Hilbert encoding |

### Functions to Implement

| Function | Location | Complexity | Notes |
|----------|----------|------------|-------|
| `covering_tiles(bounds, zoom) -> impl Iterator<TileCoord>` | `covering.rs` | ~20 lines | **NEW** - iterate tiles covering bbox |
| `percentile(values: &[f64], p: f64) -> f64` | `gap_density.rs` | ~10 lines | Simple nth-element |
| `estimate_tile_density()` | `coalesce.rs` | ~5 lines | Uses `covering_tiles().count()` |
| `CoalesceTargets` struct + methods | `coalesce.rs` | ~50 lines | Zoom-aware tracking |
| `SpatialGrid` struct + methods | `coalesce.rs` | ~80 lines | With centroid fallback |
| `coalesce_geometries()` | `coalesce.rs` | ~100 lines | All type pairs + edge cases |

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
    
    /// Set minimum features/tile to trigger coalescing
    pub fn with_coalesce_min_density(mut self, min_density: f64) -> Self {
        self.coalesce_config.get_or_insert_default().min_density_trigger = min_density;
        self
    }
}

#[derive(Default)]
pub struct CoalesceConfig {
    /// Percentile threshold for density-based coalescing (default: 90)
    pub percentile: u8,
    
    /// Optional per-zoom density thresholds (features/tile)
    /// Overrides percentile calculation when set
    pub density_thresholds: Option<HashMap<u8, f64>>,
    
    /// Minimum features/tile to trigger coalescing (default: 100)
    /// Even if percentile selects a row group, skip if below this
    pub min_density_trigger: f64,
    
    /// Grid size configuration
    pub grid_size: GridSize,
    
    /// Attribute handling mode
    pub attribute_mode: AttributeMode,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            percentile: 90,
            density_thresholds: None,
            min_density_trigger: 100.0,
            grid_size: GridSize::default(),
            attribute_mode: AttributeMode::Drop,
        }
    }
}
```

## CLI

```bash
# Enable coalescing (uses adaptive threshold)
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed

# Custom percentile (more aggressive)
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-percentile 75

# Set minimum density trigger
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-min-density 200

# Combine with attribute accumulation
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed -aC sum:population -aC concat:name

# Keep unconfigured attributes from first feature
gpq-tiles input.parquet output.pmtiles --coalesce-densest-as-needed --coalesce-attrs=keep-first
```

## Expected Performance

### Size Savings
- **30-60% tile size reduction** on dense datasets
- Multi* encoding shares feature metadata (tags, layer references)
- Delta-encoded coordinates compress better in Multi* geometries

### Speed Impact
- **Metadata scan:** O(row_groups × zoom_levels), ~100-500ms for large files
- **Per-tile coalescing:** O(n) with O(1) grid lookup
- **Net effect:** Faster downstream encoding (fewer features to serialize)

### When Coalescing Helps Most
- Low zoom levels (z0-z8) where features converge
- Dense urban datasets (buildings, POIs)
- Point-heavy datasets (Multi* encoding very efficient for points)

### When Coalescing Has Little Effect
- High zoom levels with naturally sparse tiles
- Large polygon datasets (few features per tile)
- Already-simplified low-zoom layers

## Test Plan

1. **Unit tests** for `coalesce_geometries()` - all type combinations including edge cases
2. **Unit tests** for `SpatialGrid` - cell assignment, centroid failures
3. **Unit tests** for `estimate_tile_density()` - various bbox/zoom combinations
4. **Integration tests** with gpio-optimized dense GeoParquet files
5. **Regression tests** ensuring coalescing doesn't change tile coverage
6. **Benchmark** tile sizes with/without coalescing on Overture buildings

## Tasks

### Phase 1: Foundation (TDD)
- [ ] Add `covering_tiles()` to `covering.rs` + tests
- [ ] Add `percentile()` to `gap_density.rs` + tests

### Phase 2: Core Types (TDD)
- [ ] Create `crates/core/src/coalesce.rs`
- [ ] Implement `coalesce_geometries()` + exhaustive type tests
- [ ] Implement `SpatialGrid` with centroid fallback + tests
- [ ] Implement `CoalesceTargets` + tests

### Phase 3: Integration
- [ ] Add `CoalesceConfig` to `TilerConfig`
- [ ] Integrate with `pipeline.rs` tile encoding
- [ ] Add CLI flags to `main.rs`

### Phase 4: Validation
- [ ] Integration test with gpio-optimized dense dataset
- [ ] Benchmark tile sizes before/after coalescing
- [ ] Compare output quality vs tippecanoe on same input
