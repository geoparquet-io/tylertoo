# tylertoo

Multi-resolution **overviews for GeoParquet**, plus PMTiles export — in Rust.

**tylertoo** takes its name from ["Tippecanoe and Tyler Too"](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too),
the 1840 U.S. campaign slogan. It's a nod to [tippecanoe](https://github.com/felt/tippecanoe),
the vector-tile tool this project measures itself against — tylertoo runs alongside it.

## What it does

- **`tylertoo overview`** — embed COG-style multi-resolution levels in a
  GeoParquet file (thinning, ranking, density budget, clustering, line
  coalescing, world-space simplification). The file stays valid GeoParquet:
  exact, SQL-queryable, single-artifact.
- **`tylertoo export-pmtiles`** — export a PMTiles vector-tile archive from
  an overview file.
- **`tylertoo tiles`** (or the bare form) — one-shot GeoParquet → PMTiles,
  a thin facade over the two steps above.
- **`tylertoo validate`** — check an overview file against the spec.
- **`tylertoo decode`** — decode any PMTiles vector-tile archive back to
  GeoParquet (the tiled representation; see [Decoding PMTiles](decode.md)).

## Quick Example

```bash
# One-shot: GeoParquet in, PMTiles out
tylertoo input.parquet output.pmtiles --min-zoom 0 --max-zoom 14

# Or keep the intermediate overview file (the interesting artifact)
tylertoo overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14
tylertoo export-pmtiles overviews.parquet output.pmtiles
tylertoo validate overviews.parquet
```

```python
# Python
from tylertoo import overview, export_pmtiles

overview("input.parquet", "overviews.parquet", min_zoom=0, max_zoom=14)
export_pmtiles("overviews.parquet", "output.pmtiles")
```

## Next Steps

- [Getting Started](getting-started.md) — Installation and basic usage
- [Diving Deeper](diving-deeper/index.md) — Input prep, zoom tuning, remote/multi-file input, bounded memory
- [Reference](reference/index.md) — Generated CLI, Python, and Rust API surface
- [Decoding PMTiles](decode.md) — PMTiles → GeoParquet, limitations included
- [Overview Tuning](OVERVIEW_TUNING.md) — Every generalization knob explained
- [Architecture](architecture.md) — Design decisions and internals

## License

Apache 2.0 — [View on GitHub](https://github.com/geoparquet-io/tylertoo)
