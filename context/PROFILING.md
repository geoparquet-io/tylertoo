# Profiling tylertoo

How to measure where time and memory go. The recorded performance
history of the pipeline (what was measured, what was fixed) lives in
[`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/overview/PROFILE.md).

## Memory envelope

These are code-derived sizes for a 64-bit target. They are arithmetic
allocation models, not measured peak RSS. GB means 10^9 bytes. GiB means
2^30 bytes. Do not add peaks from separate phases.
Within a phase, add the tables, active batches, workspace, and allocator
headroom that overlap.

Feature count, vertices, properties, and emitted tiles determine memory
needs. A country boundary alone gives no memory bound. Planet scale needs
larger RAM or sharding. Spilling pass 2 does not spill pass 1 or the archive
directory. The calculations below explain those limits.

### Measured peaks

Benchmark results are pending. The arithmetic below is not a measurement.
Run each input under both profiles with local spill disk and
`TYLERTOO_PROFILE_JSON` enabled. Input features is `pass1.rows`.
Peak RSS is the larger of conversion's `peak_rss_mib` and export's
`export.peak_rss_mib` in the two JSON Lines (JSONL) records from `tiles`, converted from
MiB to GiB. Pass-2 peak share is
`100 × rss_sampler.phase_peaks_mib["pass2 (output sink)"] / peak RSS`,
using MiB for both terms. This compares peaks without attributing memory
to pass 2. The sampler polls every 250 ms and can miss short spikes.
Fields of The World (FTW) supplies the Brazil MT input.

| Input | Profile | Input features | Peak RSS (GiB) | Pass-2 peak share (%) | Host |
| --- | --- | ---: | ---: | ---: | --- |
| NZ parcels (z0–14) | `auto` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |
| NZ parcels (z0–14) | `bounded` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |
| FTW Brazil MT (z0–13) | `auto` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |
| FTW Brazil MT (z0–13) | `bounded` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |
| Overture segments full part (z0–14) | `auto` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |
| Overture segments full part (z0–14) | `bounded` | {{W2}} | {{W2}} | {{W2}} | 64-core aarch64, 123 GB RAM, 24 GB cgroup |

### Budget and profiles

[`MemoryBudget`](../crates/core/src/overview/memory.rs) starts with a memory
probe. On Linux it uses the smaller of `MemAvailable` and reclaim-adjusted
cgroup headroom (`memory.max` and `memory.high`, or the v1 limit).
`TYLERTOO_AUTO_MEM_LIMIT_BYTES` overrides both. Let `M` be that probe,
`R` the resident reservations, and `G` the grants already made:

```
remaining allowance = max(0, 0.60 × max(0, M − R) − G)
```

Without a probe, the allowance is 4 GiB. Read-ahead and the pass-2 compute
window each get 10% of the remaining allowance when sized. Row-group
encodes get 25%. Each grant reduces what later phases can use. With
32 GiB available and no charges, the allowance starts at 19.2 GiB.
The shares start at 1.92 GiB and 4.8 GiB; earlier grants reduce them.
These are budget calculations, not a promise about total process memory.

- `auto` sizes concurrency to the allowance and spills output when needed.
- `bounded` spills output and caps readers at 4 and queues at 16 batches
  per reader. Row-group encodes start with at most 4. The buffered drain can
  add encodes using released batch memory, limited by headroom below the
  prior RSS peak and the thread count.
- `speed` takes the requested concurrency and keeps output in RAM.

All profiles retain minimum working units. A read or compute window needs
at least one batch. The writer needs up to 4 encodes, pool permitting.
Export needs at least one partition, which contains whole tiles. These
floors can exceed a small allowance. Pass-1 preflight prices the table at
42 B per input row and warns when 3.3× that estimate exceeds probed memory.
It fails only when the table alone exceeds a hard cgroup limit and no
per-feature filter can reduce the row count. `TYLERTOO_SKIP_MEMORY_PREFLIGHT=1`
downgrades that failure to a warning. Passing preflight does not prove the
whole run fits.

### Pass 1: scan and assignment

[`FeatureTable`](../crates/core/src/overview/assign.rs) stores columns,
not the old 64-byte `AssignFeature` structs. For `N` retained features:

| Column | Bytes per feature | When allocated |
| --- | ---: | --- |
| Input row index (`usize`) | 8 | Always |
| Center x, center y, squared diagonal (`f64`) | 24 | Always |
| Geometry kind (`u8`) | 1 | Always |
| Sort key (`f64`) | 8 | Once any finite key exists, including auto-ranking |
| Entry level (`u8`) | 1 | Once any entry ladder value exists |
| **Table total** | **33–42** | 41 with a sort key alone; 34 with a ladder alone |

Thus 300 million features cost `300M × (33…42)` = 9.9–12.6 GB
(9.22–11.73 GiB) for this table alone. At 1.58 billion features it is
52.14–66.36 GB (48.56–61.80 GiB).

Other live allocations in
[`stream.rs`](../crates/core/src/overview/stream.rs) and `assign.rs` matter:

- Ranking extraction holds 16 B per input row per candidate numeric key
  (`Option<f64>`), including auto-detected road-class candidates and
  confidence. Class groups add 4 B per row plus interned strings when
  coalescing collects them. These scratch columns can overlap the final
  sort-key column before scan assembly releases them.
- An entry ladder adds 16 B per input row of source values. Resolution
  also builds value/rung arrays and a 2 B/row `Option<u8>` result before
  writing the table's 1-byte entry column.
- Polygon areas add `4N` bytes when tiny-polygon accumulation is active.
  Accumulation source values add 16 B per input row per requested numeric
  attribute (`Option<f64>`). Cluster categories add 4 B per input row plus
  interned values. Clustering also builds feature-indexed copies and
  per-level cluster tables. Their size depends on the chosen options.
- Cell assignment holds a 1 B/feature minimum-level vector. Its winner
  lists use 8 B per winning position. The grid scheduler prices each
  occupied cell at 96 B and packs levels into waves. The grid allowance is
  the profile allowance less `42N`; at least one level still runs. Pending
  placements target about 2 MiB per pool thread across a wave, with a
  1 MiB floor per level. Grid estimates are not exact heap sizes.
- `FeatureAssignment` is 16 B: an 8-byte index, a 1-byte level, and
  alignment padding. The density fold retains that input assignment plus
  `cw_min`, `admitted`, and `admitted_at` (1 B/feature each). Its kind mask
  adds another 1 B/feature unless the budget covers all kinds. If there are no
  features of a budgeted kind, it clones the assignment and returns.
- During a binding density cut, `C` candidates and `H` already admitted
  features need `8C + 8H` bytes of positions. Tagged candidates add `32C`
  (`(u128, usize)` with padding); held super-cells add `16H`. Chosen
  positions add `8K`. Cell lists and water-filling workspace add terms in
  the number of occupied super-cells, including up to 48 B/cell for events.
  Output assignment construction can overlap two 16 B/feature assignment
  vectors. No 40 B/feature priority array remains: comparisons derive keys.

Consider 300 million polygons. Area accumulation needs an extra column. Assume
ranking and entry ladders are off, so the point budget has no points to thin.
The feature table plus areas and two assignment vectors come to `300M × (33 + 4 + 16 + 16)` = 20.7 GB (19.28 GiB).
This excludes grid peaks, batches, capacity slack, and retained allocator
pages. It is a subtotal for that path, not a universal B/feature rate.
For 1.58 billion rows the same subtotal is 109.02 GB (101.53 GiB), already
beyond a 32 GB host. Changing `--profile` cannot spill those arrays.

Before pass 2, the pipeline frees the feature table. The row-indexed minimum-level
map remains at 1 B per input row, including skipped rows. Kinds add another
1 B/row when coalescing or plan saving needs them. Carrier positions cost
8 B each per level; cluster and coalescing tables remain when enabled.
Vector capacity and scan chunks can add transient memory above element sizes.

**Line exception.** Coalescing collects geometry in pass 1 by default.
Its guard allows at most 2 million lines and `2M × 512` = 1.024 GB modeled
geometry. The guard prices each line at `56 + 16 × vertices` bytes. The chain
stage needs further workspace and keeps per-level results; the collection
guard does not cap that peak. See
[chain-stage memory](../docs/OVERVIEW_TUNING.md#chain-stage-memory).
`--no-coalesce-lines` removes that work but changes generalization.
`--coalesce-max-level-rows` changes both guard limits and can change output.

### Pass 2: batches, sink, and spill

[`read.rs`](../crates/core/src/overview/read.rs) prices a decoded input row
as `p = 2 × ceil(selected column bytes / selected rows) + 8 × columns`.
It uses uncompressed footer bytes, or larger raw byte-array sizes.
Generated integer count columns add 8 B each plus validity bits. A read
batch costs roughly `batch_rows × p`, capped at the reader's decoded-batch
limit for admission. Each reader also holds its largest selected row group.
The compute window prices a batch at `batch_rows × (1 + fanout) × p`.
Here `fanout` counts planned output rows per input row across the active levels.

Let `B` count buffered output rows, excluding a streamed canonical level,
`g` the mean encoded geometry bytes, and `I` the source row count. In
[`memory.rs`](../crates/core/src/overview/memory.rs), the model prices an unshared sink
at `B × p` when footer prices exist. Otherwise the row model is
`256 + 2g` bytes in duplicating mode or `256 + 4g` in partitioning mode;
without a geometry size the fallbacks are 8192 and 16384 B/row.

For shared RAM batches, let `J = min(B, I)`. The estimate becomes:

```
min(B × p, J × (p + 256) + (B − J) × g + 16 × B)
```

If `g` is unknown, that term uses `p`. Levels share properties once per source batch. Each level keeps geometry
positions and counts.
This path needs duplicating mode, cascading simplification, no clustering,
and more than one buffered level. Each level still adds memory.
For example, `B = 20M`, `I = 10M`, `p = 1000`, `g = 100` gives
`min(20 GB, 12.56 + 1 + 0.32 GB)` = 13.88 GB (12.93 GiB).

[`pipeline.rs`](../crates/core/src/overview/pipeline.rs) compares that
estimate with the allowance left after grants. `auto` also spills
partitioning output above 2 million buffered rows. During RAM buffering it measures
Arrow bytes plus a 256 B floor per materialized row. The tuner counts shared source rows once; each level adds its own arrays. The tuner projects
`held bytes + remaining planned rows × observed bytes/row` after each
input batch and can switch once to spill. Arrow array accounting can
count shared buffers more than once, so this remains a sizing estimate.

`bounded` spills LZ4 IPC; an `auto` spill uses raw IPC. The cgroup page-cache
reservation models spill at `0.4 × B × row price` for LZ4 and
`1.0 × B × row price` for raw IPC. It reserves that cost before grants only
when the spill fits the allowance. This ratio is a code constant, not a
compression measurement for the current input. Each spilled level has a
2-batch queue plus the batch its writer consumes. Those add roughly
`3 × batch_rows × p × spilled_levels` bytes, plus the producer's current
batch and IPC codec workspace. Put `--spill-dir` on real disk: tmpfs
charges the spill itself to RAM.

The initial row-group encode grant takes 25% of the remaining allowance,
priced for Arrow rows and encoded chunks. One encode costs
`5 × planned rows/group × output row price` in the model. A drain slot
prices at least one WKB chunk of rows. During the buffered drain,
`MemoryBudget::drain_encodes` can reuse memory the finished read and
compute window held. Reuse stops at their grants or the RSS headroom
below the prior peak. It adds only the slots that fit, up to the thread
count. These are admission estimates, not measured peaks.

### Export: tile counts and active members

[`LevelScan`](../crates/core/src/overview/export.rs) keeps a
`BTreeMap<u64, usize>` per level. Each occupied tile has 16 B of key/value
payload on a 64-bit host, plus node headers, edges, unused slots, and
allocator overhead. Counts are tile memberships, not distinct input rows.
A feature crossing tiles contributes to each. Maps for all scanned levels
can coexist. At 80 million occupied tiles, payload alone is
`80M × 16` = 1.28 GB (1.19 GiB). This is arithmetic, not a measured map
peak. The full map rate depends on occupancy and allocator overhead.
Neither profile spills these maps.

For active clipped members, the current admission model is
`q = 2 × stored bytes/row + 224 + 208 × published properties`.
The default merge valve adds 384 B/member to wave, group, and store sizing.
For a 100-byte stored row with four properties, `q = 1256` B and the
merge fit price is 1640 B/member. A million active members then price at
1.64 GB (1.53 GiB), before separately charged caches and reader buffers.
These constants model geometry and property values alongside encoding and
valve workspace. They are not fixed struct widths or new measurements.

Export collection and encoding use shares equivalent to half the probed
memory without prior charges (`0.60 × 5/6 = 0.50`). Earlier grants
reduce later ones. `auto` narrows waves and keeps only the level-store
members that fit; `bounded` spills stores and uses its minimum wave shape.
`speed` keeps members in RAM. Spill fill buffers start from a 128 MiB
model and adjust to measured segment sizes. One tile is never split across
partitions. The single-tile check uses a separate OOM probe. It checks
`MemAvailable` and the hard cgroup limit, excluding `memory.high`. It refuses a tile above that
headroom under every profile. An unknown price or limit skips the check.
`TYLERTOO_AUTO_MEM_LIMIT_BYTES` and `TYLERTOO_SKIP_MEMORY_PREFLIGHT` make
this check warn instead of refusing. The size valve runs after collection.

Lower `--read-batch-size`, `--read-workers`, `--in-flight-batches`, or
`--partition-wave` to reduce active work. Profiles and these concurrency
controls preserve tile bytes. Smaller input row groups prepared with `gpio`
reduce each reader's resident decode. Property selection reduces row and
member width but changes the published attributes. Changing zooms or
generalization can reduce memory by changing which features reach the tiles.

### Writer: entries and deduplication

[`StreamingPmtilesWriter`](../crates/core/src/pmtiles_writer.rs) sends
compressed bodies straight to disk but keeps metadata for every added tile.
Let `T` count emitted tiles, `U` distinct stored bodies, and `D` directory
entries after run-length encoding (`D <= T`):

| Allocation | Bytes | Depends on |
| --- | --- | --- |
| `StreamingDirEntry` vector | `24 × capacity` | 8-byte id + 8-byte offset + 4-byte length + 4-byte padding |
| Body hash table | 24 B key/value payload per bucket, plus control bytes and spare capacity | 8-byte hash + 16-byte padded `(offset, length)`; grows with `U` |
| `DirEntry` vector during checkpoint/finalize | `24 × capacity` | 8-byte id + 8-byte offset + 4-byte length + 4-byte run length; grows with `D` |

`estimated_memory_bytes()` uses `24T + 40U`, allowing roughly for hash
occupancy. At `T = U = 100M`, that is 6.4 GB (5.96 GiB). An unmerged
`D = 100M` directory adds at least 2.4 GB, bringing the subtotal to
8.8 GB (8.20 GiB). Vector growth, sorting workspace, encoded directory
buffers, compressed leaves, and allocator retention add more. Adjacent
identical bodies reduce `D` and dedup reduces `U`, but each tile still has
its streaming entry. Hash collisions also need overflow records. The writer verifies ordinary
hash hits against stored bytes. Ascending export writes avoid
the fallback clustered-offset hash map.

No memory profile spills these writer tables. A slower disk cannot solve
this ceiling. Lower `T` with a smaller zoom range, or split the export;
merging all shards into one archive still needs the final writer state.
The 100 million-tile example is arithmetic, not a measured writer peak.

For capacity planning, take the largest phase subtotal and add measured
batch/workspace peaks and headroom. At 1.58 billion input rows, even the scan
floor alone needs 49–62 GiB. Plan a larger host for the full-input coarse
job, then size the shards separately. Do not treat 64 GiB as a planet
budget or the historical 192 GiB failure as a new measured requirement.
The planet-scale figures here are arithmetic, not measured peaks.
For scheduler cache accounting, see
[the scaling guide](../docs/guides/scaling.md#reading-a-jobs-maxrss).

### Design notes after 1.0

- **Pass-1 table spill:** store scan columns and assignment scratch in
  external runs. Keep winner ordering and canonical fidelity unchanged.
- **A Bloom filter before dedup:** test whether a body hash might exist before a
  disk-backed exact lookup. Verify positive hits against bytes; a Bloom
  filter alone cannot decide tile identity.
- **External merge for directories:** sort entry runs and stream directory
  construction, so checkpoints and final archives need bounded RAM.

These are design directions, not available profile options.

## Phase timing (built in)

The overview pipeline keeps phase accumulators (pass 1 scan, assignment,
per-level read/decode, simplification, write; export clip/encode/write)
behind the `log` debug level:

```bash
RUST_LOG=tylertoo_core::overview=debug \
  tylertoo overview input.parquet output.parquet

RUST_LOG=tylertoo_core::overview=debug \
  tylertoo export-pmtiles output.parquet tiles.pmtiles
```

For end-to-end wall time and peak RSS, wrap the release binary in GNU
time:

```bash
cargo build --release
/usr/bin/time -v ./target/release/tylertoo overview \
  input.parquet output.parquet --min-zoom 0 --max-zoom 14
```

`Maximum resident set size` is the peak-RSS number quoted throughout
the benchmark docs.

## Structured JSON profile dump (`TYLERTOO_PROFILE_JSON`)

Set `TYLERTOO_PROFILE_JSON=<path>` and each successful run appends one JSON
object per phase (one line each, JSONL) to that file: a convert line from
`overview` run through the **streaming pipeline** (the default), and an
export line from `export-pmtiles`. A one-shot `tiles` run does both, so it
appends **two** lines — convert's, then export's — paired by a shared
`run_id` (see "Two JSONL lines for one `tiles` run" below). This is the
measurement base the perf series (pass-1 parallelization, pass-2 throughput,
checkpoint work and, since #535, export throughput) is gated on:

```bash
TYLERTOO_PROFILE_JSON=/tmp/profile.jsonl \
  tylertoo tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 10
# JSONL: one object per line, so pretty-print line by line (or `jq -s .`
# to load the whole file as an array of the two objects `tiles` writes).
while read -r line; do echo "$line" | python3 -m json.tool; done < /tmp/profile.jsonl
```

It is an env var rather than a CLI flag so this diagnostics-only knob costs
no CLI-doc churn, and it is best-effort: an unset/blank var is a no-op, and
a write failure only logs — it never fails the conversion (see
**Early warning** below).

### Schema

```jsonc
{
  "timestamp": 1790248637.27,       // UNIX epoch seconds
  "run_id": "48213-1790248630112345678", // "<pid>-<unix nanos>", one per process
  "phase": "convert",
  "phase_walls": {                  // DISJOINT wall-clock windows, run order
    "pass1": 0.0068,                // the scan alone (stops at scan end)
    "assign": 0.0021,               // resolve_winner_tables: winners, the
                                    // density budget, carriers, clusters
    "pass2": 0.0228,                // the level build (stops before finish)
    "writer_finish": 0.0005,        // writer.finish() alone
    "total": 0.0330                 // the whole conversion
  },
  "pass1": {
    "rows": 1000,                   // input rows streamed
    "rows_per_sec": 33682.7,
    "stage_secs": {                 // pass-1 stage breakdown, in seconds —
      "read": 0.00199,              // summed core-seconds; may exceed the
      "decode": 0.00265,            // phase wall (phase_walls.pass1) under
      "scan": 0.00022,              // parallelism (true once #504's parallel
      "keys": 0.00005,              // pass-1 scan lands; until then pass 1
      "assemble": 0.00006           // is serial and they agree)
    }
  },
  "pass2": {
    "rows": 1000,                   // OUTPUT rows written across every level
    "rows_per_sec": 43789.5,
    "stage_secs": {                 // pass-2 stage breakdown, in seconds —
      "read": 0.00237,              // folded across every level, including
      "decode": 0.00251,            // the finest (#517): a fixed set of
      "simplify": 0.00003,          // stages run in parallel across levels
      "build": 0.00177,             // under the pipelined engine, so these
      "drain": 0.0,                 // are core-seconds that can exceed
      "spill_write": 0.0            // phase_walls.pass2's wall time
    },
    "identical_steps": {            // cascade-fold steps that reused the
      "shared": 1632,               // previous level's geometry (#499);
      "total": 26909,               // 0/0 when the fold never ran: a Serial
                                    // run, or a single-level ladder
      "ratio": 0.0606
    },
    "sink": {                       // #626, #852: accounted retained bytes
      "bytes_per_row": 1872,        // per buffered output row; source rows
                                    // shared across levels counted once,
                                    // with a 256 B/materialized-row floor;
                                    // null when nothing was buffered
      "sampled_rows": 65536,        // buffered rows measured: every one
                                    // of the pass
      "projected_bytes": 12275712,  // bytes buffered + rows still planned
                                    // x bytes_per_row; at the end of the
                                    // pass, what was actually buffered
      "budget_bytes": 34359738368,  // allowance left after earlier grants;
                                    // 0 unless auto starts with RAM sinks
      "downgraded_to_spill": false  // auto flipped the RAM sinks to spill
    },                              // mid-pass because the projection
                                    // passed the budget
    "windows": {                    // #494: input batches the pass-2
      "batches": 156,               // consumers took, summed over both
                                    // reads (buffered levels + finest)
      "windows": 31,                // how many windows they ran in
      "max_window": 8,              // the widest one: at most
                                    // in_flight / 2; stuck at 1 means
                                    // pass 2 was reader-bound
      "mean_window": 5.03           // batches / windows
    }
  },
  "pass1_memory_preflight": {       // #543: the pre-scan verdict
    "verdict": "fits",              // fits | warned | exceeded_but_forced |
                                    // not_checked (a --plan replay)
    "rows": 1000,                   // footer-counted input rows
    "estimated_bytes": 42000,       // rows x PASS1_BYTES_PER_ROW (42)
    "limit_bytes": 34359738368,     // the probed figure; null if unprobed
    "limit_source": "available memory (MemAvailable)"
  },
  "pass0": {                        // #788: remote staging; null for a local
    "stage_secs": 1.2,              // input or a staging that stopped on an
    "requests": 13,                 // error
    "bytes_to_stage": 105060035,
    "bytes_fetched": 105060035,
    "max_in_flight": 8,
    "failed_requests": 0,
    "unstaged_bytes": 0
  },
  "levels": [                       // per WRITTEN level, writer order
    { "rows": 1000, "spill_bytes": 0 }
  ],
  "peak_rss_mib": 29.28,            // BOUNDARY-sampled peak (see below); null
                                    // if RSS sampling is unavailable
  "rss_sampler": {                  // #571: the CONTINUOUS background sampler
    "interval_ms": 250,             // poll interval
    "true_peak_mib": 32.1,          // max over EVERY sample of the whole run
    "phase_peaks_mib": {            // max RSS observed while each phase was
      "preflight": 12.0,            // current, one entry per phase the run
      "pass1 scan": 32.1,           // entered (key order is not meaningful)
      "assignment+budget (winner tables)": 30.4,
      "pre-pass2 (winner tables freed)": 28.9,
      "pass2 (output sink)": 29.9,
      "writer.finish": 29.28
    },
    "true_anon_peak_mib": 12.8,     // #627: max RssAnon over every sample
    "phase_anon_peaks_mib": {       // #627: max RssAnon while each phase was
      "pass1 scan": 5.0,            // current; same keys as phase_peaks_mib
      "writer.finish": 12.8
      // ...
    },
    "phase_rss_breakdown": {        // #627: /proc/self/status AT each phase
      "pass1 scan": {               // boundary, in the kernel's own KiB;
        "rss_kib": 42272,           // rss = anon + file + shmem exactly
        "anon_kib": 5116,
        "file_kib": 37156,
        "shmem_kib": 0,
        "swap_kib": 0
      }
      // ...
    },
    "phase_cgroup": {               // #627: cgroup v2 accounting AT each
      "pass1 scan": {               // boundary, in the kernel's own BYTES;
        "memory_current_bytes": …,  // {} on cgroup v1, off Linux, or where
        "memory_peak_bytes": …,     // the files are hidden (see below for
        "anon_bytes": …,            // what each field is)
        "file_bytes": …,
        "file_dirty_bytes": …,
        "file_writeback_bytes": …,
        "shmem_bytes": …
      }
      // ...
    }
  },
  "threads": 12,                    // rayon::current_num_threads()
  "in_flight": 12,                  // read batches in flight (pass 2)
  "memory_profile": "auto"          // the resolved MemoryProfile
}
```

### `pass2.sink` and `pass1_memory_preflight` (#626, #543)

These two are the memory-sizing pair, and they bracket the run.

`pass1_memory_preflight` is decided **before any data page is read**, from
footer row counts alone: `rows × 42 B` (the pass-1 feature table's worst case,
`PASS1_BYTES_PER_ROW`) against a fresh, source-attributed memory probe. A
`warned` verdict on a job that then died is the single most useful line in
this dump. `not_checked` means a
`--plan` replay, which builds no feature table.

`pass2.sink` is decided **during** pass 2, by measurement rather than model.
`bytes_per_row` averages retained bytes over all buffered output rows.
Owned batches contribute their Arrow array bytes plus a 256-byte floor
per row. Shared batches count their source rows once, including that floor,
and add each level's own geometry positions and count arrays.
See the [sink model](#pass-2-batches-sink-and-spill) for the sharing conditions. Before
#626 `auto` charged a flat 4 KiB/row for the property columns without ever
looking at them. Now, after each input batch, `auto` projects the bytes
already buffered plus the rows still to come at the rate so far; when that
passes `budget_bytes` it flushes the RAM sinks into spill files once mid-pass
and `downgraded_to_spill` records it. The check runs for the whole pass, so a
spatially sorted input with a sparse head and a dense tail is caught as the
tail is buffered — at the latest when the bytes actually buffered reach the
budget. Two known biases, both toward spilling early:
`RecordBatch::get_array_memory_size` reports allocated capacity rather than
live bytes, and it includes each array's fixed struct overhead, which a
coarse level's few-row batches spread over few rows. Both are real RAM while
the batches are held, so neither is subtracted.

See `docs/guides/scaling.md` (*Sizing the coarse job's memory*) for how to turn `bytes_per_row`
into a box size.

### `pass0` (#788)

Pass 0 stages a remote input's selected row groups to the local disk spill
before pass 1, so both passes read from disk. The record says what it did:

- `stage_secs`: wall seconds from planning to the last landed request.
- `bytes_to_stage`: the plan's footprint, the compressed size of every
  selected row group's chunks of the columns the conversion reads, summed
  over the remote parts. The `#272` spill-space preflight compares this
  figure against free space.
- `requests`, `bytes_fetched`: range requests that landed and the bytes
  they returned. Staging never fetches an excluded column or a pruned row
  group, so with no earlier staging `bytes_fetched` equals `bytes_to_stage`
  (a coalescing window can add the gap bytes between merged chunks).
- `max_in_flight`: the most requests in flight at once, across all parts.
  It is capped by `TYLERTOO_STAGING_CONCURRENCY` and by the memory budget.
- `failed_requests`, `unstaged_bytes`: a failed request is retried once,
  split into its row groups. These count the requests that failed twice and
  the chunk bytes they kept out of the spill. The passes read those chunks
  over the network, one request per chunk.

`null` means a local input, or a staging that stopped on an error no retry
fixes (the object changed or is unreadable); the run's warnings say which.

Once staging has landed every chunk a pass reads, the pass reads the spill
with as many reader threads as a local file gets (#544). The log shows the
`[convert] read: N reader thread(s)` line instead of "remote input not fully
staged". To check that the spill keeps up, convert a local copy of the
same file and compare the two records. Each pass's `phase_walls` entry and
`pass2.windows.mean_window` should match within noise. A remote pass that
is slower, or a lower `mean_window`, means the readers queue on the spill's
lock, which a `perf` profile of the run confirms.

### `peak_rss_mib` vs `rss_sampler` (#571)

`peak_rss_mib` only ever samples RSS **at the five phase boundaries** above
(`log_phase_rss`, unchanged since #295): the max of those five snapshots. A
peak that lives and dies entirely **inside** one phase is invisible to it. On
a 1.5M-line fixture the pass-1 line-coalescing buffer (#449) reached 3.8 GiB
and released it again 74 seconds later, all inside "pass1 scan" — `peak_rss_mib`
(and the `[rss] convert peak` log line) reported a number ~3 GiB below the
true peak.

`rss_sampler` fixes this with a background thread that polls RSS every
`interval_ms` (250 ms) whenever profiling is on, and is not spawned at all
otherwise. Besides its own ticks it also records one sample at every phase
change (credited to the phase that is ending), one when it stops, and every
boundary sample behind `peak_rss_mib`. It reports:

- `true_peak_mib`: the max of every sample above, across the whole run. It
  is never below `peak_rss_mib`, and exceeds it when a peak lived and died
  between two boundaries: the gap #571 exists to surface.
- `phase_peaks_mib`: the max RSS observed while each named phase was
  current, one entry for every phase the run entered, however short. The
  phases are `preflight` (options, schema, and input checks), then
  `pass1 scan` and `assignment+budget (winner tables)`, or `plan load` in
  their place on a `--plan` run, then `pre-pass2 (winner tables freed)`,
  `pass2 (output sink)` and `writer.finish`. The object's key order carries
  no meaning; read the order from this list. A remote input adds
  `pass0 staging` inside `preflight`: the run enters it after the checks
  and returns to `preflight` for the in-memory load that follows.

A spike shorter than `interval_ms` that falls between two samples is still
missed, so `true_peak_mib` is a lower bound on the real high-water mark,
tighter than `peak_rss_mib`. Use it for sizing line-heavy or long-running
jobs (#543's preflight guidance and the memory-envelope docs).
`peak_rss_mib` stays for existing consumers of the field. `rss_sampler` is
`null` only when profiling was off, which cannot happen for a line this
dump actually wrote (the whole dump is a no-op then).

### `rss_sampler`: anonymous memory and the cgroup's view (#627)

Every RSS figure above is a single total. It says how big the resident set
got, never what it was made of, and it is not what a batch scheduler
reports: under cgroup v2, Slurm's `MaxRSS` is the cgroup's charge, which also
counts page cache that is in no process's resident set. So a job whose
`MaxRSS` sits on the cgroup ceiling cannot be told apart from one about to be
OOM-killed. Four more fields fill that gap. Their numbers in the example
above come from a real `tiles --profile bounded` run over the `sharding-grid`
fixture (the older fields in the example are from a different run). That run
was on a cgroup v1 host, where `phase_cgroup` is `{}`, so the example shows
`phase_cgroup`'s keys without values.

- **`true_anon_peak_mib` / `phase_anon_peaks_mib` are the numbers to size a
  job from.** They are `RssAnon` from `/proc/self/status`: the process's
  heap, stacks and anonymous mappings. This is the memory `--profile` bounds.
  They are *sampled*, on the same 250 ms tick as `phase_peaks_mib`, and the
  kernel keeps the figure as a counter, so a read costs the same at any
  process size. Like `true_peak_mib`, they are a lower bound: a spike shorter
  than a tick can fall between samples. `true_anon_peak_mib` is `null` off
  Linux, where both objects are empty.
- **`phase_rss_breakdown` is a snapshot at each phase boundary**, credited to
  the phase that is *ending*, so `["pass1 scan"]` is the resident set as
  pass 1 handed off. `finish` closes the last phase, so `writer.finish` and
  `finalize` get an entry too. It is the composition at one moment, not a
  peak, and it can sit well below the phase's peak: in that run's export
  line, `levels` ended at 62.6 MiB anonymous after peaking at 93.4 MiB. Do
  not size from it. The fields are the kernel's `VmRSS`, `RssAnon`,
  `RssFile`, `RssShmem` and `VmSwap` lines verbatim, and `rss_kib` is exactly
  `anon_kib + file_kib + shmem_kib`.
  - `file_kib` is resident pages of *mapped* files. tylertoo maps no data
    file (its reads and writes all go through `read`/`write`), so this is
    the binary and its shared libraries: tens of MiB, growing as code pages
    are first touched and then levelling off. In that run it went from
    31 MiB at convert's `preflight` to 46 MiB at export's `finalize`. It does not grow with the
    dataset, and it is **not** the page cache left behind by reads and
    writes. Cache for an unmapped file is charged to the cgroup but is in no
    process's RSS, which is why `phase_cgroup` exists.
  - `swap_kib` non-zero means the run was already over its memory budget.
- **`phase_cgroup` is what the cgroup is charged for**, at each phase
  boundary, read from the process's own cgroup v2 directory
  (`/sys/fs/cgroup<path>`, `<path>` from the `0::` line of
  `/proc/self/cgroup`). Values are bytes, as the kernel prints them.
  - `memory_current_bytes` (`memory.current`) is everything charged right
    now, page cache included. `memory_peak_bytes` (`memory.peak`, kernel
    5.19+) is its high-water mark since the cgroup was created. Under cgroup
    v2, these are what Slurm's `jobacct_gather/cgroup` reports as `MaxRSS`.
  - `anon_bytes` is the cgroup's anonymous memory, the cgroup-wide
    counterpart of `RssAnon`.
  - `file_bytes` is page cache, tmpfs included. Clean cache can be
    reclaimed under pressure. Two parts of it cannot be reclaimed straight
    away: `file_dirty_bytes` and `file_writeback_bytes` have to be written
    back first, and `shmem_bytes` (tmpfs and shared memory) cannot be
    reclaimed at all without swap. Spill files or the intermediate overview
    in a tmpfs `TMPDIR` show up here.

  The object is empty on cgroup v1 or hybrid hosts, off Linux, in the host's
  root cgroup (which has no `memory.current`), or wherever the files are
  hidden. A file the kernel does not provide (`memory.peak` before 5.19, say)
  is present-and-null, never `0`.

Reading `memory.stat` makes the kernel flush per-CPU statistics, so
`phase_cgroup` is taken only at phase boundaries, not on every tick.
`docs/guides/scaling.md` (*Reading a job's MaxRSS*)
walks an operator through putting these fields together.

The four `phase_walls` phases are **disjoint** windows of one conversion, so
`pass1 + assign + pass2 + writer_finish <= total` always holds; the
remainder of `total` is the preflight, the writer setup and the closing
report sums. Until #533 each phase carried its *start* instant to the end of
the run and elapsed it there, so `pass1` silently covered the assignment,
all of pass 2 and `writer.finish()` (≈ `total`), the four over-counted
`total` by ~2×, `pass1.rows_per_sec` was wrong by the same factor, and the
assignment — the single largest phase on a planet-scale run — had no entry
at all. `crates/cli/tests/profile_json_dump.rs::profile_json_written_and_parses`
asserts the partition.

> **`pass2.stage_secs.read` changed meaning in #536.** It is now the *summed
> core-seconds of the reader worker threads* (`--read-workers`, #494), not the
> time one consumer thread observed itself waiting for batches. With several
> workers decoding at once the number goes UP while the pass gets faster, so
> comparing a dump from before #536 against one from after will read a
> ~+35% `read` as a regression when it is the opposite. Compare
> `phase_walls.pass2` across such a pair instead, and treat `stage_secs.read`
> as a core-seconds figure only. Two costs sit outside that window on purpose:
> the in-order merge's splice at a worker seam, and opening each segment's
> reader — the latter is now a bare `File::open` over already-parsed footer
> metadata (#536 stopped re-parsing the footer per segment, ~5.5 s of CPU on
> the Brazil fixture), so it is negligible rather than merely uncounted.
> `pass2.stage_secs.spill_write` likewise moved off the consumer thread onto
> each level's spill writer and is folded in when that thread is joined.

> **`pass2.stage_secs.{decode,simplify,build}` grew in the #494 window
> change, and that is not a regression.** Each stage times one batch's
> section of work, and since #494 the consumers run several batches
> concurrently (`pass2.windows`). A batch's window now also contains the time
> its parallel sections waited for pool threads busy with the other batches.
> The sums therefore rise while `phase_walls.pass2` falls; on FTW Moldova
> `simplify` went from 1 s to 4 s as pass 2 went from 3.9 s to 2.7 s.
> Compare `phase_walls.pass2` across the change, not the stage sums.

`pass1.stage_secs` and `pass2.stage_secs` are independent stage splits —
they do not need to sum to their phase's `rows_per_sec` denominator or to
`phase_walls`, since stages overlap across a producer/consumer pipeline
(read/decode/simplify/build run on a reader thread while the writer thread
drains finished batches). A stage split that sums to (near) zero while
`rows` and `phase_walls` are non-zero is a bug, not a fast run — that
inconsistency is exactly what shipped as #517 (the finest level's own
stage timers were dropped) and is now covered by
`crates/cli/tests/profile_json_dump.rs::profile_json_written_and_parses`.

### Early warning

An unwritable path (typo, missing directory, read-only mount) is probed
**at conversion start** — with the other convert path preflights, in
`validate_options` — not only when the dump is finally appended at the
end of the run: a bad path logs an unmistakable `log::warn` (`NOT WRITABLE`)
immediately, so a multi-hour batch/sweep run doesn't silently lose its
profiling data while still exiting `0`. It is deliberately **not**
fail-fast: like the dump itself it never fails the conversion, it only makes
the failure mode loud and early instead of quiet and late (#517). The probe
is read-only as far as the target is concerned — an existing file is opened
for append, a missing one is checked through a uniquely named sibling that
is removed again — so a preflight never leaves a zero-byte dump behind for
a run that wrote no line.

### What the dump does not contain

The `[profile]` debug log carries a few per-level numbers the JSON only
has as totals (for example each streamed level's `writer_busy`). Where the
log and the dump disagree in shape like this, the log is the finer-grained
view and the JSON is the stable, parseable one; only the JSON is covered by
tests.

### Saturation: CPU, cores and I/O per phase (#810)

Both lines' `rss_sampler` objects (convert's, and `export.rss_sampler`)
also say how busy the machine was in each phase. The sampler reads
`getrusage(RUSAGE_SELF)` and, on Linux, `/proc/self/io` at every phase
boundary, so the deltas are exact even for a phase shorter than the 250 ms
tick:

```jsonc
"rss_sampler": {
  "phase_order": ["preflight", "pass1 scan", ...],  // run order: `phases` is key-sorted
  "phases": {
    "pass2 (output sink)": {
      "wall_secs": 5.15,              // this sampler phase's own wall window
      "cpu_user_secs": 70.1,          // all threads; null off Unix
      "cpu_sys_secs": 4.6,
      "effective_cores": 14.5,        // (user + sys) / wall
      "utilization": 0.23,            // effective_cores / `threads`
      "stage_secs": 5.2,              // the stage timers credited to this phase
      "unattributed_core_secs": 69.5, // cpu - stage_secs
      "read_bytes": 0,                // block-layer bytes; null off Linux
      "write_bytes": 8024064
    }
  },
  // one row per 250 ms tick:
  // [t_secs, cpu_secs, rss_bytes, read_bytes, write_bytes, rchar]
  "timeline": [[0.0, 0.01, 23855104, 0, 0, 1048576], ...]
}
```

How to read them:

- **`effective_cores`** is the number of cores the phase kept busy. Compare
  it with `threads`: `utilization` near 1 means the phase is CPU-bound and
  parallel; a phase at ~1 core on a 64-thread run is serial, whatever its
  stage timers say. This is the number the effective-cores gate watches
  (see "The effective-cores gate" below); serializing work (a 1-thread
  pool) drops it (`effective_cores_drop_when_work_is_serialized`).
- **`stage_secs`** is credited only where stage timers exist: `pass1 scan`
  gets the sum of `pass1.stage_secs`, `pass2 (output sink)` the sum of
  `pass2.stage_secs` plus `writer_busy_secs`, and each export phase the
  export stages that ran during it (`fill_encode` is inside `encode` and
  `group_commit_wait` is waiting, so neither counts). Other phases have 0.
- **`unattributed_core_secs`** is CPU the stage timers do not explain:
  untimed work, rayon workers spinning before they sleep, allocator and
  scheduler cost. A large share (93% of pass 2 on a debug Moldova run) says
  the stage split is not where the cores go; profile it with samply. It can
  go slightly negative on a loaded host, because a stage window is wall time
  on its thread and counts time the thread was descheduled.
- **`read_bytes`/`write_bytes`** are what hit the block layer; a warm page
  cache reads show only in the timeline's `rchar`.
- **`timeline`** is for plotting cores over time (`Δcpu / Δt` between rows)
  next to RSS. It keeps at most 8192 rows: when full it drops every other
  row and samples half as often, so a long run costs bounded memory.

Serial-path attribution:

- **`pass2.writer_busy_secs`**: the streamed finest level's writer thread,
  busy rather than waiting on the producer (`total - recv_wait`).
- **`export.serial_secs`** `{spool_write, finalize, group_commit_wait}`:
  the single-threaded tile append, `writer.finalize`, and the time threads
  waited to lock a concurrent level group's ordered write (#647).
  **`export.serial_share`** is `(spool_write + finalize) / total`, the
  critical-path share the nightly guard bounds at `floors.serial_share_max`
  (10%) on the pinned host class.
- **`export.per_zoom[].wave_width`** is the level's actual partitions per
  wave after the #311 memory narrowing (not `partition_wave_width`, the
  ceiling), and **`spill_bytes`** the level's members the member store
  wrote to disk (0 in RAM).

#### The effective-cores gate (#817)

`benchmarks/valve_guard.py` (the nightly Valve guard) runs every cell with a
fresh `TYLERTOO_PROFILE_JSON` and records, per cell, the median whole-run
`effective_cores`, the `convert/pass2 (output sink)` and `export/levels`
phase cores (`moldova-tiles` runs both phases) and the export `serial_share`.
Two checks, both exit 1:

- **Drop** (part of the `baseline` line): on a host class with a baseline
  entry, a phase median more than 15% (`CORES_DROP_MAX`) below the baseline's
  fails. Effective cores alone is a weak signal (export levels shows 29-31
  cores at 64 threads with a 16-to-64 wall speedup of about 1, because rayon
  spin counts as busy), so the guard also runs `moldova-tiles` and `792-def`
  (`SCALING_CELLS`) at `RAYON_NUM_THREADS=16` and fails when
  `speedup_16_to_full` (wall at 16 threads over wall at full threads; whole
  run, pass 2 on `moldova-tiles`, levels on both) falls more
  than 15% below the baseline's (`SPEEDUP_DROP_MAX`), or a phase's CPU
  core-seconds (`phase_cpu_s`) rise more than 25% (`CPU_RISE_MAX`) over the
  baseline's. Core-seconds are compared only when the output sha256 equals
  the baseline's. Another host class prints `baseline: NOT APPLICABLE`.
  The 16-thread arm adds one run per round per scaling cell: `792-def`
  takes 9.7 s at 64 threads on the pinned host, so at 16 threads expect
  roughly 20-40 s per round, about 1-2 minutes over the default 3 rounds,
  plus the quiet-host waits.
- **Floors** (the `floors` line): only a baseline entry with a `floors`
  section is checked. `whole_cores_frac` x CPUs is the minimum whole-run
  effective cores and `serial_share_max` (0.10) the maximum serial share.
  The floors are data in `benchmarks/baseline/valve_guard.json` under the host
  class, so tighten them there as #813 lands; `record` seeds
  `{"whole_cores_frac": 0.05, "serial_share_max": 0.10}` into a class with
  none and keeps an existing section. A cell is exempt only through a
  `[cores_floor.<cell>]` entry (reason, issue, measured) in
  `ci/exemptions.toml`, with the count bumped in
  `scripts/ci/tests/test_exemptions.py`.

The guard sets `RAYON_NUM_THREADS` itself for every run (16 for the scaling
arm, removed otherwise); `record` and `check` refuse to start when the parent
environment sets it. `check --inject-threads N` is the one supported way to
pin the full arm, for the failure demo; the verdict and `--json-out` say it was
injected, and `record --inject-threads` is refused. A phase the cell measures
(pass 2 and levels on `tiles`, levels on export; both thread arms on a scaling
cell) missing from any round fails the check and makes `record` refuse to
write. A matched baseline missing a field the gate compares (recorded before
these fields), a cell, or `floors` fails on the pinned class (`PINNED_CLASS`,
aarch64/0xd40/64, the `tylertoo-bench` runner) with the missing fields and the
`record` command; on another recorded class it reports
`baseline: NOT APPLICABLE (... run record)`. `record --cells X` merges into the host class entry (other
cells and `floors` stay). A CPU core-seconds check that cannot run (baseline
under 0.05 s, or the output changed) prints a `SKIP` line.

On the pinned 64-core host (build both binaries first, see valve_guard.py):

```bash
G="uv run --no-project --python 3.12 python -I benchmarks/valve_guard.py"
A="--mim target/release/tylertoo --glibc target/glibc/release/tylertoo --data ~/bench-data"
$G record $A                                     # (i) baseline, seeds floors
$G check  $A                                     # (ii) must pass
$G check  $A --cells moldova-tiles --inject-threads 1   # (iii) demo: must fail, labelled INJECTED
```

All of this is collected only while `TYLERTOO_PROFILE_JSON` is set, and it
never changes output bytes.

### Export phase (#535)

`export::export_pmtiles` (the `export-pmtiles` CLI subcommand, the second
half of `tiles`, and the Python bindings) appends its own line to the same
`TYLERTOO_PROFILE_JSON` file, with the same best-effort contract: an
unset/blank var is a no-op, an open/write error is only logged, and it can
never fail an export or change its output bytes. Only a **successful**
export writes a line. An export that errors out appends nothing, so a
missing export line after a failed run is expected, not a profiling bug.

```jsonc
{
  "timestamp": 1790403985.86,
  "run_id": "48213-1790248630112345678", // same as this process's convert line
  "phase": "export",
  "output": "out.pmtiles",         // the archive written
  "layer": "features",             // the MVT layer name
  "export": {
    "mode": "partitioning",        // the overview file's mode, or "duplicating"
    "phase_walls": {               // DISJOINT WALL-clock windows, run order
      "scan": 0.041,               // level scan + range restriction + planning
      "fill": 1.92,                // partitioning single-read fill; 0.0 in
                                   // duplicating mode (see below)
      "levels": 2.87,              // every level's wave loop + checkpoints
      "finalize": 0.08,            // writer.finalize()
      "total": 4.95                // the whole export
    },
    "stage_secs": {                // CORE-SECONDS (see the callout below)
      "band_read": 0.62,
      "decode": 0.91,
      "clip": 1.55,
      "encode": 2.23,
      "spill_write": 0.0,          // partitioning mode only; 0.0 otherwise
      "spill_read": 0.0,           // partitioning mode only; 0.0 otherwise
      "spool_write": 0.0028,
      "checkpoint": 0.0,
      "scan_read": 0.011,          // the level scan's split (#730); see
      "scan_decode": 0.009,        // "Stages" below
      "scan_size": 0.017
    },
    "per_zoom": [                  // one entry per exported zoom, coarse -> fine
      { "zoom": 1, "wall_secs": 0.012, "tiles": 1, "features": 40, "bytes": 4372 },
      { "zoom": 10, "wall_secs": 2.41, "tiles": 524, "features": 26839, "bytes": 4049886 }
      // ...
    ],
    "waves_total": 10,             // sum of every level's wave-loop iterations
    "partition_wave_width": 12,    // the resolved partition-wave CEILING
    "checkpoints": 0,              // writer.checkpoint() calls, NOT finalize
    "peak_rss_mib": 812.4,         // BOUNDARY-sampled peak (see below); null if unavailable
    "rss_sampler": {                // #571: the CONTINUOUS background sampler
      "interval_ms": 250,
      "true_peak_mib": 940.7,      // max over EVERY sample of the export
      "phase_peaks_mib": {         // max RSS while each phase was current
        "scan": 210.5,
        "fill": 940.7,             // absent in duplicating mode (no fill)
        "levels": 880.2,
        "finalize": 815.0
      },
      "true_anon_peak_mib": 93.4,  // #627: same shape and keys as the
      "phase_anon_peaks_mib": { /* ... */ },  // convert side
      "phase_rss_breakdown": { /* ... */ },
      "phase_cgroup": { /* ... */ }
    },
    "threads": 12                  // rayon::current_num_threads()
  }
}
```

**Phase walls.** The four `export.phase_walls` phases are disjoint wall-clock
windows, so `scan + fill + levels + finalize <= total` always holds. The
remainder of `total` is the open/validation/writer setup before the scan.
`fill` is the partitioning-mode single-read fill (`fill_member_store`, #235),
which reads, decodes, and clips every band up front. In duplicating mode
there is no fill (`fill` is exactly 0.0) and the same work happens per wave
inside `levels`. Use `export.mode` to tell which case you are looking at.

**Stages** (`crates/core/src/overview/export.rs`). Each timer's `Instant`
window is taken inside the innermost per-item closure (one geometry, one
tile), never around a whole parallel section:

- `band_read`: reading overview rows. It covers opening the Parquet
  reader (footer/page-index setup, once per band in partitioning mode and
  once per wave in duplicating mode) plus every
  `ParquetRecordBatchReader::next()` call (read + Arrow decode). In both
  modes it is charged on a dedicated producer thread that reads ahead of
  the decode/clip consumer: the fill's producer in `fill_member_store`
  (partitioning), or each wave's producer in `process_wave` (duplicating,
  #535). The producer's blocking `tx.send` is excluded. The level scan's
  own read is `scan_read`, not `band_read`.
- `decode`: everything per batch around the clip that is not the clip. That
  means the GeoArrow → `geo::Geometry` decode, the EPSG:3857 → 4326
  reprojection (timed per geometry), property extraction, and per-row member
  materialization and routing into partition buckets or the `MemberStore`.
  A spill flush triggered while routing is subtracted out and charged to
  `spill_write`.
- `clip`: `feature_tile_members` (the recursive quadtree clip cascade),
  timed per geometry inside its `par_iter`.
- `encode`: per-tile MVT encode (including the oversized-tile valve),
  content hash and gzip, timed per tile inside `encode_members`. The
  per-tile member sort before it is not counted.
- `spill_write`: partitioning mode only. Serializing and writing
  `MemberStore` spill segments during the fill and its final flush.
- `spill_read`: partitioning mode only. `MemberStore::take_wave`, which
  reads and decodes a wave's spilled segments back (~0 under RAM backing).
- `spool_write`: the serial `writer.add_tile_precompressed` calls in
  `export_level`'s per-wave write loop.
- `checkpoint`: `writer.checkpoint(...)` calls (the throttled #229/#459
  salvage snapshots). The final `writer.finalize(...)` is not included; it
  is `phase_walls.finalize`. This stage is **legitimately 0.0** on any export
  that finishes before `CHECKPOINT_INTERVAL` elapses, and `checkpoints` (the
  call count) says the same thing as an integer.
- `scan_read`, `scan_decode`, `scan_size` (#730): the level scan
  (`scan_all_levels`) inside `phase_walls.scan`. `scan_read` is its
  producer thread's reader opens and `next()` calls, like `band_read`.
  `scan_decode` is getting each batch's per-feature bboxes, from the bbox
  covering or, without one, by decoding the geometry. `scan_size` is
  `size_bboxes`, which turns those bboxes into per-tile member counts.
  **These two are wall windows on the scan's single consumer thread, not
  core-seconds**: the rayon-parallel work inside one counts once. The read
  overlaps the other two, as `band_read` overlaps `decode`.

> **`export.stage_secs` is CORE-SECONDS, the same convention as
> `pass2.stage_secs`.** Each stage is the sum of per-item windows across all
> threads, so `decode`, `clip` and `encode` routinely EXCEED the wall time
> they ran in under parallelism. That is what the parallelism buys, not a
> bug. Because windows are per item, nested `par_iter`s and work stealing
> never double-count. Time outside every window (the member sort, channel
> waits, rayon scheduling) is uncounted rather than mis-attributed, so the
> stages are not expected to sum to any `phase_walls` entry.
>
> **Where stages overlap in wall time:** in both modes the producer
> thread's `band_read` runs concurrently with the consumer's
> `decode`/`clip` — in the partitioning-mode fill, and in every
> duplicating-mode wave (#535). The consumer's time parked in `rx.iter()`
> waiting for the next batch is uncounted. So `band_read + decode + clip`
> can exceed `phase_walls.fill` (partitioning) or the `per_zoom[].wall_secs`
> / `phase_walls.levels` it ran in (duplicating) even before parallelism is
> counted, and a small `band_read` next to a large `fill` or level wall
> means the work was CPU-bound, not I/O-bound. The level scan overlaps its
> read the same way. The per-wave `[profile]` debug log's `collect=` value
> is the wave's read + decode + clip wall, so it includes the consumer's
> wait on the producer.

`export.per_zoom[].wall_secs` is the wall time of that zoom's wave loop in
`export_level`. It **excludes the partitioning-mode fill** (which does the
band reads, decode, and clip for every level up front, in
`phase_walls.fill`) and the checkpoint after the level. In duplicating mode
it includes the per-wave reads and clip.

A zoom in a concurrent level group (#647) reports the time from the start
of its own collect to the end of its own write. This includes any wait for
a coarser zoom of the group to be written first. The zooms of a group
overlap, so their `wall_secs` values do **not** add up to
`phase_walls.levels`. The debug `[profile]` line for such a zoom gives its
own work as `clip+encode+gzip` (read, decode, clip, encode and gzip), without
the write or the wait.

`export.per_zoom[].bytes` is the **compressed** (gzipped) tile bytes the zoom
produced, summed over every tile handed to the writer and counted **before
the writer's content-hash dedup**. A duplicated tile (for example, an all-ocean tile)
counts every time it is produced but is stored once, so the per-zoom sums
can exceed the archive's size. Raw pre-gzip MVT sizes do not enter this
figure.

`export.peak_rss_mib` is the peak of RSS samples taken at the export's phase
boundaries: after the scan, after the fill, after each level, and at
finalize. Because it only samples those points, it can miss the true
high-water mark; it is `null` where the platform cannot report RSS.

`export.rss_sampler` (#571) is the continuous background sampler's report,
the export-side counterpart of convert's `rss_sampler` (see that section
above for how it differs from `peak_rss_mib`). Its thread runs from the start
of the level scan through `finalize`, and it reports `true_peak_mib` plus
`phase_peaks_mib` keyed by `scan` / `fill` / `levels` / `finalize`, the same
names as `export.phase_walls`. In duplicating mode the fill never runs, so
`phase_peaks_mib` has no `fill` key. It is spawned only when profiling is
on; `null` otherwise (which cannot happen for a line this dump actually
wrote). It carries the #627 fields too (`true_anon_peak_mib`,
`phase_anon_peaks_mib`, `phase_rss_breakdown`, `phase_cgroup`), with the same
shape, keys and platform caveats as the convert side. See
[`rss_sampler`: anonymous memory and the cgroup's view](#rss_sampler-anonymous-memory-and-the-cgroups-view-627).
`finalize` is the phase an export's `MaxRSS` most often lands in, and the
sampler's closing snapshot is what gives it an entry.

`export.partition_wave_width` is the resolved partition-wave **ceiling**
(`resolve_and_log_partition_wave`'s return, `auto` or an explicit
`--partition-wave`), a single scalar for the whole export.
`memory_safe_level_wave` (#311) can narrow a level's actual wave below it
based on that level's density. The dump does not carry that per-level detail
yet.

`crates/cli/tests/profile_json_dump.rs` covers this section:

- `profile_json_written_and_parses` runs `tiles` (duplicating mode). It
  asserts that both lines share one `run_id` and have the right `phase`, and
  that `per_zoom` matches `--report`'s zooms, tile totals and feature totals.
  It also checks that `band_read`, `decode`, `clip`, `encode` and
  `spool_write` are each positive and that the phase walls fit inside
  `total`.
- `profile_json_export_line_partitioning_mode` runs `overview --mode
  partitioning` followed by `export-pmtiles` as separate processes. It
  asserts distinct `run_id`s, `mode: "partitioning"`, a positive
  `phase_walls.fill`, and positive stages.

#### Two JSONL lines for one `tiles` run

A one-shot `tiles` conversion appends **two** JSON lines to
`TYLERTOO_PROFILE_JSON`: convert's (`"phase": "convert"`, the object
documented above) followed by export's (`"phase": "export"`). Both carry the
same `run_id` (`"<pid>-<unix nanos>"`, fixed once per process), which is how
a consumer pairs them. Group lines by `run_id` and split them by `phase`.
Separate processes get separate `run_id`s, so lines from concurrent runs
appending to one file can still be told apart when their timestamps
interleave. Each line is written with a single append-mode `write_all`, so
concurrent writers do not tear each other's lines.

Two lines are a deliberate choice. Convert's line is written the moment
`convert_streaming_strategy` returns, before `tiles` even calls
`export_pmtiles`, because convert has no idea export is coming next.
Merging the two into one object would mean either:

- (a) delaying convert's write until export finishes, which means threading
  export's completion back into `convert_to_overviews`. That is a layering
  violation.
- (b) having the `tiles` CLI facade hold both halves and write once. That
  moves the write out of the library and into the CLI, which is backwards
  for a library-first design, since `export-pmtiles` and the Python bindings
  call `export_pmtiles` directly.

A standalone `export-pmtiles` run writes exactly one `"phase": "export"`
line, the same shape as the second line of a `tiles` run.

## Sampling profilers

The phase timers say which phase is slow; a sampling profiler says which
functions inside it. `[profile.release]` strips symbols, so a release
binary profiles as bare addresses. Profile the `profiling` build instead:
release code generation with debug info and symbols kept.

```bash
cargo build --profile profiling --package tylertoo
# binary: target/profiling/tylertoo
```

Expect simplification (RDP and ring validation) to dominate convert on
polygon-heavy data, and clipping and encoding to dominate export. Both
are rayon-parallel, so read per-thread widths, not just the total.

### samply (start here)

[samply](https://github.com/mstange/samply) runs on Linux and macOS,
needs no root, and opens the result in the Firefox Profiler, with call
tree, flame graph, and a per-thread timeline.

```bash
cargo install --locked samply

samply record ./target/profiling/tylertoo \
  tiles input.parquet output.pmtiles --max-zoom 10
```

`samply record --save-only -o profile.json.gz …` writes the profile
without opening a browser; load it later with `samply load`. On Linux,
samply needs `kernel.perf_event_paranoid <= 1` (see the note below).

### perf (Linux detail)

`perf` gives hardware counters, annotated assembly, and scripting.

```bash
# May need: sudo sysctl kernel.perf_event_paranoid=1
perf record -g --call-graph dwarf -o perf.data \
  ./target/profiling/tylertoo tiles input.parquet output.pmtiles
perf report -i perf.data
```

`--call-graph dwarf` unwinds from the debug info, which works without
frame pointers but writes large `perf.data` files. Keep the input small.

### cargo-flamegraph

[cargo-flamegraph](https://github.com/flamegraph-rs/flamegraph) wraps
`perf` (Linux) or `dtrace` (macOS) and writes a `flamegraph.svg`:

```bash
cargo install flamegraph

cargo flamegraph --profile profiling --package tylertoo -- \
  tiles input.parquet output.pmtiles
```

### hyperfine (end-to-end wall time)

Profilers show where the time goes; [hyperfine](https://github.com/sharkdp/hyperfine)
checks whether a change moved the total. It runs warmup passes, repeats,
and reports mean, spread, and the relative difference between commands:

```bash
hyperfine --warmup 1 --runs 5 \
  './target/release/tylertoo tiles input.parquet /tmp/a.pmtiles' \
  './before/tylertoo tiles input.parquet /tmp/b.pmtiles'
```

Time the `release` build here, since that is what ships; the `profiling`
build differs only in debug info and symbols, so its numbers should
match closely.

## Memory profiling with dhat

Heap profiling is feature-gated (zero overhead in normal builds):

```bash
# Build with heap profiling enabled
cargo build --release --features dhat-heap

# Run your workload; dhat-heap.json is written on exit
./target/release/tylertoo overview input.parquet output.parquet
ls dhat-heap.json
```

(CI's "Profiling Features" job keeps this build working.)

### Analyzing results

1. Open <https://nnethercote.github.io/dh_view/dh_view.html>
2. Load `dhat-heap.json`

Key metrics:

- **Total bytes** — total heap allocation across the run
- **Peak bytes** — high-water mark (compare against `time -v` RSS)

The CLI uses mimalloc by default (#739); dhat replaces it while profiling.
RSS above dhat's peak in a mimalloc run, bounded runs included, is allocator
retention, and `--profile bounded` remains the memory escape hatch.
- **At end bytes** — still allocated at exit (potential leaks)
- **Allocation sites** — sorted by total bytes; expand call stacks

Compare before/after a change:

```bash
mv dhat-heap.json dhat-heap-before.json
./target/release/tylertoo overview input.parquet output.parquet
# diff dhat-heap-before.json vs dhat-heap.json in the viewer
```

### Limitations

- **Release builds only** — debug builds are too slow to be meaningful
- **~2–5% runtime overhead** while profiling
- **Feature-gated** — rebuild with `--features dhat-heap`

## Criterion benchmarks

Micro-benchmarks of the hot paths live in `crates/core/benches/`.
DEVELOPMENT.md, "Benchmarks", lists them. It also covers CodSpeed's
per-PR instruction-count comment and the opt-in wall-clock regression
gate.

```bash
cargo bench --package tylertoo-core --bench clipping
open target/criterion/report/index.html
```

## Reproducing the published numbers

The corpus-based storage/access/conversion benchmarks are scripted in
`benchmarks/overview/` (see its README for the run order); the corpus
itself is rebuilt from `corpus/fetch.sh` + `corpus/optimize.sh`.
