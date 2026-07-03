# Performance profile — methodology + history

Companion to [`RESULTS.md`](./RESULTS.md) (storage/access numbers).
This page records **how** the pipeline's performance was measured and
the sequence of changes that produced the current numbers. The full
point-in-time engineering notes are archived in
[`context/archive/bench/`](../../context/archive/bench/) —
`H3_NOTES.md` (streaming convert + export restructure),
`H3C_PROFILE.md` (wall-time profile + optimization levers),
`EXPORT_NOTES.md` (E0 export design/validation).

## Current headline (Moldova, the large stress case)

`polygons-ftw-moldova-large` — 631,910 polygons, 38 M canonical
vertices, duplicating mode, z0–14 (12 emitted levels), release build,
16-core machine:

| Command | Wall | Peak RSS |
|---|---:|---:|
| `gpq-tiles overview` (convert) | **~55 s** | **~320 MB** |
| `gpq-tiles export-pmtiles` | **~59 s** | **~2.4 GB** |

Full pipeline (GeoParquet → overview file → PMTiles) < 2 min.

## Methodology

- **Wall/RSS**: `/usr/bin/time -v` on the release binary; identical
  commands to `run_conversion.sh`.
- **Phase breakdown**: `std::time::Instant` accumulators in
  `crates/core/src/overview/stream.rs` and `export.rs`, logged at
  `RUST_LOG=gpq_tiles_core::overview=debug` (instrumentation is
  retained behind that log level).
- **Heap**: `cargo build --release --features dhat-heap` (see
  `docs/PROFILING.md`).
- **Output equivalence**: every perf change was gated on footer/row
  equivalence and all `gpq-tiles validate` checks passing.

## History (all 2026-07-03, chronological)

1. **H1 — writer footer pathology fixed.** Per-row-group min/max stats
   suppressed on geometry/string columns + per-level row-group sizing.
   Moldova duplicating footer 8.84 MB → 0.24 MB (−97 %), file
   −12.5 %, remote viewport bytes −42…−81 %. Details: the H1 revision
   note in `RESULTS.md`.
2. **H3(a) — two-pass streaming convert (now the default).** Pass 1
   builds per-feature winner tables; pass 2 re-reads per level and
   writes batch-by-batch. Peak RSS 5.30 GB → 305.7 MB (−94 %), wall
   unchanged; output equivalent (footer byte-identical). Archived:
   `H3_NOTES.md`.
3. **H3(b) — bounded-memory export.** Per-zoom whole-band
   materialization + serial BTreeMap merge replaced with a partitioned
   streaming pipeline into the PMTiles writer. Archived: `H3_NOTES.md`
   (second half).
4. **H3(c) — wall-time profile + four levers.** The profile showed
   both commands strictly serial with simplification 96.9 % of convert
   wall — and *inside* simplify, `geo` ring-validation (not RDP)
   dominating, with invalid candidates triggering full-resolution
   re-validation. Levers shipped: (1) rayon in convert pass 2,
   (2) rayon per-feature clip / per-tile encode in export, (3)
   validation cuts + eps-halving fallback fix in `overview::simplify`,
   (4) export bbox fast path skipping the clip for fully-interior
   features. Result: convert 706 s → 55.3 s (**12.8×**), export
   288 s → 58.8 s (**4.9×**); the fallback fix also cut the Moldova
   overview file −18.6 % (coarse levels no longer quietly carry ~5–9 %
   of features at full resolution). Remaining known lever: cascaded
   simplification (simplify level k from level k+1's output).
   Archived: `H3C_PROFILE.md`.

## Known behavior notes (still true)

- Streaming vs in-memory divergence: the streaming path decides level
  omission from the winner table (pre-simplification) and fails loudly
  with `EmptyLevel` if every winner of a level degenerates — impossible
  under default knobs. (`stream.rs` module docs.)
- Export reintroduces MVT border duplication: a feature spanning a
  tile seam appears in every tile it touches (0 % while a level fits
  one tile, ~7 % at z14 on Portland roads). Accounting table:
  archived `EXPORT_NOTES.md`.
- `--tile-size-limit` is a single non-iterative drop pass per
  oversized tile — a backstop, not the sizing mechanism.
