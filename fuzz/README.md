# tylertoo fuzz targets

`cargo-fuzz` targets for the untrusted-bytes surfaces of `tylertoo-core`
(issues #187, #424). This crate is a **standalone workspace** — it is not a
member of the root workspace and never affects `cargo build/check/test
--workspace` or per-PR CI time.

## Targets

| Target | Surface | Seeds |
|--------|---------|-------|
| `pmtiles_header` | `pmtiles_writer::Header::from_bytes` — the 127-byte header of any archive handed to `decode`, `merge` or `--band`; a header that parses must re-parse after `to_bytes` | the golden fixtures' headers |
| `pmtiles_directory` | `pmtiles_writer::decode_directory` — root and leaf directories (#397's class of bug); a directory that decodes must round-trip through `encode_directory` | the golden fixtures' root directories, decompressed |
| `mvt_decode` | `decode::fuzz_decode_tile` → `decode_tile_features` — one uncompressed MVT tile body; the first input byte picks the zoom (`z & 31`) | three tiles per golden fixture, decompressed, zoom byte prepended |
| `band_archive` | `pyramid::fuzz_band_archive` → `BandArchive::open` + `for_each_tile` — a whole archive as `--band` input, through the directory-walk budgets and the decompression caps (#417) | `open-buildings.pmtiles` (31 KB) |
| `filter_expr` | `overview::filter::parse_filter` — the `--filter` grammar (recursive descent; deep nesting must be an error, not a stack overflow) | a handful of expressions |
| `footer_json` | `overview::level::OverviewsMeta::from_json` — the `overviews` footer JSON read verbatim from any input file | none |
| `wkb` | `wkb::wkb_to_geometry` — WKB geometry blobs read verbatim from any input file | none |

`mvt_decode` and `band_archive` reach code that is private on purpose
through `#[doc(hidden)]` hooks in core gated on the `fuzzing` cargo feature
(`crates/core/Cargo.toml`). Only this crate enables that feature; it is
outside the public-API baseline.

## Seeds

Seed corpora live in `corpus/<target>/seed-*` and are committed (about
130 KB in all). `fuzz/.gitignore` un-ignores exactly the `seed-*` names, so
everything libFuzzer adds to `corpus/<target>/` during a run (sha1-named)
stays untracked. To promote a discovered input to a seed, copy it to a
`seed-<description>` name and commit it.

The PMTiles seeds are cut from `tests/fixtures/golden/*.pmtiles`: header =
first 127 bytes; directory = the root directory bytes, gzip-decompressed;
tile = a tile body, gzip-decompressed, with the archive's max zoom as the
leading byte.

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
matrix job per target, the fuzz build cached across runs; a failing target
uploads its `fuzz/artifacts/` and the `notify` job opens or updates a
pinned `fuzz` issue, the same way `mutation-tests.yml` does.

```bash
gh workflow run fuzz.yml                        # 300 s per target
gh workflow run fuzz.yml -f max_total_time=60   # a quick smoke run
```
