# API Reference

The authoritative flag reference is `gpq-tiles <subcommand> --help`; the
tables below summarize it. Knob semantics (directions, interactions,
why the defaults are what they are) live in
[Overview Tuning](OVERVIEW_TUNING.md).

## CLI

```bash
gpq-tiles <COMMAND>

Commands:
  tiles           GeoParquet → PMTiles one-shot (facade; also the bare form)
  overview        Build a multi-resolution overview GeoParquet file
  validate        Validate an overview file against the spec (§6.2)
  export-pmtiles  Export a PMTiles archive from an overview file
```

### `gpq-tiles overview <INPUT> <OUTPUT>`

| Flag | Default | Description |
|------|---------|-------------|
| `--mode <MODE>` | `duplicating` | `duplicating` (self-contained levels) or `partitioning` (each feature once) |
| `--min-zoom <N>` / `--max-zoom <N>` | `0` / `6` | Coarsest / finest (canonical) Web Mercator zoom for the level plan |
| `--gsd <G1,G2,…>` | — | Explicit GSD list (meters, strictly decreasing); overrides the zoom range |
| `--gsd-base <F>` | `1024.0` | Master detail knob: `gsd(z) = 40075016.69 / base / 2^z` |
| `--sort-key <COL>` | — | Numeric cell-winner priority column (mutually exclusive with `--class-rank`) |
| `--class-rank <SPEC>` | — | Categorical ranking, e.g. `road_class:motorway=5,primary=4` |
| `--no-auto-rank` | off | Disable auto-detection of Overture roads/places schemas |
| `--simplify-factor <F>` | `1.0` | RDP tolerance = factor × GSD (duplicating mode) |
| `--collapse` | off | Collapse below-visibility polygons to a representative point |
| `--cluster` | off | Point clustering: winners absorb cell losers into a `point_count` column |
| `--accumulate-attribute <COL:OP>` | — | Aggregate a numeric column across clusters (`sum`/`max`/`min`/`mean`; repeatable; requires `--cluster`) |
| `--no-coalesce-lines` | off (coalescing ON) | Disable line network coalescing |
| `--coalesce-junction-angle <DEG>` | `0.0` (off) | Continue chains through junctions within this angle of straight |
| `--coalesce-snap <F>` | `1.0` | Endpoint snap tolerance in GSD multiples |
| `--coalesce-max-level-rows <N>` | `2000000` | Per-level candidate-line memory guard |
| `--point-thinning <F>` | `4.0` (`16.0` with `--cluster`) | Point grid cell size = factor × GSD |
| `--line-thinning <F>` / `--polygon-thinning <F>` | `1.0` / `1.0` | Line/polygon grid cell size = factor × GSD |
| `--line-visibility <F>` / `--polygon-visibility <F>` | `2.0` / `4.0` | Min bbox diagonal in GSD multiples (hard drop below) |
| `--drop-rate <F>` | `1.65` | Density budget: each coarser level keeps 1/rate of the next finer budget |
| `--drop-gamma <F>` | `1.5` | Spatial fairness of the budget cut (sparse-area protection) |
| `--no-density-drop` | off | Disable the density budget entirely |
| `--cogp-compat` | off | Emit the COGP compatibility footer key (partitioning mode) |
| `--row-group-size <N>` | `10000` | Per-level row-group cap |
| `--full-column-stats` | off | Keep Parquet stats on all columns (default suppresses geometry/string stats) |
| `--no-streaming` | off (streaming ON) | Revert to the in-memory reference pipeline |
| `--read-batch-size <N>` | `8192` | Rows per Arrow read batch (streaming) |
| `--report <PATH>` | — | Write the JSON conversion report |

### `gpq-tiles export-pmtiles <INPUT> <OUTPUT>`

| Flag | Default | Description |
|------|---------|-------------|
| `--layer-name <NAME>` | `overview` | MVT layer name written into every tile |
| `--tile-buffer <PX>` | `8` | Per-tile edge buffer in tile pixels (seam continuity) |
| `--tile-size-limit <BYTES>` | — | Optional per-tile MVT cap (single non-iterative drop pass) |
| `--report <PATH>` | — | Write the JSON export report |

Tiles are gzip-compressed (the PMTiles-viewer-safe default; there is no
compression knob).

### `gpq-tiles validate <FILE>`

Checks an overview file against spec §6.2 (footer schema, level banding,
canonical fidelity, cluster/coalescing column invariants, bbox covering).
Exit code 0 on pass.

### `gpq-tiles tiles <INPUT> <OUTPUT>` (and the bare form)

One-shot facade: overview convert (default knobs) → temporary GeoParquet →
export-pmtiles.

