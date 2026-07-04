# SPIKE: PMTiles → GeoParquet decoding (issue #112)

Status: spike complete, 2026-07-04. Branch `spike/112-pmtiles-decode`,
working code in `crates/core/tests/spike_decode.rs` (run with
`cargo test --package gpq-tiles-core --test spike_decode -- --nocapture`).

## Verdict

Feasible, and **cheaper than the ticket estimated**: no new dependencies are
needed at all. The full round trip — fixture GeoParquet → `convert_to_overviews`
→ `export_pmtiles` → enumerate tiles → decode MVT → tippecanoe coordinate
transform → compare against source — runs and passes in ~0.1 s.

## 1. MVT decode path: manual (prost + existing decoders), NOT geozero

Chosen: decode with our prost-0.14-generated `gpq_tiles_core::vector_tile::Tile`
types plus the existing `mvt::zigzag_decode` / `mvt::command_decode`. The whole
geometry-command decoder is ~45 lines (see `decode_mvt_geometry` in the spike).

Why not geozero `with-mvt`:

- geozero 0.14's `with-mvt` feature pins **prost ^0.11.9 and prost-build
  ^0.11.9** (verified on docs.rs). We're on prost 0.14, so enabling it means
  two prost stacks and two prost-build runs in every build, plus a second,
  incompatible set of generated `Tile`/`Layer`/`Feature` types shadowing ours.
- geozero has no `MvtReader`; the read side is `process_geom` driving a
  `GeomProcessor`, which yields f64 tile-local coords. We want the raw
  **integer** tile-local coords so the tippecanoe integer world-coordinate
  transform is exact.
- Property decode (tags → keys/values) has to be done against the tile struct
  either way; geozero saves nothing there.

The manual decoder also round-trips against our own encoder by construction
(same command/zigzag helpers, already unit-tested in `mvt.rs`).

## 2. PMTiles reading: no `pmtiles` crate needed (the ticket was wrong)

The ticket says we already depend on `pmtiles = "0.12"`. **We don't** — no
crate in the workspace depends on it (the golden-fixture tests shell out to the
`pmtiles` CLI binary, which is likely where the confusion came from). Good
news: we don't need it either.

`pmtiles_writer.rs` already exports everything the read side needs:

- `decode_varint`, `decode_directory`, `DirEntry` — directory parsing.
- `tile_id(z, x, y)` — we only had the forward Hilbert mapping; the spike adds
  the standard inverse (`tile_id_to_zxy` / `hilbert_d2xy`, ~30 lines) and
  asserts `tile_id(zxy(id)) == id` for every entry.
- Header: fixed 127-byte layout, trivially parsed with `u64::from_le_bytes`
  (the spike parses only the fields it needs; the real PR should add
  `Header::from_bytes` next to the existing `Header::to_bytes`).

Everything is synchronous `&[u8]` slicing — **no async runtime**, unlike the
`AsyncPmTilesReader` route the ticket sketched.

### Gotchas found

- **Compression**: `export_pmtiles` hardcodes `Compression::Gzip` for both
  internal (directories/metadata) and tile data. Directories AND tiles must
  each be gunzipped before parsing (flate2 is already a core dependency). The
  real decoder should honor header bytes 97/98 and support None/Gzip/Brotli/
  Zstd — all three codecs are already core deps for the write side, but there
  is **no public `compression::decompress`** yet; one must be added (mirror of
  `compress`, ~30 lines).
- **Leaf directories**: root entries with `run_length == 0` point into the
  leaf-directory section (`offset` relative to `leaf_dirs_offset`), and each
  leaf is independently compressed. Handled in the spike; kicks in above
  ~15k tiles per archive.
- **Run-length entries**: `run_length > 1` means consecutive tile IDs share
  one blob; the decoder must expand runs (spike does).
- **Integer transform validity**: `wscale / extent` in tippecanoe's formula is
  exact only while the extent divides `2^(32-z)` — i.e. z ≤ 20 for extent
  4096. Above that (or for non-power-of-two extents) switch to f64 world
  coordinates. Assert-guarded in the spike.
- **Duplicates are real**: with the default 8 px tile buffer, features near
  tile seams decode once per neighboring tile, and every feature appears at
  each zoom of its band. Matches tippecanoe-decode semantics (ticket's "no
  deduplication" stance); `--zoom` filtering is the practical mitigation.

## 3. Coordinate transform accuracy: PASS

Implemented tippecanoe's `write_json.cpp` transform verbatim
(`tile_px_to_lonlat` in the spike):

```text
wscale = 1 << (32 - z)
wx = wscale * tile_x + (wscale / extent) * px
lon = wx / 2^32 * 360 - 180
lat = atan(sinh(pi - 2*pi * wy / 2^32))
```

This is the exact inverse of our encoder (`geo_to_tile_coords` interpolates
linearly in longitude and in Mercator-Y fraction — i.e. true Web Mercator).

Round-trip result at z14 / extent 4096 (points in Philadelphia, Paris, Sydney,
plus a small linestring and polygon):

- Quantization tolerance: 360 / (2^14 · 4096) = **5.36e-6 deg (~0.60 m)**
- Worst observed vertex error: **2.55e-6 deg (~0.28 m)** — under half the
  tolerance, i.e. pure quantization noise, no systematic bias.
- The `id` Int64 property round-trips through MVT tags.

## Revised estimate for the real PR

The ticket guessed 300–500 LOC / ~2 weeks. Spike says **~450–600 LOC, 3–5
days**, with the LOC shifted away from decode plumbing (all proven above)
toward the Arrow/GeoParquet write side, which the ticket underestimated:

| Component | LOC | Notes |
|-----------|-----|-------|
| `Header::from_bytes` + `compression::decompress` | ~70 | mirrors existing write-side code |
| Sync archive reader (dirs, leaves, runs, id→zxy) | ~120 | spike code, productionized |
| MVT geometry decode → `geo::Geometry` | ~100 | spike decodes vertices; real PR must assemble Multi\*/rings (winding + ClosePath) |
| Tippecanoe transform | ~30 | spike code as-is (+ f64 path for z > 20) |
| Property extraction: MVT `Value` → Arrow builders | ~120 | schema union across tiles/layers is the real work (per-layer schemas can differ) |
| GeoParquet writer glue + zoom/layer filters | ~80 | reuse existing writer stack |
| CLI `decode` subcommand | ~50 | thin facade per house style |

Plan:

1. `fix(core)`: add `Header::from_bytes`, `compression::decompress`,
   `tile_id_to_zxy` in `pmtiles_writer.rs` — each unit-tested against its
   existing forward counterpart (pure TDD, no new deps).
2. `feat(core)`: `decode` module — sync `PmtilesArchive` reader +
   `decode_tile(bytes) -> Vec<(geo::Geometry, properties)>` + transform;
   golden-test against `tippecanoe-decode` output on
   `tests/fixtures/golden/*.pmtiles`.
3. `feat(core)`: GeoParquet assembly (schema union, zoom/layer filters,
   `tile_z`/`tile_x`/`tile_y` provenance columns worth considering).
4. `feat(cli)`: `gpq-tiles decode in.pmtiles out.parquet [--zoom N]
   [--min-zoom/--max-zoom] [--layer NAME]`.

Out of scope, per ticket: dedup, unclipping, `--projection` other than WGS84
(defer; EPSG:3857 output is a trivial variant of the transform if wanted).
