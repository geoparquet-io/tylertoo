# Rust reference

`tylertoo-core` is the engine behind tylertoo. The CLI (`tylertoo`) and the
Python package are thin wrappers over it, so embedding the crate gives you the
same overview-construction and PMTiles-export pipeline with no subprocess or
serialization boundary.

## API documentation

The canonical, always-current API reference is the crate's rustdoc — generated
from the doc-comments in the source, so it never drifts. Rather than re-render
rustdoc into this site, this page links to it.

- **Published docs:** [docs.rs/tylertoo-core](https://docs.rs/tylertoo-core) —
  live once the crate is published to crates.io. Until the first release, use
  the local build below.
- **Local docs (works today):**

  ```bash
  cargo doc -p tylertoo-core --no-deps --open
  ```

## Key entry points

The production path is a two-step chain — build an overview GeoParquet file,
then export it to a PMTiles archive — mirroring the CLI and Python surfaces.

| Symbol | Path | Purpose |
| --- | --- | --- |
| `convert_to_overviews` | `overview::convert` | Build a multi-resolution overview file from one input. |
| `convert_to_overviews_sources` | `overview::convert` | Same, over an ordered set of inputs (multi-partition / `--files-from`). |
| `ConvertOptions` | `overview::convert` | Every overview knob (ranking, thinning, density budget, simplification, coalescing). |
| `ConvertReport` | `overview::convert` | Per-level feature/vertex/byte counts and remote-fetch stats. |
| `export_pmtiles` | `overview::export` | Export an overview file to a PMTiles archive. |
| `ExportOptions` | `overview::export` | Layer name, tile buffer/extent, per-tile size cap, wave scheduling. |
| `ExportReport` | `overview::export` | Per-zoom tile counts and oversized-tile tallies. |
| `StreamingPmtilesWriter` | crate root | Lower-level streaming PMTiles v3 writer, if you drive tiling yourself. |
| `validate_wgs84`, `extract_crs`, `CrsInfo` | `quality` | CRS checks used by the input contract. |

For the shape of `ConvertOptions` / `ExportOptions` and their defaults, see the
rustdoc above; each field carries a doc-comment. The tuning semantics behind the
knobs are covered in [Tuning at each zoom](../diving-deeper/tuning-zoom.md).
