# API Reference

## CLI

```bash
gpq-tiles [OPTIONS] <INPUT> <OUTPUT>
```

### Arguments

| Argument | Description |
|----------|-------------|
| `INPUT` | Input GeoParquet file path |
| `OUTPUT` | Output PMTiles file path |

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--min-zoom <N>` | `0` | Minimum zoom level |
| `--max-zoom <N>` | `14` | Maximum zoom level |
| `--drop-density <LEVEL>` | `medium` | Feature dropping: `low`, `medium`, `high` |
| `--layer-name <NAME>` | (from filename) | MVT layer name |
| `--compression <ALG>` | `gzip` | Compression: `gzip`, `zstd`, `brotli`, `none` |
| `--streaming-mode <MODE>` | `fast` | Streaming: `fast`, `low-memory` |
| `-y, --include <FIELD>` | (all) | Include property (repeatable) |
| `-x, --exclude <FIELD>` | (none) | Exclude property (repeatable) |
| `-X, --exclude-all` | false | Exclude all properties (geometry only) |
| `--simplify` | false | Enable zoom-dependent geometry simplification |
| `--simplify-factor <F>` | `1.0` | Simplification factor (pixel tolerance) |
| `--no-parallel` | false | Disable parallel tile processing |
| `--no-parallel-geoms` | false | Disable parallel geometry processing |
| `-v, --verbose` | false | Show progress bars |
| `--quiet` | false | Suppress optimization warnings |

### Examples

```bash
# Basic
gpq-tiles input.parquet output.pmtiles

# Full options
gpq-tiles input.parquet output.pmtiles \
  --min-zoom 0 \
  --max-zoom 14 \
  --compression zstd \
  --include name \
  --include population \
  --verbose
```

---

## Python API

### `convert()`

```python
from gpq_tiles import convert

convert(
    input: str,
    output: str,
    min_zoom: int = 0,
    max_zoom: int = 14,
    drop_density: str = "medium",
    compression: str = "gzip",
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    exclude_all: bool = False,
    layer_name: str | None = None,
    streaming_mode: str = "fast",
    parallel_tiles: bool = True,
    parallel_geoms: bool = True,
    simplify_factor: float | None = None,
    progress_callback: Callable[[dict], None] | None = None,
) -> None
```

**Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `input` | `str` | (required) | Input GeoParquet file path |
| `output` | `str` | (required) | Output PMTiles file path |
| `min_zoom` | `int` | `0` | Minimum zoom level (0-22) |
| `max_zoom` | `int` | `14` | Maximum zoom level (0-22) |
| `drop_density` | `str` | `"medium"` | Feature dropping: `"low"`, `"medium"`, `"high"` |
| `compression` | `str` | `"gzip"` | Compression: `"gzip"`, `"brotli"`, `"zstd"`, `"none"` |
| `include` | `list[str]` | `None` | Whitelist of property names to include |
| `exclude` | `list[str]` | `None` | Blacklist of property names to exclude |
| `exclude_all` | `bool` | `False` | Exclude all properties (geometry only) |
| `layer_name` | `str` | `None` | Override layer name (default: input filename) |
| `streaming_mode` | `str` | `"fast"` | Memory mode: `"fast"` or `"low-memory"` |
| `parallel_tiles` | `bool` | `True` | Enable parallel tile generation |
| `parallel_geoms` | `bool` | `True` | Enable parallel geometry processing |
| `simplify_factor` | `float` | `None` | Simplification factor (None = disabled, 1.0 = 1 pixel tolerance) |
| `progress_callback` | `Callable` | `None` | Callback for progress events |

**Raises:**

- `TypeError` — `progress_callback` is not callable
- `ValueError` — Invalid parameter value or conflicting filter options
- `RuntimeError` — Conversion failed (file not found, invalid GeoParquet, etc.)

**Examples:**

```python
from gpq_tiles import convert

# Basic conversion
convert("buildings.parquet", "buildings.pmtiles")

# With options
convert(
    input="buildings.parquet",
    output="buildings.pmtiles",
    min_zoom=0,
    max_zoom=14,
    compression="zstd",
    drop_density="high"
)

