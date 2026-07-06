# gpq-tiles

Multi-resolution **overviews for GeoParquet**, plus PMTiles export — in Rust.

## What it does

- **`gpq-tiles overview`** — embed COG-style multi-resolution levels in a
  GeoParquet file (thinning, ranking, density budget, clustering, line
  coalescing, world-space simplification). The file stays valid GeoParquet:
  exact, SQL-queryable, single-artifact.
- **`gpq-tiles export-pmtiles`** — export a PMTiles vector-tile archive from
  an overview file.
- **`gpq-tiles tiles`** (or the bare form) — one-shot GeoParquet → PMTiles,
  a thin facade over the two steps above.
- **`gpq-tiles validate`** — check an overview file against the spec.
- **`gpq-tiles decode`** — decode any PMTiles vector-tile archive back to
  GeoParquet (the tiled representation; see [Decoding PMTiles](decode.md)).

## Quick Example

```bash
# One-shot: GeoParquet in, PMTiles out
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 14

# Or keep the intermediate overview file (the interesting artifact)
gpq-tiles overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14
gpq-tiles export-pmtiles overviews.parquet output.pmtiles
gpq-tiles validate overviews.parquet
```

```python
# Python
from gpq_tiles import overview, export_pmtiles

overview("input.parquet", "overviews.parquet", min_zoom=0, max_zoom=14)
export_pmtiles("overviews.parquet", "output.pmtiles")
```

## Next Steps

- [Getting Started](getting-started.md) — Installation and basic usage
- [Decoding PMTiles](decode.md) — PMTiles → GeoParquet, limitations included
- [Overview Tuning](OVERVIEW_TUNING.md) — Every generalization knob explained
- [API Reference](api-reference.md) — CLI flags, Python API, Rust API
- [Advanced Usage](advanced-usage.md) — Input optimization, memory, export
- [Remote Reads](remote-reads.md) — Converting directly from s3://, https://, gs:// inputs, and querying overview files in place with DuckDB
- [Architecture](architecture.md) — Design decisions and internals

## License

Apache 2.0 — [View on GitHub](https://github.com/geoparquet-io/gpq-tiles)
