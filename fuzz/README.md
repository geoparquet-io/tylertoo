# tylertoo fuzz targets

`cargo-fuzz` targets for the untrusted-bytes surfaces of `tylertoo-core`
(issues #187, #424, #633). This crate is a **standalone workspace** — it is
not a member of the root workspace and never affects `cargo build/check/test
--workspace` or per-PR CI time. [Coverage](#coverage) lists every place
tylertoo reads outside data and the target that covers it.

## Targets

| Target | Surface | Seeds |
|--------|---------|-------|
| `pmtiles_header` | `pmtiles_writer::Header::from_bytes` — the 127-byte header of any archive handed to `decode`, `merge` or `--band`; a header that parses must re-parse after `to_bytes` | the golden fixtures' headers |
| `pmtiles_directory` | `pmtiles_writer::decode_directory` — root and leaf directories (#397's class of bug); a directory that decodes must round-trip through `encode_directory` | the golden fixtures' root directories, decompressed |
| `mvt_decode` | `decode::fuzz_decode_tile` → `decode_tile_features` — one uncompressed MVT tile body; the first input byte picks the zoom (`z & 31`) | three tiles per golden fixture, decompressed, zoom byte prepended |
| `geoparquet_wkb` | `batch_processor::fuzz_geoparquet_wkb` → `extract_geometries_opt_from_array` over a one-row `geoarrow.wkb` array — each value of a GeoParquet WKB geometry column, decoded by the `wkb` crate behind `wkb_column::check_wkb_value` (#632); panics if the check and the reader disagree about a value | #632's two crash inputs, a MultiPoint whose last member sets the SRID flag, and one valid value per OGC type |
| `band_archive` | `pyramid::fuzz_band_archive` → `BandArchive::open` + `for_each_tile` — a whole archive as `--band` input, through the directory-walk budgets and the decompression caps (#417) | `open-buildings.pmtiles` (31 KB) |
| `filter_expr` | `overview::filter::parse_filter` — the `--filter` grammar (recursive descent; deep nesting must be an error, not a stack overflow) | a handful of expressions |
| `footer_json` | `overview::level::OverviewsMeta::from_json` — the `overviews` footer JSON read verbatim from any input file — then `validate` against a row-group count taken from the first input byte | a row-group count byte and the footer JSON of two overview files |
| `wkb` | `wkb::wkb_to_geometry` — the WKB coalesce lines stored in a saved `--plan` file. GeoParquet geometry columns are decoded by geoarrow and the `wkb` crate instead, which this target does not reach | one of each of the seven OGC types, a ZM point, and #623's oversized-count crash |
| `geo_metadata` | `overview::convert::fuzz_geo_metadata` — the GeoParquet `geo` JSON through CRS detection (`detect_crs_from_kv` → `quality::crs_info_from_kv_metadata`, `classify_crs_info`, `classify_crs_identifier`/`_projjson`), `covering::parse_covering_metadata` and `convert::find_geometry_column` | the `geo` JSON of eight real files, and the CRS shapes the unit tests pin (Pseudo-Mercator PROJJSON, a name-only PROJJSON, `ESRI:102100`, UTM, `crs: null` with a bbox) |
| `plan_file` † | `overview::fuzz_convert_plan` → `ConvertPlan::load` + `into_winner_tables` — a saved `--plan` file. The input is the Arrow IPC payload; the hook writes a valid magic and checksum in front, so mutations reach the decoder instead of stopping at the checksum (#512) | two plans from `tiles --save-plan --plan-only`, the 16-byte header stripped |
| `decode_archive` | `decode::fuzz_decode_archive` — `decode_pmtiles` on an in-memory archive short of writing GeoParquet: header, directory walk and budgets, capped tile decompression, MVT decode, the reserved-column check and the property type union | `open-buildings.pmtiles` and a 29 KB archive tiled from `overture-names.parquet` |
| `archive_index` | `fuzz::fuzz_archive_index` → `ArchiveIndex::open`, metadata, every tile body, `stats::compute_stats` — an archive handed to `stats` or `merge`. Unlike `band_archive`, an archive that is not gzip MVT gets past the open | as `decode_archive` |
| `layer_zooms` | `pyramid::fuzz_layer_zooms` — an input archive's `vector_layers` as `pyramid` (`parse_layers`) and `merge` (`layer_zoom_range`, `declared_zoom_range`, `vector_layers_json`) read it; two leading bytes are the header's zoom range. Panics if a range comes back out of order or past z31 | the header zooms and metadata of the three golden archives |
| `parquet_footer` † | `overview::check::fuzz_parquet_footer` — every footer-only reader: `check::validate_metadata`, `geo:overviews` `validate`, the row-group bbox readers (covering columns, native geo statistics), the spill and batch sizing heuristics (`selected_compressed_bytes`, `widest`/`largest_byte_array_column`), and `--filter` pushdown on column statistics. The input is the thrift footer, without its length and `PAR1` | the footers of nine real and overview files |
| `overview_file` † | `overview::reader::fuzz_overview_file` — a whole overview file through `OverviewReader::open`, every level's band, lookups, pruning and a full read, and `check::validate_file` | two small overview files from `tylertoo overview` |
| `remote_footer` † | `input::fuzz_remote_footer` — a remote GeoParquet object through `RemoteSource` over an in-memory object store: the ranged footer fetch and its range checks (#430, #586), row-group staging, a full read. Needs `--features remote` | three small GeoParquet files, one of them an overview |
| `shard_plan` | `fuzz::fuzz_shard_plan` → `ShardPlan::load` and every range query `tiles --shard` makes of a plan | a 4-shard plan from `tylertoo shard-plan` |
| `decompress` | `fuzz::fuzz_decompress` → `compression::decompress_capped` — every directory, metadata block and tile body of an input archive; the first byte picks the codec. Panics if the output passes the 1 MiB cap | a small JSON body under each codec |
| `cli_args` | `fuzz::fuzz_cli_args` — the core parsers behind CLI and Python flags, and the public `covering::parse_bounds`: `parse_representation_spec`, `pyramid::Band::parse`, `TileRange::parse`, `ShardRole::parse`, `MaxZoom` and `FeatureOrder` `FromStr`, `input::url_scheme` | one valid value per parser, and #681's overflow input |

† Not in the nightly matrix yet: each finds an upstream panic within a
minute, and a target that crashes every night finds nothing else.
`overview_file` and `remote_footer` wait on #683 (arrow-ipc panics on a
crafted `ARROW:schema` footer key; fixed in arrow 60, which geoarrow 0.9
blocks), `remote_footer` also on #687 (parquet-rs panics decoding a crafted
page's definition levels), `plan_file` on #684 (arrow-ipc panics on a
crafted IPC buffer offset), `parquet_footer` on #690 (parquet-rs reserves
memory for a crafted schema's child count, about 12 GB from a 162-byte
footer, which libFuzzer's malloc limit reports as an OOM). Their seeds still
replay on every PR, and they run locally as usual. Re-add each to `.github/workflows/fuzz.yml` when its issue closes.

Most targets call a `#[doc(hidden)]` hook in core gated on the `fuzzing`
cargo feature (`crates/core/Cargo.toml`). Hooks next to a private parser
(`decode`, `pyramid`, `overview::plan_state`, ...) reach code that is private
on purpose; the ones in `crates/core/src/fuzz.rs` only call public API, and
live in core so `crates/core/tests/fuzz_seeds.rs` can replay the committed
seeds through exactly what the nightly run fuzzes. This crate and that test
are the only things that enable the feature; it is outside the public-API
baseline. CI's test tiers run with `--all-features`, so every PR compiles the
hooks and replays the seeds.

## Coverage

Every place tylertoo reads data it did not write, and the target that
covers it, or why none does. **A new reader without a row here is a review
comment.** "Outside data" is bytes, a path or JSON that came from a file,
archive, URL or plan; the CLI flag rows are lower stakes (the person running
the command typed them) and are covered because it is cheap.

| Outside input | Entry points | Target |
|---|---|---|
| GeoParquet WKB geometry values | `batch_processor` → geoarrow → `wkb` crate, behind `wkb_column::check_wkb_value` | `geoparquet_wkb` |
| GeoParquet `geo` JSON and CRS | `covering::get_geo_metadata` / `parse_covering_metadata` / `resolve_geometry_column_name`, `convert::detect_crs_from_kv` / `find_geometry_column`, `quality::crs_info_from_kv_metadata` / `classify_crs_projjson` / `classify_crs_identifier`, `properties::declared_covering` | `geo_metadata`; `parquet_footer` reaches the same readers from a footer |
| Parquet footer metadata (row-group stats, sizes, key-value metadata) | `check::validate_metadata`, `covering::extract_row_group_bounds_from_metadata` / `_tiered`, `input::selected_compressed_bytes`, `input_set::widest_byte_array_column` / `largest_byte_array_column`, `filter::BoundFilter::select_row_groups` | `parquet_footer` † (#690). The thrift decode itself is parquet-rs's, and #690 is in it |
| The embedded `ARROW:schema` key-value (every Parquet open) | parquet-rs `get_arrow_schema_from_metadata` → arrow-ipc | Upstream code, but a panic there aborts every command (#683). Reached by `overview_file` † and `remote_footer` † |
| Overview files as input (`export-pmtiles`, `validate`) | `overview::reader::OverviewReader::open` and its band reads, `check::validate_file` | `overview_file` † (#683); `footer_json` for the `geo:overviews` JSON alone, `parquet_footer` for the footer |
| GeoParquet data pages and property columns | parquet-rs / arrow-rs page decoding | Upstream code, but a panic there aborts tylertoo (#687). `remote_footer` † and `overview_file` † read every page of their input. Every tylertoo reader of what the pages decode to has its own row |
| Remote GeoParquet (`s3://`, `gs://`, `https://`) | `input::remote::RemoteSource` (`load_footer`, `get_bytes_range`, `stage_row_groups`) | `remote_footer` † (#683, #687) |
| Saved `--plan` files | `overview::plan_state::ConvertPlan::load`, `into_winner_tables` → `wkb::wkb_to_geometry` | `plan_file` † (#684); `wkb` for the coalesce WKB alone. `stream::load_plan_state`'s fingerprint checks compare against real input files and are not fuzzed |
| `--shard-plan` files | `shard::ShardPlan::load` | `shard_plan` |
| PMTiles header | `pmtiles_writer::Header::from_bytes` | `pmtiles_header` |
| PMTiles directories | `pmtiles_writer::decode_directory`, the directory walks (`read_all_entries_from`, `decode::collect_tile_refs`) | `pmtiles_directory`; the walks through `band_archive`, `decode_archive`, `archive_index` |
| Compressed archive sections and tile bodies | `compression::decompress_capped` | `decompress`; also inside every whole-archive target |
| MVT tile bodies (`decode`) | `decode::decode_tile_features` | `mvt_decode`; with decompression and the walk, `decode_archive` |
| Whole archives for `decode` | `decode::decode_pmtiles` | `decode_archive`. The GeoParquet writer it ends in only sees decoded values |
| Whole archives for `stats` and `merge` | `archive_index::ArchiveIndex::open`, `tiles`, `stats::compute_stats` | `archive_index` |
| `--band` archives for `pyramid` | `pyramid::BandArchive::open`, `for_each_tile` | `band_archive` |
| Archive metadata `vector_layers` (`pyramid`, `merge`) | `pyramid::parse_layers` / `layer_zoom_range` / `declared_zoom_range` / `vector_layers_json`, `merge::collect_layers` | `layer_zooms`. `merge`'s cross-archive agreement checks compare values already parsed by the readers above |
| `merge`'s output check | `pmtiles_writer::verify_clustered` | None: it reads the archive tylertoo just wrote. Its directory walk is the one `band_archive` covers |
| `--filter` | `overview::filter::parse_filter` | `filter_expr`; pushdown in `parquet_footer` |
| Core string flags (`--representation`, `--band`, `--tile-range`, `--shard`, `--max-zoom`, `--feature-order`, input URL schemes), and the public `covering::parse_bounds` (no caller in the CLI today) | see `cli_args` above | `cli_args` |
| The `--accumulate` operator name; the public `Compression::from_str` | `cluster::AccumulateOp::parse`, `compression::Compression::from_str` | None: a fixed keyword match with no other logic |
| CLI-only flag parsers (`parse_size_bytes`, `parse_bbox`, `parse_read_workers`, ...) | private functions in `crates/cli/src/main.rs` | None: the binary crate has no library target the fuzz crate can call, and the input is typed by the person running the command |
| Python binding strings (`parse_feature_order`, `accumulate_specs`) | `crates/python/src/lib.rs` | None: strings from the caller, handed to the core parsers above. No Python entry point takes bytes |
| `--files-from` manifests, globs, directory inputs | `input_set::ConvertSource::from_manifest` / `resolve`, `list_parquet_files` | None: each line or match becomes a path or URL that then goes through the readers above; the URL scheme sniff is in `cli_args` |
| Memory limits from the OS and environment | `overview::procmem`, cgroup and `/proc/meminfo` reads in `overview::pipeline`, `TYLERTOO_AUTO_MEM_LIMIT_BYTES` | None: written by the kernel or the operator, not by whoever made the input |
| Spill and temp files | export spill, remote disk spill, `pmtiles_writer` temp files | None: tylertoo writes them and reads them back in the same run |

## Seeds

Seed corpora live in `corpus/<target>/seed-*` and are committed (about
400 KB in all). `fuzz/.gitignore` un-ignores exactly the `seed-*` names, so
everything libFuzzer adds to `corpus/<target>/` during a run (sha1-named)
stays untracked. To promote a discovered input to a seed, copy it to a
`seed-<description>` name and commit it.

The PMTiles seeds are cut from `tests/fixtures/golden/*.pmtiles`: header =
first 127 bytes; directory = the root directory bytes, gzip-decompressed;
tile = a tile body, gzip-decompressed, with the archive's max zoom as the
leading byte; `layer_zooms` = the header's min and max zoom bytes, then the
gzip-decompressed metadata.

The GeoParquet seeds come from `tests/fixtures/{realdata,streaming,properties}`
and from tylertoo's own output on them: `geo_metadata` is a file's `geo`
key-value string; `parquet_footer` is the thrift footer (the `N` bytes before
the trailing 4-byte length `N` and `PAR1`); `remote_footer` and
`overview_file` are whole small files; `plan_file` is a
`tiles --save-plan --plan-only` file without its first 16 bytes; `shard_plan`
is `tylertoo shard-plan` output as written.

## Running locally

Requires a nightly toolchain and `cargo-fuzz`. CI pins the nightly to the
one the public-api gate uses (`NIGHTLY` in `.github/workflows/fuzz.yml`);
any recent nightly works locally. A prebuilt `cargo-fuzz` (for example
from `cargo binstall`) may be musl-linked and default to a musl target,
which AddressSanitizer rejects; pass `--target x86_64-unknown-linux-gnu`
to `cargo fuzz run` if so. A `cargo install` build needs no flag.

```bash
cargo install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run pmtiles_directory -- \
  -max_total_time=60
# remote_footer needs core's `remote` feature
cargo +nightly fuzz run --features remote remote_footer -- \
  -max_total_time=60
```

On aarch64 Linux, the `remote` build (AWS SDK plus AddressSanitizer) is
too big for GNU ld's branch range and fails to link with `relocation
truncated to fit: R_AARCH64_CALL26`. Link it with the toolchain's `rust-lld`
instead, in a separate target dir so the other targets' build is kept:

```bash
RUSTFLAGS="-Zunstable-options -Clink-self-contained=+linker -Clinker-flavor=gnu-lld-cc" \
  cargo +nightly fuzz build --features remote --target-dir target-lld remote_footer
```

To replay the committed seeds through the hooks on stable, without
libFuzzer:

```bash
cargo test -p tylertoo-core --features fuzzing,remote --test fuzz_seeds
```

Run several targets in parallel from separate shells rather than with
`-jobs`: `-jobs` forks a child that writes `fuzz-0.log` into the current
directory, and parallel runs then all report the same file.

## When a target crashes

Crashing inputs land in `fuzz/artifacts/<target>/` (locally) or in the
`fuzz-artifacts-<target>` job artifact (CI). Then:

```bash
cd fuzz
# Reproduce
cargo +nightly fuzz run <target> \
  artifacts/<target>/crash-<sha>
# Minimize
cargo +nightly fuzz tmin <target> \
  artifacts/<target>/crash-<sha>
# The minimized input lands next to the original as minimized-from-<sha>
```

Add the minimized bytes as a deterministic regression test in the core
module that owns the parser (a `#[test]` with the bytes inline or under
`tests/fixtures/`), fix the parser, and commit the input as a seed so the
nightly run keeps covering it.

## CI

`.github/workflows/fuzz.yml` runs every target for 300 s nightly (3 AM
UTC) and on `workflow_dispatch` (with a `max_total_time` input). One
matrix job per target, run in parallel, the fuzz build cached across runs;
a failing target uploads its `fuzz/artifacts/` and the `notify` job opens or
updates a pinned `fuzz` issue, the same way `mutation-tests.yml` does. A new
target goes in the matrix; one that needs a fuzz-crate feature goes under
`include:` with `features:` (the workflow has `remote_footer`'s entry
commented out, ready for when #683 closes). A crash a new target
finds gets its own issue, as #623, #681, #683, #684, #686, #687 and #690 did.

```bash
gh workflow run fuzz.yml                        # 300 s per target
gh workflow run fuzz.yml -f max_total_time=60   # a quick smoke run
```
