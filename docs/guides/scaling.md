# Scaling

Streaming keeps tylertoo from holding the whole dataset in memory. One run used a
16-core machine with 54 GiB of RAM. It tiled Brazil's 43.9M 2025 field
predictions out of the 629.6 GiB Fields of The World (FTW) collection:

| Stage | Wall time | Peak resident memory | Output |
| --- | --- | --- | --- |
| Convert, including 40.7 GiB read remotely | 1 h 11 m 56 s | 9.6 GiB | 15-level overview |
| Export | 11 m 44 s | 1.54 GiB | 1,647,927 tiles, z0 to z14 |

[`demo/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/demo/RESULTS.md)
records the commands and method. The controls below trade memory, disk, and
speed without changing the PMTiles bytes. Sharding splits larger builds
across machines. The coarse job still needs memory for the full input.

## One machine, bounded memory

### How streaming bounds memory

Convert reads the input twice. Pass 1 records each feature's bounding box,
geometry kind, and sort key. Level assignment produces a winner table of
about 1 byte per feature. Pass 2 simplifies and writes each level in batches.

Peak memory is one read batch plus the per-feature tables. It depends on
the largest row group and feature count: well under 1 GB on a 632k-polygon
file that would otherwise need several GB. Choose the row-group size when
preparing input with `gpio`. Pass 1 uses up to 42 bytes per row, reaching
tens of GiB at a billion rows. See [sizing the coarse job's memory](#sizing-the-coarse-jobs-memory).

- **`--no-streaming`** decodes the whole dataset in memory. It can be slightly
  faster on small inputs that fit in RAM.
- **`--read-batch-size <rows>`** sets the Arrow batch size for both passes
  (default 8192, maximum 1,048,576). Larger batches run slightly faster at
  proportionally more memory. tylertoo lowers it, with a warning, if a batch
  would exceed Arrow's ~2 GiB limit per byte-array column. It has
  no effect with `--no-streaming`.

### Memory profiles

Pass 2 buffers each output level before writing it:

| `--profile` | Buffer storage |
| --- | --- |
| `speed` | RAM |
| `bounded` | Temporary Arrow Inter-Process Communication (IPC) files |
| `auto` (default) | Estimates memory from feature and level counts and spills above a fraction of available RAM. Measures buffered rows and switches to spilling mid-pass if they outgrow it |

tylertoo reads cgroup v2 `memory.max` and `memory.high` or the cgroup v1 limit,
subtracting memory the cgroup cannot reclaim. Slurm, Docker, and Kubernetes
jobs use this limit to size buffers. `TYLERTOO_AUTO_MEM_LIMIT_BYTES` overrides
both `MemAvailable` and the cgroup figure. Use it to reserve headroom, but
avoid setting it cluster-wide to the node's RAM: it disables the cgroup check.

Spills go to the temp directory. On many Slurm and Kubernetes nodes, `/tmp`
is tmpfs and uses RAM charged to the same cgroup. Point `TMPDIR` or
`--spill-dir` at real disk to avoid running out of memory while spilling.

### Read concurrency

- **`--in-flight-batches N|auto`** sets how many read batches move through
  each pass at once. `auto` uses the core count, clamped to 4 through 16.
  Each pass logs its choice. More batches keep more cores busy and consume
  more memory. The passes do not overlap.
- **`--read-workers N|auto`** sets pass 2's reader threads. `auto` uses a
  quarter of the cores, at most 4. tylertoo honors an explicit value up to
  twice the core count. Remote inputs always use one reader, even when staged
  to disk: concurrent readers would evict each other's shared chunk cache.

Resident batches include those in flight, each worker's read-ahead queue,
and, under `bounded`, up to three per level in spill writers. Read-ahead gets
10% of available RAM, so smaller machines get fewer workers. `bounded`
halves each queue. The estimate allows about 4 KiB per row plus twice its
measured geometry bytes. Unlike the pass-2 buffers, tylertoo sizes the read-ahead
before pass 2 reads a row, so it cannot correct itself. Wide schemas with
large strings can exceed it.
Measure memory use if headroom is tight, or use `--read-workers 1`.

### Spill files

`--spill-dir <path>` controls these temporary files and defaults to `$TMPDIR`.
The run fails before starting if the directory does not exist.

- **Remote stage file.** A remote convert copies the column chunks it touches
  to local disk, using about as much space as those chunks. Both passes read
  the staged data. Local inputs never stage. A free-space check warns of a
  shortfall, and a full `$TMPDIR` fails the run. Set `--spill-dir` on large remote runs.
- **`tiles` intermediate overview.** At least the input's size, with its own
  free-space check. It goes to `--spill-dir`, else `$TMPDIR`, else the output
  directory, and export deletes it when done. `--keep-overview PATH` retains it.
- **Export spill file.** `export-pmtiles --spill-dir` (and `tiles --spill-dir`)
  backs the export's buffered tiles on disk when they would not
  fit the memory budget.

Export writes tiles directly to `<output>.partial` on the output filesystem.
Every archive is at least 16 KiB, the prefix every PMTiles client fetches first.

### Export waves

`--partition-wave N|auto` sets how many partitions export holds at once.
`auto` uses the core count, capped by how many estimated per-partition working
sets fit in a fraction of available RAM, with a floor of 6, or a cap of 16
when tylertoo cannot probe RAM. Export logs its choice at start. Pass a smaller
number on a shared machine, or a larger one to keep more cores busy.

### Interrupted exports

After each finished zoom, at most once a minute, export logs a checkpoint
for `<output>.partial`. If the run stops before writing another tile, that
file is a valid archive of the completed zooms. You can serve or rename it.
Further tile writes invalidate it until the next checkpoint. PMTiles readers
reject the file in that state. If it opens, it is safe to use.

A new export moves an existing `<output>.partial` to `<output>.partial.prev`
and deletes that copy after succeeding.

## When one machine is not enough

A 19.5M-polygon country converts in about an hour on a 48-core node. The
1.58-billion-feature FTW 2025 field-boundary dataset would take an estimated
one to three days, exceeding most cluster scheduling windows. A late failure
loses the run.

A sharded build runs N jobs on N machines, then merges their output in
minutes. The most expensive zooms build in parallel, each shard uses bounded
memory and disk, and failed shards can rerun independently.

The coarse job scans and assigns levels across the full input. Pass 2 decodes
all rows, discards most, and writes only zooms below the pivot. It uses less
time and disk than a full convert but needs the same peak memory. With
`--no-drop` or a loose density budget, its cost approaches a full convert.

## Sharded builds

A shard owns a contiguous range of tile ids at a pivot zoom and all their
descendants. PMTiles Hilbert order keeps those descendants in an exact range
at each deeper zoom, so shards never share tiles. Neighboring shards both
read and clip features that cross their boundary, then emit only their own
tiles. Splitting by `--bbox` bands duplicates these features in merged edge
tiles. The coarse job owns zooms below the pivot.

### 1. Cut the shard plan

```bash
tylertoo shard-plan fields.parquet --shards 16 --pivot 6 -o shards.json
```

`shard-plan` reads Parquet footers and takes seconds even on planet-scale
input. It uses row-group bounding boxes to estimate rows per pivot tile,
cuts roughly equal ranges, and prints each range's share.

If it cannot place rows, the input lacks bounding box statistics. Re-sort
with `gpio sort hilbert --add-bbox` to add them and enable shard read pruning.
If it reports empty ranges, reduce `--shards`. Choose a pivot (default 6)
that gives each shard a few populated tiles. Zooms z4 to z8 suit most fleets.

### 2. Run the coarse job

```bash
tylertoo tiles fields.parquet coarse.pmtiles \
    --min-zoom 0 --max-zoom 14 --no-coalesce-lines \
    --shard coarse --shard-plan shards.json \
    --save-plan convert.plan
```

The coarse job builds zooms below the pivot and saves the dataset-wide level
assignment to `convert.plan`. Each shard replays that plan. Use the same zoom
range for every job: a lower `--max-zoom` changes the plan and shards reject it.
If no features appear below the pivot, the coarse job writes an empty archive
and exits 0.

An overview retained with `--keep-overview` contains only the coarse zooms.
To export it, set `export-pmtiles --zoom-ceiling` at or below its ceiling.

**Plan only.** Another archive can supply coarse zooms, such as aggregates
at z0 to z8 with polygons at z9 to z13. Set the pivot to the handover zoom
(`--pivot 9`). Give every job the same zoom range and generate only the plan:

```bash
tylertoo tiles fields.parquet \
    --min-zoom 9 --max-zoom 13 --no-coalesce-lines \
    --shard coarse --shard-plan shards.json \
    --save-plan convert.plan \
    --plan-only
```

`--plan-only` requires `--save-plan` and rejects an output path or `--shard I/N`.
It validates export flags but does not use them, and prints a plan summary
(`--verbose` adds per-level detail). Include `--shard coarse --shard-plan`
so data shards accept the plan. This skips pass 2 and export but needs the
same memory.

### 3. Run the shards

```bash
tylertoo tiles fields.parquet shard-$i.pmtiles \
    --min-zoom 0 --max-zoom 14 --no-coalesce-lines \
    --shard $i/16 --shard-plan shards.json \
    --plan convert.plan
```

Each shard reads row groups that intersect its range, replays the plan, and
writes its tiles. Shards with no rows write an empty archive and exit 0.
Every data shard requires `--plan` and reports mismatches for:

- a different tylertoo version or thinning option
- an input part whose path, size, mtime, row count, or row-group layout
  changed
- a `shards.json` re-cut after the coarse job ran (the error names both
  digests).

### 4. Merge

```bash
tylertoo merge fields.pmtiles coarse.pmtiles shard-*.pmtiles
```

Merge copies tiles without decoding them, skips empty archives, and rejects
duplicate tile ids, naming both archives. This catches shards listed twice
or overlapping shards from an earlier run.

Tile bodies match a single-machine run byte for byte, as checked by
`crates/core/tests/shard_merge_parity.rs`. The merged archive has a different
directory layout and tile order, and lacks tilestats. Generate tilestats
from it if a consumer needs them.

### A Slurm fleet

Submit the coarse job, then the shards as an array job that waits for it:

```bash
coarse=$(sbatch --parsable coarse.sh)
sbatch --dependency=afterok:$coarse shards.sh
```

```bash
#!/bin/bash
# shards.sh
#SBATCH --job-name=tylertoo-shard
#SBATCH --array=0-15
#SBATCH --cpus-per-task=48
#SBATCH --mem=128G
#SBATCH --time=08:00:00

tylertoo tiles "$INPUT" "$OUT/shard-${SLURM_ARRAY_TASK_ID}.pmtiles" \
    --min-zoom 0 --max-zoom 14 --no-coalesce-lines \
    --shard "${SLURM_ARRAY_TASK_ID}/16" \
    --shard-plan "$OUT/shards.json" \
    --plan "$OUT/convert.plan" \
    --profile bounded --spill-dir "$SPILL"
```

Set `SPILL` to an existing directory on real disk. To rerun shard 7, submit
`shards.sh` with `--array=7`.

### Constraints

- **No line coalescing.** Coalesced chains span row groups and can leave gaps
  at shard boundaries. Pass `--no-coalesce-lines` to every job, including the
  coarse job. tylertoo supports clustering, `--accumulate-attribute`, and
  tiny-polygon carriers.
- **`--tile-buffer` must be at most 256 tile pixels**, the cap for every
  export. This fits inside the two pivot tiles (512 pixels) read around each
  shard's range.
- **CLI only.** The Python bindings do not expose sharding.
- **Manual ranges.** `export-pmtiles --tile-range LO..HI` takes two tile ids
  at one zoom, and `--zoom-ceiling Z` emits the coarse half.

## Sizing the coarse job's memory

The coarse job holds a column-major feature table through pass 1 and level
assignment. This memory floor is independent of the shard count. Winner
grids and pass-2 buffers need additional memory.

The table's cost per input row depends on its optional columns:

| Columns | Bytes per row |
| --- | --- |
| Base: bbox center and squared diagonal (three `f64`), row index, geometry kind | 33 |
| Base plus a sort key | 41 |
| Base plus a sort key and an entry-zoom ladder | 42 |

A sort key comes from `--sort-key` or `--class-rank`. tylertoo also detects
some rankings from the schema, such as Overture road classes or places
`confidence`. Pass 1 reduces each bbox to its center and diagonal at scan
time and keeps neither the bbox nor any padding.

**Budget at least rows × 42 bytes × 3.3**, about 138 bytes per row. For 1.58
billion rows, the floor is 61.8 GiB and the budget about 204 GiB. This job ran
out of memory on a 192 GiB machine and finished on 360 GiB. A `--plan` replay
skips pass 1, but generating the plan first requires a machine with enough RAM.

That job ran when the table cost 64 bytes per row, a 94.2 GiB floor budgeted at
×2.5. #543 shrank only the table, so the multiplier rose to ×3.3 to keep the
absolute budget near its old value. The 138 bytes per row assume the smallest
saving (64 to 42). The winner grids, scan vectors, and pass-2 buffers did not
shrink. #541 and #565 had already cut the coarse job's pass-2 buffers and the density
budget's priority table, so the multiplier is conservative. Tightening it needs
a fresh billion-row run.

Before pass 1, both convert pipelines estimate rows × 42 bytes from the
footers and compare it with available memory:

- It **warns** when 3.3 times the floor exceeds the figure, naming the
  figure's source: `MemAvailable`, cgroup `memory.max` or `memory.high`, or
  `TYLERTOO_AUTO_MEM_LIMIT_BYTES`. With `--bbox` or `--filter` the count is an
  upper bound, so this check only warns.
- It **fails** in seconds when the floor alone exceeds a hard cgroup limit
  (`memory.max`, or v1 `memory.limit_in_bytes`), since the kernel would kill
  the job anyway.

`TYLERTOO_SKIP_MEMORY_PREFLIGHT=1` (or `true`, `yes`, `on`) turns the error
into a warning. Off Linux, the check only runs if you set
`TYLERTOO_AUTO_MEM_LIMIT_BYTES`. A `--plan` replay skips it.

**Wide property schemas need more.** The 3.3 multiplier assumes buffered rows
are mostly geometry. A buffered row also carries every kept property column,
so a schema with dozens or hundreds of columns can cost far more per row.
Under `auto`, pass 2 measures this cost and spills mid-pass when it would
exceed the budget (#626). To size a machine for it:

- Run once over a subset with `TYLERTOO_PROFILE_JSON` set. It reports the
  measured cost as `pass2.sink.bytes_per_row`, and the info log prints
  `pass2 sink: measured N B per buffered row`. Multiply it by the buffered
  row count the plan expects.
- Drop columns you never style on with `--include-property` or
  `--exclude-property`. tylertoo never decodes an excluded column.
- Skip pass 2 in the coarse job with `--save-plan PATH --plan-only`. It stops
  after level assignment, so property width costs it nothing. The pass-1
  floor and the preflight still apply.

The pass-1 feature table is still neither measured per schema nor spillable
(#543).

**Size from the job's own peak, not its `MaxRSS`.** The 64-byte floor that
calibrated the budget came from the process's resident set, and the multiplier
from a cgroup kill.
Both describe tylertoo's own memory. Under Slurm on cgroup v2, `MaxRSS` also
counts reclaimable page cache. A job that moves tens of GiB can report a
`MaxRSS` at the cgroup ceiling while the process is ten times smaller. Size
from `true_peak_mib` or `true_anon_peak_mib` in a `TYLERTOO_PROFILE_JSON`
capture. See [reading a job's MaxRSS](#reading-a-jobs-maxrss).

**Line coalescing needs extra memory.** tylertoo enables it by default. Below
the coalescing ceiling, line geometry stays resident through pass 1, costing up
to `--coalesce-max-level-rows` × 512 bytes, or about 1.2 GiB at the defaults.
The chain stage peaks at 16 to 29 times that amount. See the
[tuning reference](../OVERVIEW_TUNING.md). Add this to the budget for
line-heavy inputs, or use `--no-coalesce-lines`. Above the ceiling, tylertoo
skips coalescing.

## Reading a job's MaxRSS

The `MaxRSS` a scheduler reports can measure more than tylertoo's memory.
One `--profile bounded` coarse job over 134M features was modeled at about
10 GiB. It finished inside a 192 GiB Slurm cgroup with
`MaxRSS 201,321,784 K` and was not killed. Page cache likely made up most of
that charge, but the run recorded no breakdown to confirm it.

tylertoo maps none of its data files. It reads the input and the intermediate
overview, writes spill files, and writes the archive with ordinary `read` and
`write` calls. That data stays out of the process's resident set. The kernel
still caches those pages and charges them to the job's cgroup. Whether
`MaxRSS` includes that cache depends on how Slurm gathers it:

| Slurm setup | `MaxRSS` source | Includes page cache |
| --- | --- | --- |
| `jobacct_gather/cgroup`, cgroup v2 | `memory.current`, or `memory.peak` where available | Yes, unless `JobAcctGatherParams=no_file_cache` |
| `jobacct_gather/cgroup`, cgroup v1 | `total_rss` from `memory.stat` | No |
| `jobacct_gather/linux` | `/proc/<pid>/statm` summed over the job | No |

On a compute node, `stat -fc %T /sys/fs/cgroup` prints `cgroup2fs` on
cgroup v2. `scontrol show config` lists `JobAcctGatherType` and
`JobAcctGatherParams`.

On cgroup v2, a `MaxRSS` at the ceiling can be harmless cache. Setting
`JobAcctGatherParams=no_file_cache` in `slurm.conf` subtracts that cache from
the report. It also stops Slurm from reading `memory.peak`, so Slurm can miss
short spikes. On cgroup v1 or `jobacct_gather/linux`, a `MaxRSS` at the
ceiling is the job's own memory. Treat it like an out-of-memory failure.

The cgroup kills the job when it cannot reclaim enough of the charge. The
kernel drops clean page cache first. These parts are harder to reclaim:

- Anonymous memory, the heap that `--profile` bounds.
- tmpfs files. Spill files or an intermediate overview in a tmpfs `TMPDIR`
  count as shared memory, and only swap can reclaim them.
- Dirty pages and pages under write-back, such as the archive's latest writes. They must
  reach disk first.
- Kernel memory charged to the cgroup.

### Get the breakdown

With `TYLERTOO_PROFILE_JSON` set, the profile samples anonymous memory every
250 ms. At each phase boundary it also records the resident set's split and
the cgroup's own accounting:

```bash
TYLERTOO_PROFILE_JSON=profile.jsonl \
  tylertoo tiles in.parquet out.pmtiles \
  --profile bounded --layer-name fields
# convert carries rss_sampler at the top level,
# export carries it under .export
jq '(.rss_sampler // .export.rss_sampler)
    | {true_peak_mib, true_anon_peak_mib,
       phase_anon_peaks_mib, phase_rss_breakdown,
       phase_cgroup}' profile.jsonl
```

The export line of a run over the `sharding-grid` test fixture on a cgroup v1
host, with `phase_rss_breakdown` trimmed to one phase:

```json
{
  "true_peak_mib": 139.01171875,
  "true_anon_peak_mib": 93.41015625,
  "phase_anon_peaks_mib": { "finalize": 65.171875, "levels": 93.41015625, "scan": 23.796875 },
  "phase_rss_breakdown": {
    "levels": { "anon_kib": 64068, "file_kib": 46696, "rss_kib": 110764, "shmem_kib": 0, "swap_kib": 0 }
  },
  "phase_cgroup": {}
}
```

| Field | Meaning |
| --- | --- |
| `true_anon_peak_mib`, `phase_anon_peaks_mib` | The process's sampled heap peak. Size jobs from these, or from `true_peak_mib`, plus headroom. |
| `phase_rss_breakdown` | The resident set at the end of each phase, where `anon_kib` + `file_kib` + `shmem_kib` = `rss_kib`. This is a snapshot, not a peak. Above, `levels` peaked at 93.4 MiB of anonymous memory and ended at 62.6 MiB. `file_kib` is the binary's code. A nonzero `swap_kib` means the run already exceeded its budget. |
| `phase_cgroup` | cgroup v2 only. `memory_current_bytes` and `memory_peak_bytes` match what Slurm reports. `anon_bytes`, `file_bytes`, `file_dirty_bytes`, `file_writeback_bytes`, and `shmem_bytes` break that charge down. |

To diagnose a high `MaxRSS`:

1. If `true_anon_peak_mib` approaches the limit, the profile is not holding
   memory down. Report it with `profile.jsonl` attached.
2. On cgroup v2, read `phase_cgroup`. If `file_bytes` covers the gap between
   `anon_bytes` and `memory_current_bytes`, the gap is clean cache and the job
   is healthy. A large `shmem_bytes` means spill files sit on tmpfs. Point
   `--spill-dir` or `TMPDIR` at real disk. Large `file_dirty_bytes` plus
   `file_writeback_bytes` means writes outrun the disk.
3. On cgroup v1 or `jobacct_gather/linux`, `MaxRSS` should sit close to
   `true_peak_mib`. A much higher figure means another process shares the
   job's accounting.

Off Linux, `phase_anon_peaks_mib` and `phase_rss_breakdown` are empty and
`true_anon_peak_mib` is `null`. `phase_cgroup` is empty unless the process
runs in a cgroup v2 hierarchy whose memory files it can read.

tylertoo does not call `posix_fadvise(DONTNEED)` to shrink the reported
figure. Deleting the spill files and the intermediate overview after use
frees their cache outright. Pass 2 reads the input again for each level, so
evicting its cache would turn cache hits into disk reads. The archive's last
pages are still dirty, and `DONTNEED` leaves dirty pages in place.