# Property filtering
convert("data.parquet", "out.pmtiles", include=["name", "population"])
convert("data.parquet", "out.pmtiles", exclude=["internal_id"])
convert("data.parquet", "out.pmtiles", exclude_all=True)  # geometry only

# Large file handling
convert("huge.parquet", "out.pmtiles", streaming_mode="low-memory")

# Simplification for linear features
convert("roads.parquet", "roads.pmtiles", simplify_factor=1.0)  # 1 pixel tolerance
convert("roads.parquet", "roads.pmtiles", simplify_factor=0.5)  # More detail preserved

# Progress callback
def on_progress(event):
    if event["phase"] == "complete":
        print(f"Generated {event['total_tiles']} tiles in {event['duration_secs']:.1f}s")
    elif event["phase"] == "phase1_progress":
        print(f"Reading row group {event['row_group']}/{event['total_row_groups']}")

convert("data.parquet", "out.pmtiles", progress_callback=on_progress)
```

**Progress Events:**

When using `progress_callback`, the callback receives a dict with a `"phase"` key:

| Phase | Description | Additional Keys |
|-------|-------------|-----------------|
| `"start"` | Phase started | `phase_num`, `name` |
| `"phase1_progress"` | Reading row groups | `row_group`, `total_row_groups`, `features_in_group`, `records_written` |
| `"phase1_complete"` | Reading complete | `total_records`, `peak_memory_bytes` |
| `"phase2_start"` | Sorting started | — |
| `"phase2_complete"` | Sorting complete | — |
| `"phase3_progress"` | Encoding tiles | `tiles_written`, `records_processed`, `total_records` |
| `"complete"` | All done | `total_tiles`, `peak_memory_bytes`, `duration_secs` |

---

## Rust API

### High-Level API

The `Converter` provides a simple, opinionated interface:

```rust
use gpq_tiles_core::{Converter, Config, Compression, PropertyFilter};

let config = Config {
    min_zoom: 0,
    max_zoom: 14,
    compression: Compression::Gzip,  // Default, maximum PMTiles viewer compatibility
    property_filter: PropertyFilter::Include(vec!["name".into(), "population".into()]),
    ..Default::default()
};

