# H3 — Streaming / bounded-memory overview conversion

Date: 2026-07-03. Branch: `feat/streaming-overviews`.

Implements plan item **H3(a)** (Phase 5 / V4): the two-pass streaming
convert pipeline (`crates/core/src/overview/stream.rs`). Pass 1 streams
the input (geometry + ranking columns only) into per-feature winner
tables (level assignment + Q2 density budget, O(occupied-cells) engine
state, 1-byte-per-feature result). Pass 2 re-reads the seekable input
once per level, filters each read batch against the winner table,
simplifies only the selected rows, and writes batch-by-batch through
the existing `OverviewWriter` (per-level row-group sizing via
`level_row_hint` = winner count).

Streaming is the **default**; `--no-streaming` keeps the original
in-memory pipeline as the reference implementation (equivalence-tested
in `overview::convert::tests::streaming_matches_*`).

## Moldova before / after

Dataset: `corpus/data/gpio/polygons-ftw-moldova-large.parquet`
(631,910 polygons, 38 M canonical vertices). Command (identical to the
V3 `run_conversion.sh` recipe):

```bash
/usr/bin/time -v target/release/tylertoo overview \
  corpus/data/gpio/polygons-ftw-moldova-large.parquet \
  out.parquet --mode duplicating \
  --min-zoom 0 --max-zoom 14 [--no-streaming]
```

Both runs on the same release build, same machine, sequential
(raw `time -v` logs: `corpus/data/bench/h3/moldova.{mem,stream}.time.txt`).

| pipeline | wall time | Max RSS |
|---|---|---|
| in-memory (`--no-streaming`), this machine | 11:50.48 | **5.30 GB** (5,557,952 kB) |
| in-memory, plan-recorded baseline (V1, 2026-07-02) | 10:57 | 5.44 GB |
| **streaming (default)** | **11:46.07** | **305.7 MB** (312,996 kB) |

- **Peak RSS: 5.30 GB → 305.7 MB (−94 %, 17.8×), well under the 1 GB
  acceptance target.**
- Wall time: unchanged (11:50 → 11:46). The per-level re-read +
  re-decode of pass 2 costs about what the in-memory path's
  whole-table `take()` + per-level geometry cloning did. The H3(c)
  wall-time item (per-level decode→rebuild churn, WKB caching /
  cascaded simplification) remains open and is orthogonal.

## Output equivalence (Moldova, duplicating z0–14)

- `geo:overviews` footer JSON: **byte-identical** between the two runs.
- `geo` (GeoParquet 1.1) footer JSON: **byte-identical**.
- Rows: 1,583,053 across 12 emitted levels in both; row groups: 167 in
  both; per-level feature and vertex counts identical at every level.
- Both files pass all 9 `tylertoo validate` checks.
- Compressed level bytes differ marginally (e.g. canonical 97.25 vs
  97.33 MiB): the streaming writer encodes many read-batch-sized
  batches per row group instead of one level-sized batch, which
  changes Parquet page framing, not values.

## Knobs added (core `ConvertOptions` → CLI → docs)

| flag | default | meaning |
|---|---|---|
| `--no-streaming` | off (streaming on) | revert to the in-memory reference pipeline |
| `--read-batch-size ROWS` | 8192 | rows per Arrow read batch in both passes; bounds the transient working set |

Documented in `context/OVERVIEW_TUNING.md` § "Memory / streaming knobs".

## Residual memory + behavior notes

- Residual O(N) state in streaming mode: pass 1 holds the
  `AssignFeature` vector (~48 B/feature) plus ranking-key vectors
  (16 B/feature) while the assignment runs (freed before pass 2);
  pass 2 carries only the 1 B/feature winner table. ~40 MB total for
  Moldova; scales linearly but stays laptop-sized at planet tier.
- The remaining per-batch hotspot is a single read batch of decoded
  geometries (WKB → `geo::Geometry`); `--read-batch-size` bounds it.
  There is **no** in-memory per-level sort in either path: Hilbert
  order comes from the gpio-sorted input contract and input order is
  preserved within each level.
- One documented behavior divergence (see `stream.rs` module docs):
  the in-memory path omits a level whose *post-simplification* output
  is empty; the streaming path decides level omission from the winner
  table (pre-simplification). They differ only if every winner of a
  level degenerates during simplification — impossible under default
  knobs (assign visibility gates 2–4×GSD are stricter than the 1×GSD
  simplify drop gate) — in which case the streaming writer fails
  loudly with `EmptyLevel` instead of silently renumbering.
- H3(b) (export: flush finished tiles instead of holding a zoom map)
  is **not** part of this change; export still holds one zoom in
  memory.

# H3(b) — Bounded-memory export + serial-merge removal

