# Keeping memory bounded

A 40 GiB input tiles on a 54 GiB machine because tylertoo never holds the whole
dataset. In the Brazil demo, convert peaked at 9.6 GiB while reading 40.7 GiB
over the network, and export held to 1.56 GiB while streaming all fifteen
levels. This topic explains the streaming model those numbers come from, the
two-pass structure behind it, and the knobs that trade memory against speed when
a file outgrows RAM or a run gets tight.

## Design decisions

**Peak memory tracks the largest row group.** The streaming reader holds one
read batch plus a set of compact per-feature tables, never the decoded dataset.
Memory therefore scales with the largest row group and the feature count, not
the file size, which is why a multi-gigabyte input tiles in single-digit
gigabytes. It is also why row-group sizing during input preparation is a memory
decision, not just a throughput one.

**Two passes avoid holding the whole dataset.** Pass 1 scans the input to assign
each feature its levels and apply the density budget, keeping only bounding
boxes, geometry kinds, and sort keys. Pass 2 re-reads the input per level and
simplifies and writes it batch by batch. The cost is reading the input twice.
The benefit is a peak of O(read batch + winner tables) instead of O(dataset),
which on a 632k-polygon file is the difference between well under 1 GB and
several.

**Pass 1 has an O(rows) floor, checked before the scan.** The per-feature
tables are compact, not free: pass 1 keeps a 64-byte record per input row from
its scan through the level assignment, plus smaller transient per-row vectors
during the scan. That is a few tens of MB for a million rows but tens of GiB
at a billion. Before pass 1 reads a data page, convert estimates `rows × 64 B`
from the footers and compares it to the memory figure: it warns when the
realistic whole-job need (about 2.5× that floor) exceeds the figure, and fails
fast only when the floor alone exceeds a hard cgroup limit (`memory.max`, or
v1 `memory.limit_in_bytes`). See
[sizing the coarse job's memory](sharded-builds.md#sizing-the-coarse-jobs-memory)
for the numbers and the `TYLERTOO_SKIP_MEMORY_PREFLIGHT` escape hatch. The
check runs in both pipelines (`--no-streaming` too) and is skipped for a
`--plan` replay. Off Linux there is no `MemAvailable` or cgroup to read, so it
does nothing unless `TYLERTOO_AUTO_MEM_LIMIT_BYTES` is set.

**The auto profile spills output under memory pressure.** Pass 2 accumulates
each output level's rows before writing them. The `speed` profile keeps that
buffer in RAM, `bounded` spills it to temporary Arrow IPC files, and `auto`
estimates the buffer from feature and level counts and spills when it would
exceed a fraction of available RAM. That figure is container-aware: cgroup v2
(`memory.max` and `memory.high`) and v1 memory limits are respected, less the
non-reclaimable memory the cgroup is already holding, so a job inside a Slurm,
Docker or Kubernetes memory cgroup sizes against the cgroup, not the whole
node. The output is byte-identical across all three, so the choice is purely
about the memory ceiling.

One caveat for cgroup users: the bounded path spills to the process temp
directory, and on many Slurm and Kubernetes nodes `/tmp` is a tmpfs — RAM
charged to the same cgroup, so spilling there buys nothing and can itself
trigger the kill. Point `TMPDIR` (or `--spill-dir`) at real disk on those
machines.

**Export writes its tiles once, beside the output.** The PMTiles writer used to
spool tile bytes to `TMPDIR` and copy the whole spool into the archive on every
checkpoint, so a long run's salvage snapshots cost more write I/O than the tiles
themselves. Export now appends tile bytes straight into `<output>.partial` after
a reserved 16 KiB header/directory prefix and never moves them again; a
checkpoint rewrites only that prefix and re-appends the metadata and leaf
directories at the file's tail. Two practical consequences: the space the
archive needs is on the **output** filesystem, not `TMPDIR`, and every archive
has a 16 KiB floor (invisible at any real size — it is the same 16 KiB every
PMTiles client fetches on its first request).

**An interrupted export salvages from `<output>.partial`.** After each finished
zoom (throttled to once a minute) export logs a line naming that file. If the
run was interrupted with no tile written since that checkpoint, it is a
complete, readable PMTiles archive capped at the zooms finished so far — open
it, serve it, or rename it. If tiles *were* written since, those tiles have
overwritten the index sections at the file's tail, and the file is deliberately
made **invalid**: every PMTiles reader will reject it. That is the guarantee
worth knowing — the file is never quietly wrong, so if it opens, trust it. A
run that finishes normally renames the file over `<output>`, so it is gone.

If an earlier crashed run already left a `<output>.partial` behind, starting a
new export to the same output moves it to `<output>.partial.prev` instead of
overwriting it, so a rerun cannot destroy what you were about to salvage. A
successful run deletes that copy once the real output is in place.

**Remote input stages to a local spill file.** A remote convert fetches each
column chunk it touches into a temporary file, growing to roughly one times the
touched bytes. Later passes then re-read from local disk instead of the network.
This bounds a remote run to about a single download of the data it needs, rather
than one download per pass.

**Export waves trade cores for memory.** Export processes partitions in waves,
holding one wave resident at a time. A wider wave keeps more cores busy at
proportionally more peak memory. The default preflights a budget from the core
count and available RAM (the same container-aware probe), so the common case
needs no tuning.

## Reading a job's MaxRSS

A batch scheduler's `MaxRSS` and tylertoo's own memory bound can measure
different things, and on a big job the gap between them can be enormous. A
`--profile bounded` coarse job over 134M features, modelled at about 10 GiB
and spilling its pass-2 sinks by definition, finished inside a 192 GiB Slurm
cgroup having reported `MaxRSS 201,321,784 K`, a hair under the ceiling. It
was not killed. That is consistent with most of the charge being page cache
rather than tylertoo's own memory, but that run recorded no breakdown, so it
is an inference, not a measurement. It also only holds if the cluster runs
cgroup v2 (see below). This section explains when the two numbers disagree,
and how to tell a healthy run from one that is genuinely close to the kill.

**tylertoo does not map its data files.** Every byte it reads or writes goes
through ordinary `read`/`write` syscalls: the input Parquet and the
intermediate overview through `File::open` plus the Parquet reader, the
`bounded` profile's pass-2 spill files as Arrow IPC over a
`BufWriter`/`BufReader`, and the export's tile data appended to
`<output>.partial` and copied with `std::io::copy`. So none of that data is in
the process's address space, and none of it appears in the process's own
resident set. A profiled run confirms it: the file-backed part of the resident
set (`file_kib`, below) stays at the size of the binary.

**But the kernel still caches it, and a cgroup is charged for it.** Pages read
or written through those syscalls land in the page cache, which is charged to
the cgroup that brought them in. Whether that shows up in `MaxRSS` depends on
how Slurm gathers it:

| Slurm setup | What `MaxRSS` is | Includes page cache? |
|---|---|---|
| `jobacct_gather/cgroup` on **cgroup v2** | the cgroup's `memory.current`, or `memory.peak` where the kernel has it | **yes**, unless `JobAcctGatherParams=no_file_cache` |
| `jobacct_gather/cgroup` on **cgroup v1** | `total_rss` from the cgroup's `memory.stat`: anonymous memory plus swap cache | no |
| `jobacct_gather/linux` | `/proc/<pid>/statm` summed over the job's processes | no (only mapped pages) |

To tell which one your cluster runs: `stat -fc %T /sys/fs/cgroup` on a
compute node prints `cgroup2fs` on cgroup v2 and `tmpfs` on v1 or a hybrid
host. `scontrol show config` lists `JobAcctGatherType`,
`JobAcctGatherParams`, and the `CgroupPlugin` in use. A profiled run also
tells you: `phase_cgroup` (below) is populated only on cgroup v2.

So only on **cgroup v2 with `jobacct_gather/cgroup`** can writing a 40 GiB
archive and reading a 42 GiB input twice push `MaxRSS` to the cgroup's limit
however small the process is. There, **sizing the next job off `MaxRSS` will
over-provision it**, and the operator fix is `JobAcctGatherParams=no_file_cache`
in `slurm.conf`, which subtracts the cgroup's file cache (`active_file` and
`inactive_file`) from what Slurm reports. The man page notes that it also stops
Slurm using `memory.peak`, so very short spikes can be missed. On **cgroup v1 or
`jobacct_gather/linux`**, a `MaxRSS` at the ceiling is not page cache. It is the
job's own memory, and it deserves the same scrutiny as an OOM.

