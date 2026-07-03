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
/usr/bin/time -v target/release/gpq-tiles overview \
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
- Both files pass all 9 `gpq-tiles validate` checks.
- Compressed level bytes differ marginally (e.g. canonical 97.25 vs
  97.33 MiB): the streaming writer encodes many read-batch-sized
  batches per row group instead of one level-sized batch, which
  changes Parquet page framing, not values.

## Knobs added (core `ConvertOptions` → CLI → docs)

| flag | default | meaning |
|---|---|---|
| `--no-streaming` | off (streaming on) | revert to the in-memory reference pipeline |
| `--read-batch-size ROWS` | 8192 | rows per Arrow read batch in both passes; bounds the transient working set |

Documented in `docs/OVERVIEW_TUNING.md` § "Memory / streaming knobs".

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