| Flag | Default | Description |
|------|---------|-------------|
| `--min-zoom <N>` / `--max-zoom <N>` | `0` / `14` | Zoom range (feeds the overview level plan) |
| `--layer-name <NAME>` | derived from input filename | MVT layer name |
| `--max-tile-size <SIZE>` | — | Per-tile byte cap (e.g. `500K`, `1M`) |
| `-v, --verbose` | off | Per-level / per-zoom breakdowns |

For any other tuning, use `overview` + `export-pmtiles` directly.

---

## Python API

Type stubs ship with the wheel (`gpq_tiles.pyi`, verified against the
built module by `mypy.stubtest` in CI).

### `overview()`

```python
from gpq_tiles import overview

report = overview(
    input: str,
    output: str,
    *,
    mode: str = "duplicating",
    min_zoom: int = 0,
    max_zoom: int = 6,
    gsds: list[float] | None = None,
    gsd_base: float = 1024.0,
    sort_key: str | None = None,
    sort_direction: str = "desc",
    class_rank_column: str | None = None,
    class_ranks: dict[str, float] | None = None,
    class_rank_unknown: float | None = None,
    no_auto_rank: bool = False,
    simplify_factor: float = 1.0,
    collapse: bool = False,
    point_thinning: float | None = None,
    line_thinning: float = 1.0,
    polygon_thinning: float = 1.0,
    line_visibility: float = 2.0,
    polygon_visibility: float = 4.0,
    drop_rate: float = 1.65,
    drop_gamma: float = 1.5,
    density_drop: bool = True,
    cluster: bool = False,
    accumulate_attributes: dict[str, str] | None = None,
    coalesce_lines: bool = True,
    coalesce_snap: float = 1.0,
    coalesce_junction_angle: float = 0.0,
    coalesce_max_level_rows: int = 2_000_000,
    cogp_compat: bool = False,
    row_group_size: int = 10_000,
    full_column_stats: bool = False,
    streaming: bool = True,
    read_batch_size: int = 8192,
) -> dict  # the JSON conversion report
```

Parameters mirror the CLI flags above one-to-one (booleans instead of
`--no-*` switches: `density_drop=False` ≙ `--no-density-drop`,
`coalesce_lines=False` ≙ `--no-coalesce-lines`, `streaming=False` ≙
`--no-streaming`).

### `export_pmtiles()`

```python
from gpq_tiles import export_pmtiles

report = export_pmtiles(
    input: str,
    output: str,
    *,
    layer_name: str = "overview",
    tile_buffer: int = 8,
    extent: int = 4096,
    tile_size_limit: int | None = None,
) -> dict  # the JSON export report
```

### `validate()`

```python
from gpq_tiles import validate

result = validate(file: str) -> dict  # per-check pass/fail report
```

### `convert()` (deprecated facade)

```python
from gpq_tiles import convert

convert(
    input: str,
    output: str,
    min_zoom: int = 0,
    max_zoom: int = 14,
    layer_name: str | None = None,
    tile_size_limit: int | None = None,
) -> None
```

Runs the same overview-convert → export chain as the CLI `tiles`
facade. Prefer `overview()` + `export_pmtiles()` for anything beyond
the defaults.

**Raises:** `ValueError` for invalid parameter values, `RuntimeError`
for conversion failures (missing file, invalid GeoParquet, wrong CRS).

---

## Rust API (`gpq-tiles-core`)

The production entry points are in the `overview` module:

```rust
use gpq_tiles_core::overview::convert::{convert_to_overviews, ConvertOptions, ConvertReport};
use gpq_tiles_core::overview::export::{export_pmtiles, ExportOptions, ExportReport};
use gpq_tiles_core::overview::check::validate_file;
use std::path::Path;

// GeoParquet → overview GeoParquet
let opts = ConvertOptions::default();       // duplicating, z0..6, all defaults
let report: ConvertReport =
    convert_to_overviews("input.parquet", "overviews.parquet", &opts)?;

// Overview GeoParquet → PMTiles
let eopts = ExportOptions::default();       // layer "overview", buffer 8, gzip
let ereport: ExportReport =
    export_pmtiles("overviews.parquet", "output.pmtiles", &eopts)?;

// Spec §6.2 validation
let validation = validate_file("overviews.parquet")?;
```

`ConvertOptions` / `ExportOptions` fields correspond to the CLI flags;
see the rustdoc (`cargo doc --package gpq-tiles-core --open`) for the
full structs, report types, and error enums (`ConvertError`,
`ExportError`, `CheckError`).

### Shared types

```rust
pub struct TileCoord { pub x: u32, pub y: u32, pub z: u8 }  // Web Mercator XYZ
pub struct TileBounds {                                      // WGS84 degrees
    pub lng_min: f64, pub lat_min: f64,
    pub lng_max: f64, pub lat_max: f64,
}
```
