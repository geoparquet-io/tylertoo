# gpq-tiles Architecture

Design decisions and tippecanoe divergences.

## Design Principles

1. **Arrow-First**: Process geometries within Arrow batch scope for zero-copy benefits
2. **Semantic Comparison**: Golden tests use IoU and feature counts, not byte-exact comparison
3. **Reference Implementations**: All algorithms match tippecanoe behavior; document divergences
4. **PMTiles Writer**: The `pmtiles` crate is read-only; we implement our own v3 writer

## Known Divergences from Tippecanoe

| Area | Our Approach | Tippecanoe | Notes |
|------|--------------|------------|-------|
| Metadata | Basic `vector_layers` | Full layer/field/tilestats | Field metadata planned |
| Simplification | Custom tolerance formula | `tile.cpp` | Tuned to match output quality |
| Density dropping | **Gap-based OR Grid-based** | Hilbert-gap selection | **MATCHES** with `--gamma` (Issue #24) |
| Polygon clipping | Sutherland-Hodgman (f64) | Sutherland-Hodgman (int) | Same algorithm, different coordinate space |
| Tiny polygon handling | **Accumulation** | Accumulation | **MATCHES** (Issue #85) |

## Polygon Clipping: Sutherland-Hodgman

**DIVERGENCE**: Tippecanoe uses Sutherland-Hodgman in integer tile coordinates (0-4096). We use the same Sutherland-Hodgman algorithm but operate in f64 geographic coordinates to avoid coordinate conversion overhead.

**Why Sutherland-Hodgman instead of Wagyu/Vatti:**
- Tile clipping is always against axis-aligned rectangles
- SH is O(n) per polygon ring; Vatti is O(n log n) for general boolean ops
- A 316k-coordinate polygon clips in 0.02s with SH vs 10.4s with Wagyu (500x faster)
- SH matches tippecanoe's clip.cpp approach exactly

**Known behavior difference from Wagyu:**
- SH does not split disconnected clipping results into separate polygons
- A U-shape clipped across its opening produces a single (possibly self-touching) polygon, not two
- For tile rendering purposes, this is acceptable and matches tippecanoe's behavior

**Wagyu is retained** in `wagyu_clip.rs` for potential future use in complex boolean operations, but is not used in the hot clipping path.

## Density-Based Dropping

**MATCHES TIPPECANOE** (with `--gamma`): We now support tippecanoe's gap-based selection algorithm using Hilbert index gaps.

### Gap-Based Selection (Recommended)

**Reference**: tippecanoe `tile.cpp:manage_gap()`

The gap-based algorithm processes features sorted by Hilbert curve index and uses the gap between consecutive features to decide whether to drop. This provides excellent preservation of spatial distribution.

```rust
// Enable gap-based dropping (tippecanoe-compatible)
let config = TilerConfig::new(0, 14)
    .with_drop_densest_as_needed();  // gamma=2.0 (tippecanoe default)

// Or with custom gamma
let config = TilerConfig::new(0, 14)
    .with_gamma(2.0);  // Explicit gamma value
```

**CLI:**
```bash
gpq-tiles input.parquet output.pmtiles --drop-densest-as-needed
gpq-tiles input.parquet output.pmtiles --gamma=2.0
```

**Gamma values:**
- `gamma=0`: Disabled (use grid-based instead)
- `gamma=1`: Linear spacing
- `gamma=2`: "Reduces dots < 1 pixel apart to square root of original" (tippecanoe default)
- Higher values: More aggressive dropping of closely-spaced features

### Grid-Based Selection (Legacy)

For backward compatibility, we also support simpler grid-cell limiting:

```rust
// Enable grid-based density dropping
let config = TilerConfig::new(0, 14)
    .with_density_drop(true)
    .with_density_cell_size(32);   // Pixels per cell
```

**Cell size reference** (Z8, 4096 extent):

| Cell Size | Grid | Typical Features |
|-----------|------|------------------|
| 16px | 256×256 | 34 |
| 32px | 128×128 | 23 |
| 64px | 64×64 | 13 |
| 128px | 32×32 | 9 |

**Note:** Gap-based selection takes precedence when `gamma` is set.

## Tiny Polygon Accumulation (Issue #85)

**MATCHES TIPPECANOE**: Instead of dropping tiny polygons (diffuse probability), we accumulate their area and emit synthetic pixel-sized squares when the accumulated area exceeds a threshold. This preserves visual density — 10 tiny polygons in a cluster become a single visible square.

**Reference**: tippecanoe `clip.cpp:1048-1097`

**How it works:**

```
For each tile being encoded:
1. Create TinyPolygonAccumulator
2. For each polygon:
   a. Check if polygon is "tiny" (area < 4 sq pixels)
   b. If tiny: accumulate area + weighted centroid
   c. If accumulated area >= threshold: emit synthetic square at centroid, reset
3. Emit any remaining accumulated area as final synthetic square
```

**Configuration:**

```rust
// Enabled by default (matches tippecanoe)
let config = TilerConfig::new(0, 14);

// Disable to use legacy diffuse probability dropping
let config = TilerConfig::new(0, 14)
    .with_tiny_polygon_accumulation(false);
```

**Implementation details:**

- Accumulator uses `u128` for area to avoid overflow when accumulating many tiny polygons
- Weighted centroid calculated using area-weighted average of polygon centroids
- Synthetic squares are 1 pixel × 1 pixel at the accumulated centroid
- Threshold: 4 square pixels (matches tippecanoe's default)

**Why this matters:**

At low zoom levels, many small features (building footprints, parcels) become sub-pixel and would disappear entirely with simple dropping. By accumulating them, we preserve the visual density — a city with 10,000 tiny buildings still shows as a populated area, not empty space.

## Attribute Accumulation (Issue #23)

**MATCHES TIPPECANOE**: When features are merged during tile generation, attributes can be combined using configurable accumulator operations. This matches tippecanoe's `-ac` flag behavior.

**Reference**: tippecanoe command-line options and attribute accumulation

**Supported Operations:**

| Operation | Behavior | Type Handling |
|-----------|----------|---------------|
| `sum`     | Add numeric values | Strings → 0.0 |
| `product` | Multiply numeric values | Strings → 0.0, missing → 1.0 |
| `mean`    | Running average with count tracking | Stored as Float |
| `max`     | Keep maximum value | Strings skipped |
| `min`     | Keep minimum value | Strings skipped |
| `concat`  | Concatenate strings directly | Numbers → string |
| `comma`   | Concatenate with comma separator | Numbers → string |
| `count`   | Count merged features | Increments per accumulation |

**CLI Usage:**

```bash
gpq-tiles input.parquet output.pmtiles \
  --accumulate population:sum \
  --accumulate names:comma \
  --accumulate max_height:max
```

**API Usage:**

```rust
use gpq_tiles_core::accumulator::{AccumulatorConfig, AccumulatorOp};
use gpq_tiles_core::pipeline::TilerConfig;

let mut acc_config = AccumulatorConfig::new();
acc_config.set_operation("population", AccumulatorOp::Sum);
acc_config.set_operation("names", AccumulatorOp::Comma);

let config = TilerConfig::new(0, 14)
    .with_accumulator(acc_config);
```

**Key Behaviors (tippecanoe-compatible):**

1. **Unspecified attributes are DROPPED**: Only attributes with configured operations are preserved in the output. This matches tippecanoe's behavior.

2. **Mean requires count tracking**: The accumulator tracks a separate count per mean attribute to compute correct running averages.

3. **Type coercion rules**:
   - Numeric ops (`sum`, `product`, `mean`): strings treated as 0.0
   - Comparison ops (`min`, `max`): strings are skipped, numeric value preserved
   - String ops (`concat`, `comma`): numbers converted to string representation

**Implementation:**

- Module: `crates/core/src/accumulator.rs`
- `AccumulatorConfig` stores per-attribute operations and mean counts
- `accumulate()` method modifies target properties in-place

## Spatial Indexing

**We use space-filling curve sorting, not R-tree.**

| Consideration | R-tree | Space-filling Sort |
|---------------|--------|-------------------|
| Memory | +30-50% overhead | None |
| Access pattern | Random | Sequential |
| Streaming | Difficult | Natural |
| Complexity | Tree balancing | Standard sort |

**Hilbert vs Z-order:**
- Z-order (Morton): Simple bit interleaving, has "jumps" at quadrant boundaries
- Hilbert: Better locality, spatially adjacent points always close in 1D index

Default: Hilbert (matches tippecanoe's `-ah` flag).

## Golden Comparison Results

Validated against tippecanoe v2.49.0 using `open-buildings.parquet` (~1000 buildings):

| Zoom | Tippecanoe | gpq-tiles | Ratio |
|------|------------|-----------|-------|
| Z10 | 484 | 392 | 0.81x |
| Z8 | 97 | 76 | 0.78x |
| Z5 | 1 | ~200 | Configurable |

**Analysis:**
- We drop more aggressively at high zoom due to diffuse probability
- Area preservation after clip+simplify: 88%
- All zoom levels produce valid MVT tiles

## GeoParquet File Structure: Critical Performance Factor

**Row group size dramatically affects performance.** Our pipeline has per-row-group overhead (memory tracking, progress reporting, sorter flushes), so files with many small row groups are pathologically slow.

| File | Size | Rows | Row Groups | Rows/Group | Performance |
|------|------|------|------------|------------|-------------|
| ADM4 | 3.2 GB | 363,783 | 364 | ~1,000 | ✅ Good (3 min) |
| ADM2 | 1.9 GB | 43,064 | 4,307 | ~10 | ❌ Very slow |

**Rule of thumb:** Aim for 1,000+ rows per row group. Files with <100 rows/group will have significant overhead.

**Why this happens:**
- Each row group triggers progress callbacks, memory tracking, and sorter operations
- The external sort flushes buffers based on record count, not row groups
- Small row groups = more overhead per feature processed

**Recommendation:** If you control file creation, use larger row groups. If processing files with small row groups, consider consolidating them first with tools like DuckDB or gpio.

## Streaming Processing

### The Challenge

For a geometry that spans multiple tiles across multiple zoom levels, we need to store/process it multiple times. With 363K features at z0-z6, this can mean millions of geometry instances.

**The core problem:** PMTiles requires ALL features for a tile (z,x,y) to be encoded together. We can't write a tile incrementally as we encounter each feature.

### Algorithm: Geometry-Centric with External Sort

We use a **single geometry-centric algorithm** with external sort for bounded-memory processing.
This approach is both fast AND memory-bounded. See `context/adr/001-consolidate-streaming-modes.md` for the decision record.

**How it works:**

```
Phase 1: Read + Clip
├── For each row group in input file:
│   ├── Read geometries
│   ├── Sort by Hilbert index for cache-locality
│   └── For each geometry:
│       ├── Simplify once at max_zoom tolerance
│       ├── For each tile the geometry touches:
│       │   ├── Clip to tile bounds
│       │   ├── Serialize to WKB
│       │   └── Write (tile_id, feature_idx, wkb) to sorter
│       └── Process in parallel if geometry touches >1000 tiles
└── Memory bounded by row_group size + sorter buffer

Phase 2: Sort
├── External merge sort by tile_id
└── Uses memory-mapped I/O for efficiency

Phase 3: Encode
├── Read sorted records
├── Group consecutive records by tile_id
├── Encode MVT tiles
└── Write to StreamingPmtilesWriter
```

**Complexity comparison:**

| Algorithm | Complexity | Memory |
|-----------|------------|--------|
| Tile-centric (old) | O(tiles × geometries) | All geometries in RAM |
| Geometry-centric (current) | O(geometries × tiles_per_geom) | Row group + sorter buffer |

For a 363K feature file at z0-z6:
- Tile-centric: ~36 billion intersection checks
- Geometry-centric: ~18 million checks (2000× fewer)

### Deterministic Processing

By default, processing is parallelized for speed. Use `--deterministic` (CLI) or `.with_deterministic(true)` (API) for reproducible output:

```rust
let config = TilerConfig::new(0, 14)
    .with_deterministic(true);  // Sequential processing
```

### StreamingPmtilesWriter

The `StreamingPmtilesWriter` solves the memory problem for **output** (tiles):

| Component | PmtilesWriter | StreamingPmtilesWriter |
|-----------|---------------|------------------------|
| Tile data | In-memory BTreeMap | Temp file (disk) |
| Directory | Calculated at end | Built incrementally |
| Memory (30K tiles) | ~1.2 GB | ~2-3 MB |
| Deduplication | Hash → in-memory data | Hash → file offset |

**Usage:**

```rust
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::compression::Compression;

// Create streaming writer
let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)?;

// Generate tiles with optional memory budget
let config = TilerConfig::new(0, 14)
    .with_memory_budget(4 * 1024 * 1024 * 1024);  // 4GB advisory budget
generate_tiles_to_writer(Path::new("large.parquet"), &config, &mut writer)?;

// Finalize assembles: header + directory + metadata + tile_data
writer.finalize(Path::new("output.pmtiles"))?;
```

**Memory breakdown:**

```
StreamingPmtilesWriter memory:
├── Directory entries: 24 bytes × total_tiles  (~720 KB for 30K tiles)
├── Dedup cache:       40 bytes × unique_tiles (~800 KB for 20K unique)
├── Temp file buffer:  64 KB
└── Total:             ~2-3 MB
```

### File Quality Detection

Before streaming, `assess_quality()` checks input files and warns about suboptimal formats:

| Check | Cost | Action |
|-------|------|--------|
| Missing `geo` metadata | O(1) | Warn: "File missing GeoParquet metadata" |
| No row group bboxes | O(1) | Warn: "Cannot skip spatially" |
| Few row groups for size | O(1) | Warn: "Large file limits streaming efficiency" |
| Not Hilbert-sorted | O(1000) | Warn: "File not spatially sorted" (sampled) |

Warnings recommend optimizing with [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):

```
gpq optimize input.parquet -o optimized.parquet --hilbert
```

Use `config.with_quiet(true)` to suppress warnings. See `quality.rs` for implementation.

## Module Structure

```
crates/core/src/
├── lib.rs              # Public API
├── accumulator.rs      # Attribute accumulation for feature merging
├── tile.rs             # TileCoord, TileBounds
├── clip.rs             # Geometry clipping (dispatcher)
├── sutherland_hodgman.rs # O(n) polygon clipping for axis-aligned rectangles
├── wagyu_clip.rs       # Wagyu/Vatti clipping (retained for complex boolean ops)
├── simplify.rs         # RDP simplification
├── validate.rs         # Geometry validation
├── mvt.rs              # MVT encoding
├── pmtiles_writer.rs   # PMTiles v3 writer (PmtilesWriter + StreamingPmtilesWriter)
├── feature_drop.rs     # Dropping algorithms
├── spatial_index.rs    # Hilbert/Z-order curves
├── pipeline.rs         # Tile generation (streaming + non-streaming)
├── batch_processor.rs  # GeoArrow iteration (row group support)
├── memory.rs           # Memory tracking and estimation
├── quality.rs          # File quality assessment
├── dedup.rs            # Tile deduplication (XXH3)
├── compression.rs      # gzip/brotli/zstd compression
├── property_filter.rs  # Include/exclude field filtering
└── golden.rs           # Golden tests

crates/core/examples/
└── test_large_file.rs  # Large file streaming test
```
