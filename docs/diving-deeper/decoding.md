# Decoding PMTiles back to GeoParquet

`tylertoo decode` converts a PMTiles vector-tile archive into a GeoParquet
file, following the model of `tippecanoe-decode`. It works on any PMTiles v3
MVT archive (not just ones produced by tylertoo) and supports all four spec
compression codecs (none, gzip, brotli, zstd) for directories and tiles.

```bash
# Everything, all zooms (large: every feature, every tile, every zoom)
tylertoo decode input.pmtiles output.parquet

# Extract a single zoom (recommended for most use cases)
tylertoo decode input.pmtiles output.parquet --zoom 14

# Zoom range
tylertoo decode input.pmtiles output.parquet --min-zoom 10 --max-zoom 14

# One MVT layer only
tylertoo decode input.pmtiles output.parquet --layer buildings

# Machine-readable summary
tylertoo decode input.pmtiles output.parquet --zoom 14 --report report.json
```

Rust API: `tylertoo_core::decode::decode_pmtiles(input, output, &DecodeOptions)`.

## What you get — and what you don't

**The output is the tiled representation, not the original source data.**
Tiling is lossy by design; decoding recovers what the tiles contain, nothing
more:

| Limitation | Why | What to do |
|------------|-----|------------|
| **Simplified geometries** | Vertices are removed during tiling at lower zooms | Extract the maximum zoom (`--zoom <maxzoom>`) for the best available detail |
| **Clipped geometries** | Features are cut at (buffered) tile boundaries | Accept clipped output, or post-process a merge keyed on a stable property |
| **Duplicate features** | A feature near a tile seam appears once per neighboring tile (buffer copies), and once per zoom level | Filter to a single zoom (`--zoom`, or the output's `zoom` column); dedupe by a stable feature property if needed |
| **Lost properties** | Attributes dropped during tiling are not in the tiles | Nothing — they cannot be recovered |
| **No round-trip guarantee** | All of the above | `A.parquet → B.pmtiles → C.parquet` yields `C ≠ A`; this is inherent to vector tiles |

Matching `tippecanoe-decode`, **nothing is deduplicated**: every feature from
every selected tile is emitted as its own row.

## Output schema

Three provenance columns come first, so the duplicated representation stays
filterable after the fact:

| Column | Type | Meaning |
|--------|------|---------|
| `zoom` | UInt8 | Zoom level of the tile the row came from |
| `layer` | Utf8 | MVT layer name (always present, even for single-layer archives, so the schema is stable) |
| `mvt_id` | UInt64 (nullable) | The raw MVT feature id, when the encoder set one |

They are followed by the union of all property columns seen across every
tile and layer (alphabetical, all nullable — a feature that lacks a property
gets null), then `geometry` (WKB GeoParquet with a bbox covering, EPSG:4326).

Property types are unified across features: integers → Int64, floats/doubles
→ Float64, Int64 mixed with Float64 → Float64, and any other mixture (e.g. a
key that is a bool in one tile and a string in another) degrades to Utf8 with
values stringified. A `uint` above `i64::MAX` degrades to Float64. A source
property named `zoom`, `layer`, `mvt_id`, or `geometry` is rejected with an
error rather than silently clobbered.

Coordinates are recovered through tippecanoe's 32-bit world-coordinate
transform (`write_json.cpp`), accurate to the tile quantization limit — about
0.6 m at z14 with the standard 4096 extent, halving per zoom.

## Tips

- Decoded output is unsorted point-in-time data; before serving it anywhere,
  optimize it with [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):
  `gpio convert reproject decoded.parquet out.parquet -d EPSG:4326 --hilbert --row-group-size 100000`.
- Feature counts match `tippecanoe-decode` exactly for the same zoom
  selection; the integration suite pins this against the real binary when it
  is installed.