**What the cgroup's OOM killer counts.** The kill comes when the cgroup's
charge cannot be reclaimed back under its limit. Clean page cache is dropped
first, which is why a charge full of it is harmless. The rest of the charge is
not so easily recovered:

- anonymous memory: the process's heap, which `--profile` bounds;
- **tmpfs files**. Spill files or the intermediate overview written to a tmpfs
  `TMPDIR` are shared memory. They are charged to the cgroup and cannot be
  reclaimed at all without swap (the tmpfs caveat under
  [Design decisions](#design-decisions) above);
- **dirty and writeback pages**, for example the archive's most recent writes.
  These have to reach disk before they can be dropped, so a job writing faster
  than its disk absorbs them can still hit the limit;
- kernel memory charged to the cgroup (page tables, slab, socket buffers).

**Get the breakdown from the run itself.** Set `TYLERTOO_PROFILE_JSON` and the
profile records the process's anonymous memory, sampled every 250 ms, together
with, at every phase boundary, the resident set's composition and the cgroup's
own accounting:

```bash
TYLERTOO_PROFILE_JSON=profile.jsonl tylertoo tiles in.parquet out.pmtiles \
  --profile bounded --layer-name fields
jq '.rss_sampler | {true_peak_mib, true_anon_peak_mib, phase_anon_peaks_mib,
    phase_rss_breakdown, phase_cgroup}' profile.jsonl
```

This is the export line of a real run over the `sharding-grid` test fixture on
a cgroup v1 host, which is why `phase_cgroup` is empty (`phase_rss_breakdown`
is trimmed to one phase):

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

Read it like this:

| Field | What it means |
|---|---|
| `true_anon_peak_mib`, `phase_anon_peaks_mib` | The process's own heap, sampled. **Size a job from these** (or from `true_peak_mib`, which adds the few tens of MiB of the binary), plus headroom. |
| `phase_rss_breakdown` | The resident set's composition *at the end* of each phase: `anon_kib` + `file_kib` + `shmem_kib` = `rss_kib`. A snapshot, not a peak. Above, `levels` ended at 62.6 MiB anonymous after peaking at 93.4 MiB, so do not size from it. `file_kib` is the binary's code: tens of MiB, growing as code is first touched and then levelling off, and never the page cache of reads and writes. A non-zero `swap_kib` means the run was already over budget. |
| `phase_cgroup` | cgroup v2 only: `memory_current_bytes` and `memory_peak_bytes` are what Slurm reports on v2. `anon_bytes`, `file_bytes`, `file_dirty_bytes`, `file_writeback_bytes` and `shmem_bytes` say what that charge is made of. |

The diagnosis then goes like this:

1. If `true_anon_peak_mib` approaches the limit, the process itself is near the
   kill and the profile is not holding. That is a bug worth reporting with
   `profile.jsonl` attached.
2. Otherwise, on cgroup v2, look at `phase_cgroup`. If `file_bytes` makes up
   the gap between `anon_bytes` and `memory_current_bytes`, and `shmem_bytes`
   and `file_dirty_bytes` are small, the gap is reclaimable cache: the job is
   healthy, and `no_file_cache` will make Slurm say so. If `shmem_bytes` is
   large, the spill or intermediate files are on tmpfs. Point `--spill-dir` or
   `TMPDIR` at real disk. If `file_dirty_bytes` plus `file_writeback_bytes` is
   large, writes are outrunning the disk.
3. On cgroup v1 or `jobacct_gather/linux`, `MaxRSS` should already be close to
   `true_peak_mib`. If it is much higher, something besides tylertoo shares
   the job's accounting.

The process fields are Linux-only: off Linux, `phase_anon_peaks_mib` and
`phase_rss_breakdown` are empty and `true_anon_peak_mib` is `null`.
`phase_cgroup` is empty everywhere except in a cgroup v2 hierarchy whose
memory files the process can read.

!!! tip "Why tylertoo does not drop the cache itself"

    An obvious reflex is to have tylertoo call `posix_fadvise(DONTNEED)` on the
    files it finishes with, so the accounted peak reflects the true working set.
    It would buy almost nothing here. The pass-2 spill files and (absent
    `--keep-overview`) the intermediate overview are **unlinked** as soon as
    they are consumed, and unlinking frees their cache outright — an advisory
    hint cannot do better.
    The input is re-read once per level, so evicting its cache between levels
    would convert cache hits into real disk reads and slow the run down. And the
    output archive's pages are still dirty when the last write returns;
    `DONTNEED` does not drop dirty pages, so it would need a `sync_file_range`
    first, buying a slower export in exchange for a cosmetically smaller number.
    The accounting is reported rather than manipulated.

## API walkthrough

### Streaming instead of loading the dataset

**The two-pass default.** No flag turns it on; it is how convert runs. Pass 1
builds the winner tables, pass 2 writes the levels. This is the mechanism that
delivers the O(row group) memory bound.

**`--no-streaming`.** Reverts to the in-memory pipeline, which decodes the whole
dataset once. It can be marginally faster on small inputs that fit in RAM
comfortably, at the cost of the memory bound. Reach for it only when the input
is small and speed matters more than the ceiling.

**`--read-batch-size <rows>`.** The Arrow batch size for both passes, defaulting
to 8192. Larger batches amortize per-batch overhead for a little more speed at
proportionally more peak memory; smaller batches bound memory tighter. The
default keeps per-batch transients in the tens of megabytes even for
vertex-heavy polygons.

### Choosing a memory profile

**`--profile auto|speed|bounded`.** Selects how pass 2 handles buffered output,
per the profile decision above. `auto` is the default and the safe choice for
large duplicating runs, which it steers toward `bounded` rather than risking an
out-of-memory kill.

**`TYLERTOO_AUTO_MEM_LIMIT_BYTES`.** Sets the available-RAM figure `auto` sizes
against, in bytes. It takes precedence over every other term of the probe: when
it is set, neither the machine's `MemAvailable` nor the cgroup limit is
consulted. Use it to supply a figure where the probe finds none, or to reserve
headroom for other work on the box. The warning is the flip side of that
precedence: a cluster that exported this variable as a workaround for the
old, node-sized probe is now *disabling* the cgroup awareness that would
otherwise size the run correctly. Unset it and let the probe read the cgroup.
The pass-1 memory preflight reads this figure too, but only ever warns against
it: an override is a sizing knob, not an OOM limit, so it never fails a run.

### Overlapping read and compute

**`--in-flight-batches N|auto`.** How many read batches move through the
streaming pipeline at once — pass 1's scan and pass 2's per-level fan-out both
use it. `auto` sizes this to the core count, clamped to 4 through 16. Raising
it improves core utilization on long-pole geometries at proportionally more
peak memory, since each in-flight batch stays resident (per pass; passes 1 and
2 never run concurrently, so this does not double). The chosen depth prints at
the start of each pass.

It is no longer the only resident-batch term, though. Pass 2's readers hold
their own read-ahead on top of it (`--read-workers` × that worker's queue
depth), and under `bounded` each level's spill writer can hold up to three more
batches — two queued plus the one it is encoding. When you are counting
resident batches, count all three.

**`--read-workers N|auto`.** How many threads pass 2 reads the input with.
Parquet row groups are independently readable, so several threads decode
disjoint runs of them and an in-order merge puts the batches back in read order
— the output is byte-identical for every value. `auto` takes a quarter of the
cores, capped at 4; an explicit value is honoured up to 2× the machine's cores.

The read-ahead this needs (each worker buffers its run ahead of the merge) is
sized against the same available-RAM budget the pass-2 sink uses, taking 10% of
it. So a constrained box gets fewer workers rather than a bigger resident set.
Under an explicit `--profile bounded` a worker's queue is additionally capped
at half its usual maximum depth, so a machine with RAM to spare does not build
the deepest read-ahead under the profile that exists to cap memory.

That bound is a **model**, not a measurement. The budget is charged
`workers × depth × read-batch-size × per_row`, where `per_row` is the same
estimate the sink uses: a fixed ~4 KiB non-geometry term plus twice pass 1's
measured per-row geometry bytes. An input whose non-geometry columns cost much
more than that — a very wide schema, big strings, many dictionaries — is priced
too cheaply, and the read-ahead will exceed its nominal share. If you are
sizing a run to the last hundred MB on a wide schema, measure rather than trust
the fraction, and `--read-workers 1` removes the term entirely.

Remote inputs read sequentially regardless: their parts share one in-memory
chunk cache that concurrent readers would evict out from under each other.
Staging the input to local disk first does **not** change this — staging fills
that same cache and its disk spill, it does not turn a remote source into a
local one — so a remote convert's pass-2 read is always sequential. Only inputs
that were local to begin with read in parallel.

### Placing spill files

**`--spill-dir <path>`.** Where the remote-input stage file lands, and on
`tiles`, where the intermediate overview lands. Point it at a volume with room
for roughly the touched input size. A free-space preflight warns about a
projected shortfall. The directory must exist. Local inputs never spill.

**`$TMPDIR`.** The default spill location when `--spill-dir` is unset. The
caveat is that a small or full `$TMPDIR` volume turns a remote convert's stage
write into a failure, so on a big remote run, set `--spill-dir` to somewhere you
know has space.

### Sizing export waves

**`--partition-wave N|auto`.** The number of partitions export holds resident
per band. `auto` preflights a memory budget from the core count and available
RAM. Override it with an explicit integer to cap memory harder on a shared
machine, or to push utilization on a dedicated one. The output is byte-identical
for every value.