Date: 2026-07-03. Branch: `fix/export-memory-flush` (on top of the
H3(c) levers, base b8a1635).

Restructures `crates/core/src/overview/export.rs` from per-zoom
whole-band materialization (level `Vec<Feature>` + staged per-feature
clip results + a **serial** `BTreeMap` grouping merge + all encoded
tiles held until written) to a partitioned streaming pipeline:

1. **Scan pass** per level: stream the band once (geometry decode
   only) → per-tile member counts, band row count, overall bounds.
   O(#tiles) state.
2. **Partition**: split the zoom's tiles into contiguous ascending
   `(x, y)` ranges of ~`DEFAULT_PARTITION_TARGET` = 32,768
   (feature × tile) members.
3. **Partition pass**: partitions run in parallel waves of
   `PARTITION_WAVE` = 6 (order-preserving collect, serial in-order
   write). Each partition re-reads a row-group-pruned subset of the
   band (conservative bbox pruning via the covering stats; skipped for
   EPSG:3857 inputs), clips features in parallel per 8k-row batch
   (`OverviewReader::read_level_with_batch_size`, new), groups by a
   **parallel `(tile key, band order)` sort** — the serial per-zoom
   `BTreeMap` merge is gone — and MVT-encodes tiles in parallel.
   Finished partitions stream straight to the PMTiles writer.

Parallel: per-batch clip, member grouping sort, per-tile MVT encode,
and up to 6 concurrent partitions. Serial: band scan, per-batch
decode/property extraction (within each partition), and the in-order
gzip/write loop (unchanged).

## Byte-identity

Tiles are still added zoom-ascending and `(x, y)`-ascending within a
zoom (the packed tile key sorts exactly like the old `BTreeMap<(x, y)>`
iteration), and within-tile members keep band order, so tile bytes,
dedup offsets, directory, and metadata are unchanged. Verified three
ways:

- `overview::export::tests::export_archive_matches_pre_refactor_reference`:
  whole-archive xxh3 pinned from the pre-refactor implementation
  (captured at b8a1635 before the rewrite) on a mixed-geometry
  2-level fixture — green after the rewrite.
- `overview::export::tests::partitioned_export_is_partition_invariant`:
  `partition_target = 1` (max partitions, max band re-reads) vs the
  default produce byte-identical archives and identical reports.
- Moldova: `cmp` of the full 123 MB archive, pre- vs post-refactor
  build — **byte-identical**; reports identical modulo
  `duration_secs`. 17,372 tiles, 1,807,912 tile features, 0 oversized
  in both.

## Moldova before / after

Input `corpus/data/bench/h3/moldova.dup.stream.parquet` (632k
polygons, duplicating z0–14), command:

```bash
/usr/bin/time -v target/release/tylertoo \
  export-pmtiles \
  corpus/data/bench/h3/moldova.dup.stream.parquet \
  out.pmtiles --layer-name fields
```

Caveat: the machine was heavily loaded by unrelated jobs (load avg
13–30 of 16 cores) throughout these runs, so absolute walls are
inflated vs the idle-machine H3(c) figures (58.8 s / 2.38 GB).
Before/after pairs were therefore run back-to-back under the same
load; best-of-pairs shown:

| build | wall (paired, loaded) | CPU | Max RSS |
|---|---:|---:|---:|
| pre-refactor (b8a1635 + anchor test) | 75.5 s | 717 % | **2.41 GB** |
| **partitioned streaming** | **73.9 s** | 688 % | **0.89 GB** |

- **Peak RSS 2.4 GB → 0.89 GB (−63 %), under the 1 GB target.** The
  bound is now O(wave × partition): ~6 × (32k members + one 8k-row
  batch transient), independent of the zoom band size.
- Wall: parity with the pre-refactor build under identical load
  (73.9 vs 75.5 s; run-to-run noise under this contention was ±25 %,
  e.g. ref itself ranged 75.5–103.5 s). The serial `BTreeMap` merge is
  eliminated (per-partition sort+encode is 0.2–0.6 s at z14), but the
  per-partition band re-reads add ~3.5× row-read redundancy at z14
  (row groups pruned per partition; Hilbert row groups vs `(x, y)`
  stripe partitions overlap imperfectly), and wave scheduling absorbs
  most of the freed serial time. The 8.4× Amdahl ceiling (~32 s) was
  not demonstrable under this load; on the paired evidence the
  restructure is wall-neutral and memory is the win.
- Debug-level `[profile]` instrumentation now also logs per-partition
  `rows_read / members / collect / sort+encode` (kept, like the H3(c)
  instrumentation, behind `RUST_LOG=tylertoo_core::overview=debug`).
