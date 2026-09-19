# tylertoo

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

Fast GeoParquet → PMTiles converter in Rust.

📖 **[Documentation](https://geoparquet-io.github.io/tylertoo/)** ·
[Getting Started](https://geoparquet-io.github.io/tylertoo/getting-started/) ·
[Live demo](https://geoparquet-io.github.io/tylertoo/demo/) ·
[Overview tuning](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/) ·
[CLI reference](https://geoparquet-io.github.io/tylertoo/reference/cli/)

## Quickstart

Three commands and real data — 17,465 Madagascar admin-4 boundary polygons
(28 MB) from this repo's fixture release. The download dominates; the
conversion itself takes about a second.

```bash
cargo install tylertoo
curl -LO https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/fieldmaps-madagascar-adm4.parquet
tylertoo fieldmaps-madagascar-adm4.parquet madagascar.pmtiles --max-zoom 10
```

Real output, with log timestamps stripped and the routine per-level lines
elided:

```text
[convert] scan complete: 17465 feature(s) from 17465 row(s)
[convert] pass 2: building 11 overview level(s) from a single read (finest level streamed last)
[rss] convert peak: 231 MiB
  intermediate overview: /var/.../T/.tylertoo-overview-p7TdMr.parquet (25.87 MiB, removed after export; --keep-overview PATH retains it)
[export] scan complete: 10 levels, single read, 0.15s
[export] level 10/10 z10 done: 17465 feats, 524 tiles, 1 partitions, 0.3s (total 1s)
✓ Converted fieldmaps-madagascar-adm4.parquet → madagascar.pmtiles
  752 tiles across z0..z10 in 1.18s
  z0 declared but empty: every feature generalized away there (see --collapse / --collapse-square), or an entry-zoom ladder holds every feature out of them
```

That leaves a 6.7 MB `madagascar.pmtiles`. Drop it onto
[pmtiles.io](https://pmtiles.io/) to view it — the MVT source-layer is
`overview`. (z0 is one tile at 39 km/pixel, where every polygon simplifies to
nothing; `--collapse` keeps them as dots instead.)

From here,
[Getting Started](https://geoparquet-io.github.io/tylertoo/getting-started/)
covers preparing your own input and the two-step overview workflow.

> **Status: 0.7, actively developed toward 1.0.** The CLI surface and the
> `geo:overviews` format still move between minor versions; the
> [Road to 1.0](https://github.com/geoparquet-io/tylertoo/issues/463) issue
> tracks what stabilizing them requires. Output is checked against readers
> other than our own: PMTiles archives are verified with `pmtiles verify`
> (go-pmtiles) in the test suite, `tylertoo decode` is golden-compared against
> `tippecanoe-decode`, the generalization ladder is calibrated on rendered
> sweeps against tippecanoe v2.49 golden tiles
> ([corpus/SWEEPS.md](corpus/SWEEPS.md)), and overview files stay ordinary
> GeoParquet that DuckDB and any Arrow reader can open. Published head-to-head
> benchmark numbers are still to come — see
> [#447](https://github.com/geoparquet-io/tylertoo/issues/447).

**tylertoo** takes its name from ["Tippecanoe and Tyler Too"](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too),
the 1840 U.S. campaign slogan. It's a nod to [tippecanoe](https://github.com/felt/tippecanoe),
the vector-tile tool this project measures itself against — tylertoo runs alongside it.

**Features:**
- COG-style multi-resolution **overviews embedded in GeoParquet** (`tylertoo overview`) — the file stays valid, exact, SQL-queryable GeoParquet
- PMTiles export from an overview file (`tylertoo export-pmtiles`)
- One-shot GeoParquet → PMTiles (`tylertoo tiles`, or the bare form)
- Quality ladder tuned against tippecanoe: class ranking (Overture auto-detect), visibility gates, density budget, point clustering, line coalescing
- Memory-bounded streaming conversion — a 632k-polygon / 38M-vertex file converts to a full z0–14 overview pyramid in ~45 s at ~1.4 GB peak RSS, or a default z0–6 pyramid in ~7 s at ~0.4 GB (16-core machine; measured, see [ARCHITECTURE.md](context/ARCHITECTURE.md))
- Remote inputs (`s3://`, `https://`, `gs://`) read via byte-range requests — with `--bbox`, extract a city from a remote country-scale file while downloading only the matching row groups ([Remote Reads](docs/diving-deeper/remote-and-multi-file.md))
- Attribute filtering (`--filter` / `--where`) — tile only the features matching a SQL-WHERE-style predicate (`"confidence > 0.8"`, `"crop IN ('soy', 'corn')"`), with parquet row-group statistics pushdown so non-matching row groups are never read (or fetched, on remote input); composes with `--bbox` ([Tuning guide](docs/OVERVIEW_TUNING.md#attribute-filter---filter----where))
- Spec validation (`tylertoo validate`)
- PMTiles → GeoParquet decoding (`tylertoo decode`) — tippecanoe-decode
  semantics, any PMTiles v3 MVT archive

## Install

```bash
cargo install tylertoo    # CLI
pip install tylertoo      # Python
```

Prebuilt CLI binaries for Linux (x86_64 gnu and musl), macOS (Intel and Apple
Silicon) and Windows x86_64 are attached to every
[GitHub Release](https://github.com/geoparquet-io/tylertoo/releases).

## Usage

```bash
# One-shot: GeoParquet in, PMTiles out (recommended)
tylertoo input.parquet output.pmtiles --min-zoom 0 --max-zoom 14

# Keep the reusable multi-resolution overview file too — one run, both artifacts
tylertoo input.parquet output.pmtiles --max-zoom 14 \
  --keep-overview overviews.parquet
```

The one-shot form materializes an intermediate overview GeoParquet before
exporting (at least input-sized — it is **not** zero-disk). Its path and
size are logged, `--spill-dir` / `$TMPDIR` control where it lives, and a
free-space preflight warns when the volume looks too small.

### The Two-Step Workflow

The overview GeoParquet file is the interesting artifact — build it
explicitly when you want to validate it, query it, or re-export with
different flags without re-converting:

```bash
# 1. Embed multi-resolution levels in a GeoParquet file
tylertoo overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14

# 2. Validate against the spec
tylertoo validate overviews.parquet

# 3. Export a PMTiles archive for map rendering
tylertoo export-pmtiles overviews.parquet output.pmtiles
```

To match what `tiles` writes, pass `export-pmtiles --min-zoom <the overview's
requested minimum>` — otherwise the archive header starts at the coarsest
level that actually holds features.

Every tuning knob is available on the one-shot `tiles` command as well as
on `overview` / `export-pmtiles` — see
[Overview Tuning](docs/OVERVIEW_TUNING.md). Defaults are calibrated on
rendered corpus sweeps and are meant to look right out of the box.

### Decoding PMTiles back to GeoParquet

```bash
# Extract one zoom of any PMTiles v3 vector archive as GeoParquet
tylertoo decode input.pmtiles output.parquet --zoom 14
```

The output is the **tiled representation** (simplified, clipped, duplicated
across tiles and zooms — no round-trip guarantee), with `zoom`/`layer`/
`mvt_id` provenance columns for filtering. See
[Decoding PMTiles](docs/decode.md).

### Input Preparation

Inputs must be WGS84 (EPSG:4326), and should be Hilbert-sorted with sane
row groups. Use [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io)
(`gpio`, verified against 1.5.0):

```bash
# Already WGS84: Hilbert-sort and repack row groups in one pass.
gpio sort hilbert input.parquet prepared.parquet --row-group-size-mb 128

# In another projection: reproject first, then sort.
gpio convert reproject input.parquet wgs84.parquet -d EPSG:4326
gpio sort hilbert wgs84.parquet prepared.parquet --row-group-size-mb 128
```

### Python

```python
from tylertoo import overview, export_pmtiles, validate

overview("input.parquet", "overviews.parquet", min_zoom=0, max_zoom=14)
validate("overviews.parquet")
export_pmtiles("overviews.parquet", "output.pmtiles")

# One-shot facade (deprecated in favor of the two-step API)
from tylertoo import convert
convert("input.parquet", "output.pmtiles", min_zoom=0, max_zoom=14)
```

## Documentation

- **[Getting Started](docs/getting-started.md)** — Installation, one-shot conversion, the two-step workflow
- **[Diving Deeper](docs/diving-deeper/index.md)** — Input prep, zoom tuning, remote/multi-file input, bounded memory
- **[Reference](docs/reference/index.md)** — Generated CLI, Python, and Rust API surface
- **[Overview Tuning](docs/OVERVIEW_TUNING.md)** — Every generalization knob explained
- **[Decoding PMTiles](docs/decode.md)** — PMTiles → GeoParquet, limitations included
- **[Format Spec (draft)](context/OVERVIEWS_SPEC.md)** — The `geo:overviews` format contract

## Development

```bash
git clone https://github.com/geoparquet-io/tylertoo.git && cd tylertoo
git config core.hooksPath .githooks
cargo build && cargo check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [DEVELOPMENT.md](DEVELOPMENT.md) for details.

## License

Apache-2.0
