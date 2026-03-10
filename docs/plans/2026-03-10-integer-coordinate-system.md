# Integer Coordinate System Migration Plan

**Issue:** [#85 - feat: Implement tippecanoe-style tiny polygon accumulation](https://github.com/geoparquet-io/gpq-tiles/issues/85)
**Date:** 2026-03-10
**Status:** Phase 0 Implementation

## Executive Summary

This document outlines the parallelization strategy for migrating gpq-tiles from floating-point geographic coordinates (f64 lat/lng) to 32-bit integer world coordinates, matching tippecanoe's approach for improved precision and performance.

## Background

### Current State (gpq-tiles)
- Coordinates: `f64` geographic (longitude/latitude in degrees)
- Conversion: Geographic → tile-local integers only at MVT encoding time
- Types: `TileCoord { x: u32, y: u32, z: u8 }`, `TileBounds { lng_min/max, lat_min/max: f64 }`

### Target State (tippecanoe parity)
- Coordinates: `i32` world coordinates (2^32 units cover the world at zoom 0)
- Conversion: Geographic → world coords once at ingestion, then integer math throughout
- Benefits: No floating-point accumulation errors, faster operations, deterministic output

### Tippecanoe Reference (geometry.hpp, projection.cpp)
```cpp
// Tippecanoe's world coordinate system
// At zoom 0: world spans [0, 2^32) in both x and y
// At zoom z: shift by (32 - z) to get tile coordinates

// projection.cpp:88
*y = std::round(((1LL << 32) - 1) - (iy * (1LL << 31) / 6378137.0 / M_PI + (1LL << 31)));

// Coordinate struct (geometry.hpp:26-29)
struct draw {
    long long x : 40;  // Uses 40 bits for extended precision in operations
    long long y : 40;
};
```

## Migration Phases

### Phase 0: Core Types & Conversion Traits (This PR)
**Duration:** 2-3 hours
**Dependencies:** None

Deliverables:
1. `WorldCoord` struct with i32 x/y coordinates
2. Conversion traits: `f64 (lng/lat)` ↔ `WorldCoord` ↔ tile-local coords
3. Unit tests for precision edge cases
4. Integration test verifying tippecanoe-equivalent output

### Phase 1: Parallel Module Refactors (4 agents, separate PRs)

| Agent | Module(s) | Lines | Duration | Branch |
|-------|-----------|-------|----------|--------|
| 1 | clip.rs, sutherland_hodgman.rs, wagyu_clip.rs, hierarchical_clip.rs | 3,082 | 4-6 hours | `feat/int-coords-clipping` |
| 2 | simplify.rs | 668 | 2-3 hours | `feat/int-coords-simplify` |
| 3 | feature_drop.rs | 1,922 | 3-4 hours | `feat/int-coords-feature-drop` |
| 4 | mvt.rs, validate.rs | 1,915 | 2-3 hours | `feat/int-coords-mvt-validate` |

### Phase 2: Pipeline Integration (sequential, after Phase 1 merges)
**Duration:** 4-6 hours
**Branch:** `feat/int-coords-pipeline`

### Phase 3: Final Validation
**Duration:** 3-4 hours
- Tippecanoe golden test comparison
- Performance benchmarks (expect 10-20% improvement in coordinate-heavy ops)
- Documentation updates

## Technical Design

### New Types (Phase 0)

```rust
/// 32-bit world coordinate system, matching tippecanoe.
///
/// At zoom 0, the world spans [0, 2^32) in both x and y.
/// At zoom z, divide by 2^(32-z) to get tile coordinates.
///
/// # Coordinate System
/// - Origin (0, 0): Northwest corner (lng=-180, lat=~85.05)
/// - X increases eastward
/// - Y increases southward (Web Mercator convention)
///
/// # Precision
/// At zoom 32, 1 unit ≈ 0.009 meters at equator
/// At zoom 20, 1 unit ≈ 0.149 meters at equator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorldCoord {
    pub x: i32,
    pub y: i32,
}

/// Conversion trait for geographic to world coordinates
pub trait ToWorldCoord {
    fn to_world_coord(&self) -> WorldCoord;
}

/// Conversion trait from world to geographic coordinates
pub trait FromWorldCoord {
    fn from_world_coord(coord: WorldCoord) -> Self;
}
```

### Conversion Functions

```rust
/// Convert longitude/latitude to world coordinates.
///
/// Uses Web Mercator projection (EPSG:3857) with 32-bit precision.
///
/// # Arguments
/// * `lng` - Longitude in degrees [-180, 180]
/// * `lat` - Latitude in degrees [-85.05, 85.05] (Web Mercator bounds)
///
/// # Returns
/// WorldCoord with x, y in [0, 2^32) space
pub fn lng_lat_to_world(lng: f64, lat: f64) -> WorldCoord {
    const WORLD_SCALE: f64 = (1_i64 << 32) as f64;  // 2^32

    // Longitude → x: simple linear mapping
    let x = ((lng + 180.0) / 360.0 * WORLD_SCALE) as i32;

    // Latitude → y: Web Mercator projection
    let lat_rad = lat.clamp(-85.05, 85.05).to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0 * WORLD_SCALE) as i32;

    WorldCoord { x, y }
}

/// Convert world coordinates to tile-local coordinates at a given zoom.
///
/// # Arguments
/// * `coord` - World coordinate
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates [0, extent]
pub fn world_to_tile_local(coord: WorldCoord, tile: &TileCoord, extent: u32) -> (i32, i32) {
    let shift = 32 - tile.z;
    let tile_size = 1_i64 << shift;

    // Position within tile (0.0 to 1.0 range, scaled to extent)
    let tile_x = tile.x as i64 * tile_size;
    let tile_y = tile.y as i64 * tile_size;

    let local_x = ((coord.x as i64 - tile_x) * extent as i64 / tile_size) as i32;
    let local_y = ((coord.y as i64 - tile_y) * extent as i64 / tile_size) as i32;

    (local_x, local_y)
}
```

## Parallelization Strategy

### Agent Coordination

```
                    ┌─────────────────────────────────────────────────────────────┐
                    │ PHASE 0: Core Types (main agent) - 2-3 hours               │
                    │ Branch: feat/integer-coordinate-system-phase0              │
                    │ • WorldCoord struct + conversions                          │
                    │ • Unit tests + integration tests                           │
                    │ • This document                                            │
                    └─────────────────────────────────────────────────────────────┘
                                               │
                                        (merge to main)
                                               │
         ┌──────────────────┬──────────────────┼──────────────────┬──────────────────┐
         ▼                  ▼                  ▼                  ▼                  │
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐     │
│   AGENT 1       │ │   AGENT 2       │ │   AGENT 3       │ │   AGENT 4       │     │
│ Clipping Suite  │ │ Simplification  │ │ Feature Drop    │ │ MVT + Validate  │     │
│                 │ │                 │ │                 │ │                 │     │
│ git worktree:   │ │ git worktree:   │ │ git worktree:   │ │ git worktree:   │     │
│ wt-clipping/    │ │ wt-simplify/    │ │ wt-feature-drop/│ │ wt-mvt/         │     │
│                 │ │                 │ │                 │ │                 │     │
│ Files:          │ │ Files:          │ │ Files:          │ │ Files:          │     │
│ • clip.rs       │ │ • simplify.rs   │ │ • feature_drop  │ │ • mvt.rs        │     │
│ • sutherland_*  │ │                 │ │                 │ │ • validate.rs   │     │
│ • wagyu_clip    │ │                 │ │                 │ │                 │     │
│ • hierarchical_*│ │                 │ │                 │ │                 │     │
└────────┬────────┘ └────────┬────────┘ └────────┬────────┘ └────────┬────────┘     │
         │                   │                   │                   │              │
         └───────────────────┴───────────────────┴───────────────────┘              │
                                               │                                    │
                                    (all branches merged)                           │
                                               │                                    │
                    ┌──────────────────────────▼────────────────────────────────────┘
                    │ PHASE 2: Pipeline Integration - 4-6 hours                    │
                    │ Branch: feat/int-coords-pipeline                             │
                    │ • pipeline.rs refactor                                       │
                    │ • Wire up all converted modules                              │
                    └─────────────────────────────────────────────────────────────┘
                                               │
                    ┌──────────────────────────▼────────────────────────────────────┐
                    │ PHASE 3: Final Validation - 3-4 hours                        │
                    │ • Golden tests vs tippecanoe                                 │
                    │ • Performance benchmarks                                     │
                    │ • Documentation                                              │
                    └─────────────────────────────────────────────────────────────┘
```

### Worktree Commands (for Phase 1)

```bash
# Create worktrees for parallel agents
git worktree add ../wt-clipping feat/int-coords-clipping
git worktree add ../wt-simplify feat/int-coords-simplify
git worktree add ../wt-feature-drop feat/int-coords-feature-drop
git worktree add ../wt-mvt feat/int-coords-mvt-validate

# Each agent works in isolation, then creates PR
# Merge order: clipping → simplify → feature-drop → mvt (any order works, minimal deps)
```

## Risk Mitigation

### Merge Conflicts
- **Shared type changes:** All agents depend on `WorldCoord` from Phase 0
- **Mitigation:** Phase 0 must be merged before Phase 1 begins
- **Interface contract:** Document exact function signatures in this plan

### Precision Edge Cases
- **Antimeridian crossing:** Test coordinates near ±180°
- **Polar regions:** Test coordinates near ±85.05° (Web Mercator bounds)
- **Integer overflow:** Use i64 for intermediate calculations, cast to i32 at end

### Performance Regression
- **Benchmark baseline:** Run `cargo bench` before changes
- **Target:** No regression in clipping/simplification benchmarks
- **Expected improvement:** 10-20% in coordinate-heavy operations

## Acceptance Criteria

### Phase 0 (This PR)
- [ ] `WorldCoord` type with i32 x/y coordinates
- [ ] `lng_lat_to_world()` and `world_to_lng_lat()` conversions
- [ ] `world_to_tile_local()` and `tile_local_to_world()` conversions
- [ ] Unit tests for coordinate conversions
- [ ] Unit tests for precision edge cases (antimeridian, poles, equator)
- [ ] Integration test comparing output to tippecanoe
- [ ] This plan document

### Phase 1-3 (Future PRs)
- [ ] All coordinate operations use `WorldCoord` internally
- [ ] `geo::Geometry<f64>` only at input/output boundaries
- [ ] Tippecanoe golden tests pass
- [ ] No performance regression

## Timeline Estimate

| Phase | Sequential | Parallel (4 agents) |
|-------|------------|---------------------|
| Phase 0 | 2-3 hours | 2-3 hours (not parallelizable) |
| Phase 1 | 12-16 hours | 4-6 hours |
| Phase 2 | 4-6 hours | 4-6 hours (not parallelizable) |
| Phase 3 | 3-4 hours | 3-4 hours |
| **Total** | **21-29 hours** | **13-19 hours** |

With coordination overhead and merge conflict resolution, realistic wall-clock time with 4 agents: **2-3 days**.

## References

- [Issue #85](https://github.com/geoparquet-io/gpq-tiles/issues/85) - Original issue
- [tippecanoe geometry.hpp](https://github.com/felt/tippecanoe/blob/main/geometry.hpp) - Reference implementation
- [tippecanoe projection.cpp](https://github.com/felt/tippecanoe/blob/main/projection.cpp) - Coordinate conversion
