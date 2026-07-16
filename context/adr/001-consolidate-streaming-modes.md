# ADR-001: Consolidate to Single Geometry-Centric Pipeline

**Date:** 2026-02-25
**Status:** Superseded — the pipeline this ADR consolidated was removed
entirely with the legacy tiles pipeline (#177, PR #189); see the decision
record in `context/ARCHITECTURE.md`. Kept as the historical record of
the consolidation.
**Context:** PR #63

## Summary

Remove the `StreamingMode` enum (`Fast`, `LowMemory`, `ExternalSort`) and consolidate to a single geometry-centric algorithm that is both fast AND memory-bounded.

## Context

### The Problem

The codebase had **two fundamentally different algorithms** with dramatically different performance characteristics:

| Algorithm | Code Path | Complexity | Memory | Used By |
|-----------|-----------|------------|--------|---------|
| Tile-centric | `TileIterator` | O(T × G) | All geometries in RAM | Benchmarks, old API |
| Geometry-centric | `generate_tiles_to_writer` with ExternalSort | O(G × t) | Bounded by sorter | CLI (production) |

Where:
- T = total tiles (can be 100K+ at higher zooms)
- G = total geometries
- t = average tiles per geometry (typically 10-50)

### The Math

For a 363K feature file at z0-z6:
- **Tile-centric**: ~36 billion intersection checks
- **Geometry-centric**: ~18 million checks (**2000× fewer**)

Our benchmarks were measuring the **wrong algorithm**, which is why performance optimizations to `TileIterator` had limited impact.

### Industry Validation

After examining tippecanoe's source code (`tile.cpp`), we confirmed it uses the geometry-centric approach: `serialize_feature()` → sort → `write_tile()`. The external sort pattern is industry standard for tile generation.

## Decision

1. **Remove `StreamingMode` enum** - No more choosing between Fast, LowMemory, or ExternalSort
2. **Always use geometry-centric with external sort** - Single algorithm that's fast AND memory-bounded
3. **Remove parallel configuration flags** - Replace `--no-parallel` and `--no-parallel-geoms` with single `--deterministic` flag
4. **Update benchmarks** - Use `generate_tiles_to_writer` (production path), not `TileIterator`

### New API

```rust
// Before (v0.3.x)
let config = TilerConfig::new(0, 14)
    .with_streaming_mode(StreamingMode::ExternalSort)
    .with_parallel(true)
    .with_parallel_geoms(true);

// After (v0.4.0)
let config = TilerConfig::new(0, 14)
    .with_deterministic(false);  // Optional, defaults to false (parallel)
```

### CLI Changes

```bash
# Before
tylertoo input.parquet output.pmtiles --streaming-mode external-sort --no-parallel

# After
tylertoo input.parquet output.pmtiles --deterministic
```

## Consequences

### Positive

1. **Simpler mental model** - One algorithm, one code path
2. **Correct benchmarks** - Now measure what production actually uses
3. **Memory-bounded by default** - No risk of OOM on large files
4. **Faster for all file sizes** - O(G × t) beats O(T × G) always
5. **Easier maintenance** - Less code to maintain and debug

### Negative

1. **Breaking change** - `StreamingMode` enum removed
2. **Small file overhead** - External sort adds WKB serialization overhead
   - Mitigated: For files with <100K records, buffer stays in memory
3. **Less granular parallelism control** - Only on/off, not separate tile/geom parallelism
   - Mitigated: Single `--deterministic` flag covers debugging use case

### Neutral

1. **`TileIterator` still exists** - Used by `Converter` API and tests
   - Future work: Migrate `Converter` to use streaming API

## Alternatives Considered

### Keep All Three Modes

**Rejected:** Adds complexity, users must understand tradeoffs, benchmarks were misleading.

### Add Bbox Pre-filter to TileIterator

**Rejected:** Doesn't change fundamental O(T × G) complexity. Even with pre-filter, still iterates all tiles.

### Remove TileIterator Entirely

**Deferred:** Breaking change to `Converter::convert()` API. Keep for now, deprecate in future.

## Implementation Notes

### Parallelism Control

The `deterministic` flag controls two parallel paths:

1. **Geometry-level parallelism** - Process geometries in parallel (rayon)
2. **Tile-level parallelism** - For large geometries (>1000 tiles), process tiles in parallel

Both are disabled when `deterministic = true`.

### Memory Budget

The advisory memory budget (`with_memory_budget()`) affects:
- Row group processing order
- Sorter buffer size
- Warning thresholds

It does NOT hard-limit memory - row groups are processed atomically.

## Related

- **Issue #53**: TileIterator missing bbox pre-filter (led to this investigation)
- **PR #36**: Added parallel tile processing within large geometries
- **tippecanoe**: Reference implementation (`tile.cpp`)
