# Getting Started

## Installation

### CLI (Cargo)

```bash
cargo install gpq-tiles
```

**Prerequisites:** Rust 1.75+, protoc

```bash
# macOS
brew install protobuf

# Ubuntu/Debian
apt install protobuf-compiler
```

### Python

```bash
pip install gpq-tiles
```

### From Source

```bash
git clone https://github.com/geoparquet-io/gpq-tiles.git
cd gpq-tiles
cargo build --release
```

## Input Requirements

**gpq-tiles requires WGS84 (EPSG:4326) coordinates.** If your GeoParquet file uses a projected CRS (e.g., UTM, British National Grid), you must reproject first using [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):

```bash
# Reproject to WGS84 with geoparquet-io (recommended)
gpio convert reproject input.parquet output.parquet -d EPSG:4326

# Optimize for streaming performance at the same time
gpio convert reproject input.parquet output.parquet -d EPSG:4326 --hilbert --row-group-size 100000
```

If you forget, gpq-tiles will error with a helpful message showing the detected CRS and the reprojection command.

**Why gpio?** Unlike generic tools, `gpio` produces optimized GeoParquet with Hilbert sorting and proper row group sizing, which dramatically improves gpq-tiles performance on large files.

## Basic Usage

### CLI

```bash
# Basic conversion
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 14

# Property filtering (matches tippecanoe -y/-x/-X)
gpq-tiles input.parquet output.pmtiles --include name --include population
gpq-tiles input.parquet output.pmtiles --exclude internal_id
gpq-tiles input.parquet output.pmtiles --exclude-all  # geometry only

# Compression options (gzip is default for compatibility)
gpq-tiles input.parquet output.pmtiles                       # gzip (default)
gpq-tiles input.parquet output.pmtiles --compression zstd    # faster encoding
gpq-tiles input.parquet output.pmtiles --compression brotli  # best ratio
gpq-tiles input.parquet output.pmtiles --compression none    # debugging only

# Verbose progress (useful for large files)
gpq-tiles input.parquet output.pmtiles --verbose

# Suppress optimization warnings
gpq-tiles input.parquet output.pmtiles --quiet
```

**Compression tradeoffs:**

| Algorithm | Notes |
|-----------|-------|
| `gzip` | **Default.** Maximum compatibility with PMTiles viewers (pmtiles.io, MapLibre, etc.) |
| `zstd` | Faster encoding, larger files. Use when your viewer supports it |
| `brotli` | Slowest encoding, best compression ratio |
| `none` | No compression (debugging only) |

Based on benchmark: 3.3GB GeoParquet, zoom 0-8. Gzip: 3:04 (175MB output), Zstd: 2:59 (254MB output).

### Python

**Current API** (basic conversion only):

```python
from gpq_tiles import convert

convert(
    input="buildings.parquet",
    output="buildings.pmtiles",
    min_zoom=0,
    max_zoom=14,
    compression="gzip",          # "gzip" | "zstd" | "brotli" | "none"
    drop_density="medium",       # "low" | "medium" | "high"
)
```

**Coming soon:** Property filtering, streaming modes, progress callbacks. Track progress in [#45](https://github.com/geoparquet-io/gpq-tiles/issues/45).

### Rust

**Low-level API** (full control):

```rust
use gpq_tiles_core::pipeline::{generate_tiles, TilerConfig};
use gpq_tiles_core::PropertyFilter;
use std::path::Path;

let config = TilerConfig::new(0, 14)
    .with_density_drop(true)
    .with_property_filter(PropertyFilter::Include(vec!["name".into(), "population".into()]))
    .with_layer_name("buildings");

let tiles = generate_tiles(Path::new("input.parquet"), &config)?;
for tile in tiles {
    let tile = tile?;
    println!("Tile z={} x={} y={}: {} bytes",
             tile.coord.z, tile.coord.x, tile.coord.y, tile.data.len());
}
```

**High-level API** (convenience wrapper):

```rust
use gpq_tiles_core::{Converter, Config, Compression, PropertyFilter};

let config = Config {
    min_zoom: 0,
    max_zoom: 14,
    compression: Compression::Gzip,  // Default, maximum compatibility
    property_filter: PropertyFilter::Exclude(vec!["internal_id".into()]),
    ..Default::default()
};

let converter = Converter::new(config);
converter.convert("input.parquet", "output.pmtiles")?;
```

## Optimizing Input Files

For best performance, optimize your GeoParquet files first:

```bash
# Check and fix GeoParquet formatting with geoparquet-io
gpio check all --fix input.parquet
```

gpq-tiles warns if input files lack optimization. See [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io) for details.

## Property Filtering

Control which attributes are included in output tiles (matches tippecanoe's `-y/-x/-X` flags):

**Include only specific fields** (whitelist):
```bash
gpq-tiles input.parquet output.pmtiles \
  --include name \
  --include population \
  --include area_sqkm
```

**Exclude specific fields** (blacklist):
```bash
gpq-tiles input.parquet output.pmtiles \
  --exclude internal_id \
  --exclude temp_field \
  --exclude debug_info
```

**Exclude all properties** (geometry only):
```bash
gpq-tiles input.parquet output.pmtiles --exclude-all
```

**Why filter properties?**
- **Reduce tile size** — Remove unnecessary attributes
- **Optimize performance** — Less data to encode/decode
- **Privacy** — Strip sensitive fields before publishing

## Large Files

For files larger than available memory, use streaming mode:

```bash
# Fast mode (default): Single pass, processes row groups sequentially
gpq-tiles large.parquet output.pmtiles --streaming-mode fast

# Low-memory mode: External sort, bounded memory (slower)
gpq-tiles large.parquet output.pmtiles --streaming-mode low-memory
```

**Mode comparison:**

| Mode | Memory | Speed | Notes |
|------|--------|-------|-------|
| `fast` | Row-group based | **Fastest** | Memory bounded by largest row group (~100-200MB typical) |
| `low-memory` | External sort | Slower | Sorts to disk, guaranteed low memory |

Row-group streaming works best with properly formatted files. Use `gpio check all --fix` for optimal performance.
