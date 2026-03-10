# Real-World Test Fixtures

Production data samples for testing gpq-tiles tiling performance.

## Fixtures

| File | Features | Size | Source | Use Case |
|------|----------|------|--------|----------|
| `open-buildings.parquet` | 1,000 | 143KB | Google Open Buildings | Quick tests, golden comparisons |
| `fieldmaps-madagascar-adm4.parquet` | 17,465 | 28MB | [FieldMaps](https://fieldmaps.io) | **Parallelization benchmarks** |
| `fieldmaps-boundaries.parquet` | 3 | 2.2MB | FieldMaps | Large polygon tests |
| `road-detections.parquet` | ~1,000 | 90KB | Road detection ML | LineString tests |

## Attribution

- **FieldMaps data** courtesy of Maxym Malynowsky ([fieldmaps.io](https://fieldmaps.io)) — edge-matched humanitarian admin boundaries
- **Google Open Buildings** — CC BY 4.0
- **Road detections** — derived from ML model outputs

## Git LFS

Most fixtures are tracked with Git LFS. After cloning:

```bash
git lfs pull
```

## Large Benchmark Files (Manual Download)

Some benchmark files are too large to track in the repository:

### `adm2_polygons.parquet` (1.8 GB, ~472k features)

Used for large polygon regression benchmarks. This file is **not tracked** — download manually if needed.

To run the regression benchmark:

```bash
# Place adm2_polygons.parquet in this directory, then:
cargo test --release -p gpq-tiles-core --test large_polygon_regression -- --nocapture
```

The test automatically skips if the file is not present.
