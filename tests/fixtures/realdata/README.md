# Real-World Test Fixtures

Production data samples for testing tylertoo tiling performance.

## Fixtures

| File | Features | Size | Source | Use Case |
|------|----------|------|--------|----------|
| `open-buildings.parquet` | 1,000 | 143KB | VIDA Google-Microsoft-OSM Open Buildings (Andorra) | Quick tests, golden comparisons |
| `fieldmaps-madagascar-adm4.parquet` | 17,465 | 28MB | [FieldMaps](https://fieldmaps.io) | **Parallelization benchmarks** |
| `fieldmaps-boundaries.parquet` | 3 | 2.2MB | FieldMaps | Large polygon tests |
| `road-detections.parquet` | ~1,000 | 90KB | Road detection ML | LineString tests |
| `ftw-moldova-z10.parquet` | 10,931 | 1.9MB | [Fields of The World](https://source.coop/ftw/global-data) | Coverage-fill work ratchets (#750) |

## Attribution

- **FieldMaps data** courtesy of Maxym Malynowsky ([fieldmaps.io](https://fieldmaps.io)) — edge-matched humanitarian admin boundaries
- **Open buildings**: Andorra subset of [VIDA's Google-Microsoft-OSM Open Buildings](https://source.coop/vida/google-microsoft-osm-open-buildings), ODbL 1.0 (Microsoft and OpenStreetMap footprints)
- **Road detections**: derived from ML model outputs
- **FTW Moldova**: field boundaries from [Fields of The World](https://source.coop/ftw/global-data) (Taylor Geospatial Institute and Microsoft AI for Good Research Lab), [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). The fields of z10 tile 10/593/357 of the `bench-coalesce-v1` Moldova input, cut with `uvx --from geoparquet-io==1.7.0 gpio extract --bbox 28.476562,47.517201,28.828125,47.754098`, then reduced to the geometry and bbox columns.

The repository-root `NOTICE` file lists the verified licenses.

## Getting Fixtures

### CI Environment

Fixtures are automatically downloaded from the `fixtures-v1` release during CI runs.

### Local Development

Download fixtures from the release:

```bash
gh release download fixtures-v1 --dir tests/fixtures/realdata/ --clobber
```

Or manually from: https://github.com/geoparquet-io/tylertoo/releases/tag/fixtures-v1

## Large Benchmark Files (Manual Download)

Some benchmark files are too large to track in the repository:

### `adm2_polygons.parquet` (1.8 GB, ~472k features)

Used for large polygon regression benchmarks. This file is **not tracked** — download manually if needed.

To run the regression benchmark:

```bash
# Place adm2_polygons.parquet in this directory, then:
cargo test --release -p tylertoo-core --test large_polygon_regression -- --nocapture
```

The test automatically skips if the file is not present.