let converter = Converter::new(config);
converter.convert("input.parquet", "output.pmtiles")?;
```

#### `Config`

```rust
pub struct Config {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub extent: u32,
    pub drop_density: DropDensity,
    pub layer_name: Option<String>,
    pub property_filter: PropertyFilter,
    pub compression: Compression,
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `min_zoom` | `0` | Minimum zoom level |
| `max_zoom` | `14` | Maximum zoom level |
| `extent` | `4096` | Tile extent (MVT spec) |
| `drop_density` | `Medium` | Feature dropping level |
| `layer_name` | `None` | MVT layer name (derived from filename if None) |
| `property_filter` | `None` | Property filtering strategy |
| `compression` | `Gzip` | Compression algorithm |

#### `PropertyFilter`

```rust
pub enum PropertyFilter {
    None,                         // Include all properties
    Include(Vec<String>),        // Whitelist
    Exclude(Vec<String>),        // Blacklist
    ExcludeAll,                  // Geometry only
}
```

#### `Compression`

```rust
pub enum Compression {
    None = 1,
    Gzip = 2,
    Brotli = 3,
    Zstd = 4,
}
```

### Low-Level API

For fine-grained control, use the pipeline directly:

```rust
use gpq_tiles_core::pipeline::{generate_tiles, TilerConfig};
use gpq_tiles_core::PropertyFilter;
use std::path::Path;

let config = TilerConfig::new(0, 14)
    .with_density_drop(true)
    .with_density_max_per_cell(3)
    .with_property_filter(PropertyFilter::Include(vec!["name".into()]))
    .with_layer_name("buildings");

// With simplification for linear features
let config_simplified = TilerConfig::new(0, 14)
    .with_simplify(1.0)  // 1 pixel tolerance
    .with_layer_name("roads");

let tiles = generate_tiles(Path::new("input.parquet"), &config)?;

for tile_result in tiles {
    let tile = tile_result?;
    println!("z={} x={} y={}: {} bytes, {} features",
             tile.coord.z, tile.coord.x, tile.coord.y,
             tile.data.len(), tile.feature_count);
}
```

#### `TilerConfig` Builder

```rust
TilerConfig::new(min_zoom: u8, max_zoom: u8)
    .with_extent(extent: u32)                           // Default: 4096
    .with_buffer_pixels(pixels: u32)                    // Default: 8
    .with_layer_name(name: &str)                        // Default: "layer"
    .with_density_drop(enabled: bool)                   // Default: true
    .with_density_cell_size(pixels: u32)                // Default: 32
    .with_density_max_per_cell(max: usize)              // Default: 1
    .with_hilbert_sorting(enabled: bool)                // Default: true
    .with_property_filter(filter: PropertyFilter)       // Default: None
    .with_simplify(factor: f64)                         // Default: None (disabled)
    .with_parallel_tiles(enabled: bool)                 // Default: true
    .with_parallel_geoms(enabled: bool)                 // Default: true
    .with_quiet(enabled: bool)                          // Default: false
    .with_streaming_mode(mode: StreamingMode)           // Default: Fast
```

#### `StreamingMode`

```rust
pub enum StreamingMode {
    Fast,         // Row-group streaming (default)
    LowMemory,    // External sort
}
```

### Streaming API

For progress reporting and writer control:

```rust
use gpq_tiles_core::pipeline::{generate_tiles_to_writer_with_progress, ProgressEvent, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use std::sync::Arc;

let config = TilerConfig::new(0, 14);
let mut writer = StreamingPmtilesWriter::new();

let progress = Arc::new(|event: ProgressEvent| {
    match event {
        ProgressEvent::Phase1Progress { row_group, total_row_groups, .. } => {
            println!("Reading row group {}/{}", row_group, total_row_groups);
        }
        ProgressEvent::Phase3Progress { tiles_written, .. } => {
            println!("Encoded {} tiles", tiles_written);
        }
        ProgressEvent::Complete { total_tiles, duration_secs, .. } => {
            println!("Done: {} tiles in {:.1}s", total_tiles, duration_secs);
        }
        _ => {}
    }
});

generate_tiles_to_writer_with_progress(
    Path::new("input.parquet"),
    &config,
    &mut writer,
    progress
)?;

writer.write_to_file("output.pmtiles")?;
```

---

## Types

### `TileCoord`

```rust
pub struct TileCoord {
    pub x: u32,
    pub y: u32,
    pub z: u8,
}
```

Web Mercator tile coordinates (XYZ scheme).

### `TileBounds`

```rust
pub struct TileBounds {
    pub lng_min: f64,
    pub lat_min: f64,
    pub lng_max: f64,
    pub lat_max: f64,
}
```

Geographic bounds in WGS84 (longitude/latitude degrees).

### `GeneratedTile`

```rust
pub struct GeneratedTile {
    pub coord: TileCoord,
    pub data: Vec<u8>,           // Uncompressed MVT bytes
    pub feature_count: usize,
}
```

A single generated tile with its MVT-encoded data.

---

## Error Handling

### Rust

```rust
use gpq_tiles_core::Error;

match converter.convert("input.parquet", "output.pmtiles") {
    Ok(()) => println!("Success!"),
    Err(Error::GeoParquetRead(msg)) => eprintln!("Failed to read input: {}", msg),
    Err(Error::PMTilesWrite(msg)) => eprintln!("Failed to write output: {}", msg),
    Err(Error::InvalidGeometry { feature_id, reason }) => {
        eprintln!("Invalid geometry at feature {}: {}", feature_id, reason);
    }
    Err(e) => eprintln!("Error: {}", e),
}
```

### Python

```python
from gpq_tiles import convert

try:
    convert("input.parquet", "output.pmtiles")
except ValueError as e:
    print(f"Invalid parameter: {e}")
except RuntimeError as e:
    print(f"Conversion failed: {e}")
```
