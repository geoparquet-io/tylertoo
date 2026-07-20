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

**The auto profile spills output under memory pressure.** Pass 2 accumulates
each output level's rows before writing them. The `speed` profile keeps that
buffer in RAM, `bounded` spills it to temporary Arrow IPC files, and `auto`
estimates the buffer from feature and level counts and spills when it would
exceed a fraction of available RAM. The output is byte-identical across all
three, so the choice is purely about the memory ceiling.

**Remote input stages to a local spill file.** A remote convert fetches each
column chunk it touches into a temporary file, growing to roughly one times the
touched bytes. Later passes then re-read from local disk instead of the network.
This bounds a remote run to about a single download of the data it needs, rather
than one download per pass.

**Export waves trade cores for memory.** Export processes partitions in waves,
holding one wave resident at a time. A wider wave keeps more cores busy at
proportionally more peak memory. The default preflights a budget from the core
count and available RAM, so the common case needs no tuning.

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

**`TYLERTOO_AUTO_MEM_LIMIT_BYTES`.** Overrides the available-RAM figure that
`auto` reads when it cannot probe the machine, or when you want to reserve
headroom for other work on the box.

### Overlapping read and compute

**`--in-flight-batches N|auto`.** How many read batches move through the pass-2
pipeline at once. `auto` sizes this to the core count, clamped to 4 through 16.
Raising it improves core utilization on long-pole geometries at proportionally
more peak memory, since each in-flight batch stays resident. The chosen depth
prints at pass-2 start.

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
