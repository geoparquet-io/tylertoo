# Scaling

tylertoo never holds the whole dataset in memory. One measured run used a
16-core machine with 54 GiB of RAM. It tiled Brazil's 43.9M 2025 field
predictions out of the 629.6 GiB Fields of The World (FTW) collection:

| Stage | Wall time | Peak resident memory | Output |
| --- | --- | --- | --- |
| Convert, including 40.7 GiB read remotely | 1 h 11 m 56 s | 9.6 GiB | 15-level overview |
| Export | 11 m 44 s | 1.54 GiB | 1,647,927 tiles, z0 to z14 |

[`demo/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/demo/RESULTS.md)
records the commands and method. This guide covers keeping one machine within
its memory, then splitting a build across machines, then sizing the one job a
split does not shrink. Every knob here trades memory, disk, or speed. The
PMTiles output is byte-identical for every value.

## One machine, bounded memory

### How streaming bounds memory

Convert reads the input in two passes. Pass 1 keeps a small record per
feature: bounding box, geometry kind, and sort key. The level assignment
turns those records into a winner table of about 1 byte per feature. Pass 2
reads the input again and simplifies and writes every level batch by batch.

Peak memory is one read batch plus those per-feature tables. It scales with
the largest row group and the feature count, not the file size: well under
1 GB instead of several on a 632k-polygon file. The row-group size you set
when you prepare the input with `gpio` is therefore a memory decision. The
pass-1 record costs 64 bytes per row, which reaches tens of GiB at a billion
rows. See [sizing the coarse job's memory](#sizing-the-coarse-jobs-memory).

- **`--no-streaming`** decodes the whole dataset in memory. It can be slightly
  faster on small inputs that fit in RAM, and gives up the memory bound.
- **`--read-batch-size <rows>`** sets the Arrow batch size for both passes
  (default 8192, maximum 1,048,576). Larger batches run slightly faster at
  proportionally more memory. tylertoo lowers the value, with a warning, when
  a batch would pass the ~2 GiB Arrow can decode per byte-array column. It has
  no effect with `--no-streaming`.

### Memory profiles

Pass 2 buffers each output level before writing it. `--profile speed` keeps
the buffer in RAM, `bounded` spills it to temporary Arrow
Inter-Process Communication (IPC) files, and
`auto`, the default, estimates the buffer from the feature and level counts
and spills when it would exceed a fraction of available RAM.

Available RAM is container-aware: tylertoo reads cgroup v2 `memory.max` and
`memory.high` and the cgroup v1 limit, less the non-reclaimable memory the
cgroup already holds. A job in a Slurm, Docker, or Kubernetes memory cgroup
sizes against the cgroup, not the node. `TYLERTOO_AUTO_MEM_LIMIT_BYTES`
overrides the figure, in bytes, ahead of both `MemAvailable` and the cgroup.
Use it to reserve headroom. Because it disables the cgroup check, do not
export it cluster-wide as a stand-in for the node's RAM.

Spills go to the temp directory. On many Slurm and Kubernetes nodes `/tmp` is
a tmpfs: RAM charged to the same cgroup. Spilling there saves nothing and can
trigger the kill. Point `TMPDIR` or `--spill-dir` at real disk
on those machines.

### Read concurrency

- **`--in-flight-batches N|auto`** sets how many read batches move through
  each pass at once. `auto` uses the core count, clamped to 4 through 16, and
  each pass prints its choice. More batches keep more cores busy, and each
  one stays resident. The two passes never overlap, so the cost does not
  double.
- **`--read-workers N|auto`** sets pass 2's reader threads. `auto` uses a
  quarter of the cores, at most 4. tylertoo honors an explicit value up to
  twice the core count. Remote inputs always use one, because their parts share a
  chunk cache that concurrent readers would evict, even when staged to disk.

Resident batches have three sources: the in-flight batches, each worker's
read-ahead queue, and, under `bounded`, up to three batches per level in its
spill writer. Read-ahead gets 10% of the available-RAM budget, so a small
machine gets fewer workers, and `--profile bounded` halves each queue. That
budget prices a row at about 4 KiB plus twice its measured geometry bytes, so
a wide schema with large strings costs more than modeled. To size such a run
to the last hundred MB, measure it, or pass `--read-workers 1`.

### Spill files

`--spill-dir <path>` places three spill files, and defaults to `$TMPDIR`. The
directory must exist. A missing one fails the run before any work starts.

- **Remote stage file.** A remote convert copies the column chunks it touches
  to local disk, about one times the touched bytes, and both passes read from
  there. Local inputs never stage. A free-space check warns of a shortfall,
  and a full `$TMPDIR` fails the run. Set `--spill-dir` on large remote runs.
- **`tiles` intermediate overview.** At least the input's size, with its own
  free-space check. It goes to `--spill-dir`, else `$TMPDIR`, else the output
  directory, and export deletes it when done. `--keep-overview PATH` keeps it
  at `PATH`. The PMTiles output is the same either way.
- **Export spill file.** `export-pmtiles --spill-dir` (and `tiles
  --spill-dir`) backs the export's buffered tiles on disk when they would not
  fit the memory budget.

The archive is never spilled. Export writes tiles once, straight into
`<output>.partial`, so its space comes from the output's filesystem. Every
archive is at least 16 KiB, the prefix every PMTiles client fetches first.

### Export waves

`--partition-wave N|auto` sets how many partitions export holds at once.
`auto` uses the core count, capped by how many estimated per-partition working
sets fit in a fraction of available RAM, with a floor of 6, or a cap of 16
when tylertoo cannot probe RAM. Export logs its choice at start. Pass a smaller
number on a shared machine, or a larger one to keep more cores busy.

### Interrupted exports

After each finished zoom, at most once a minute, export logs a checkpoint
naming `<output>.partial`. If a run stops with no tile written since, that
file is a complete archive of the zooms finished so far: serve it or rename
it. If export wrote tiles since, it leaves the file invalid on purpose, and
every PMTiles reader rejects it. So if the file opens, trust it. A new export to the same
output first moves an old `<output>.partial` to `<output>.partial.prev`, and
deletes that copy once it succeeds.

## When one machine is not enough

A 19.5M-polygon country converts in about an hour on a 48-core node. The
1.58-billion-feature FTW 2025 field-boundary dataset projects to one to three
days: past most cluster scheduling windows, and lost entirely if the job dies
late.

A sharded build splits the tiling into N parallel jobs on N machines plus a
merge that takes minutes. The finest, most expensive zooms build N ways at
once, each data shard needs bounded memory and disk, and a failed shard reruns
on its own.

The coarse job does not shrink. It runs pass 1 and the level assignment over
the whole input. Its pass 2 decodes the full input and discards most of it,
then generalizes and writes only the zooms below the pivot. That costs
less time and disk than a full convert and the same peak memory. With
`--no-drop` or a loose density budget, it costs nearly a full convert.

## Sharded builds

A shard is a contiguous run of tile ids at a pivot zoom plus all their
descendants. Along the PMTiles Hilbert order those descendants form an exact
id range at every deeper zoom, so shards never share a tile. Both
neighbors read and clip a feature that crosses a seam, and each emits only its
own tiles. (Splitting by `--bbox` bands instead duplicates such features in
the merged edge tiles.) One coarse job owns the zooms below the pivot.

### 1. Cut the shard plan

```bash
tylertoo shard-plan fields.parquet --shards 16 --pivot 6 -o shards.json
```

`shard-plan` reads only the Parquet footers, so it takes seconds on a planet.
It estimates rows per pivot tile from each row group's bounding box, cuts
ranges of about equal rows, and prints each range and its share. A warning
that it could not place rows means the row groups lack bounding box
statistics.
Re-sort the input with `gpio sort hilbert --add-bbox`, which also makes each
shard's read pruning work. If it warns of empty ranges, use fewer `--shards`.
Choose the pivot (default 6) so each shard holds a few tiles of data. A pivot
from z4 to z8 suits most fleets.

### 2. Run the coarse job

```bash
tylertoo tiles fields.parquet coarse.pmtiles \
    --min-zoom 0 --max-zoom 14 --no-coalesce-lines \
    --shard coarse --shard-plan shards.json \
    --save-plan convert.plan
```

The coarse job builds the zooms below the pivot and writes `convert.plan`, the
level assignment every shard replays. The assignment is dataset-wide, so it
runs once, here. Give every job the same zoom range: a shallower `--max-zoom`
changes the plan, and the shards refuse it. A `--keep-overview` file from this
job is partial, so `export-pmtiles` reads it only with a `--zoom-ceiling` at
or below its own. If no feature appears below the pivot, the job writes an
empty archive and exits 0.

**Plan only.** Sometimes another archive replaces the coarse zooms, such as
external aggregates at z0 to z8 above real polygons at z9 to z13. Cut the
shard plan at the handover zoom (`--pivot 9`), give every job the same zoom
range, and run the coarse job for its plan alone:

```bash
tylertoo tiles fields.parquet \
    --min-zoom 9 --max-zoom 13 --no-coalesce-lines \
    --shard coarse --shard-plan shards.json \
    --save-plan convert.plan \
    --plan-only
```

`--plan-only` requires `--save-plan` and refuses an OUTPUT path and `--shard
I/N`. It ignores export flags but still rejects values the full job would,
and prints a summary of the plan (`--verbose` for per-level detail). Keep
`--shard coarse --shard-plan`, or the data shards refuse the plan. It saves
pass 2 and export time, not memory.

### 3. Run the shards

```bash
tylertoo tiles fields.parquet shard-$i.pmtiles \
    --min-zoom 0 --max-zoom 14 --no-coalesce-lines \
    --shard $i/16 --shard-plan shards.json \
    --plan convert.plan
```

Each shard reads only the row groups that reach its range, replays the plan
onto them, and writes only its own tiles. A shard that owns no rows writes an
empty archive and exits 0. A data shard needs `--plan`, and fails by name
when the plan does not match:

- a different tylertoo version or thinning option
- an input part whose path, size, mtime, row count, or row-group layout
  changed
- a `shards.json` re-cut after the coarse job ran (the error names both
  digests).

### 4. Merge

```bash
tylertoo merge fields.pmtiles coarse.pmtiles shard-*.pmtiles
```

Merge copies each tile as stored, without decoding it, and verifies that the
inputs share no tile id. A shard listed twice or left from an earlier run is
an error naming both archives. Merge skips empty shards. The tile bodies are
byte-identical to a single-machine run, which
`crates/core/tests/shard_merge_parity.rs` checks tile by tile. The file is
not: its directory layout and tile order differ, and it has no tilestats.
Generate those from the merged archive if a consumer needs them.

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

- **No line coalescing.** A coalesced chain spans many row groups and would
  leave a gap at a seam. Pass `--no-coalesce-lines` to every job, the coarse
  job included. Clustering, `--accumulate-attribute`, and tiny-polygon
  carriers work.
- **`--tile-buffer` stays at or under 256 tile pixels**, the cap for every
  export, inside the two pivot tiles (512 pixels) a shard reads around its
  range.
- **CLI only.** The Python bindings do not expose sharding.
- **Hand-cut ranges.** `export-pmtiles --tile-range LO..HI` takes two tile ids
  at one zoom, and `--zoom-ceiling Z` emits the coarse half.

## Sizing the coarse job's memory

The coarse job's floor is the pass-1 feature table: 64 bytes per input row,
held from the scan through the level assignment. Sharding does not lower it,
because the coarse job scans the whole input whatever N is. The winner grids
and pass-2 buffers need memory on top.

**Budget at least rows × 64 bytes × 2.5.** A 1.58-billion-row coarse job has a
94.2 GiB floor and so needs about 235 GiB. That job ran out of memory on a
192 GiB machine and finished on 360 GiB. The remedy is a bigger machine. Only
a `--plan` replay skips pass 1, and a machine big enough for pass 1 must cut
that plan first.

Before pass 1 reads a row, convert estimates rows × 64 bytes from the footers,
in both pipelines, and compares it to the memory figure:

- It **warns** when 2.5 times the floor exceeds the figure, naming the
  figure's source: `MemAvailable`, cgroup `memory.max` or `memory.high`, or
  `TYLERTOO_AUTO_MEM_LIMIT_BYTES`. With `--bbox` or `--filter` the count is an
  upper bound, so it only warns.
- It **fails** in seconds when the floor alone exceeds a hard cgroup limit
  (`memory.max`, or v1 `memory.limit_in_bytes`), since the kernel would kill
  the job anyway.

`TYLERTOO_SKIP_MEMORY_PREFLIGHT=1` (or `true`, `yes`, `on`) turns the error
into a warning. Off Linux the check does nothing unless
`TYLERTOO_AUTO_MEM_LIMIT_BYTES` is set, and a `--plan` replay skips it.

**Line coalescing adds to this.** Coalescing is on by default. A line input
under the coalescing ceiling then also holds its line geometry through pass
1. That costs up to `--coalesce-max-level-rows` × 512 bytes, about 1.2 GiB
resident at the defaults. The chain stage then peaks at 16 to 29 times that (see the
[tuning reference](../OVERVIEW_TUNING.md)). Add it for line-heavy inputs, or
pass `--no-coalesce-lines`. Above the ceiling, tylertoo skips coalescing, so it
adds nothing.
