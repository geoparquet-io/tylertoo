# Sharded builds across a fleet

A single `tylertoo tiles` run is one process on one machine. That is the right
shape up to a surprising size — a 19.5M-polygon country converts in about an
hour on a 48-core node — but it does not survive the jump to a planet. The
2025 FTW field-boundary dataset is 1.58 billion features, and a monolithic
convert of it projects to somewhere between one and three days: past every
normal HPC scheduling window, and with nothing to show for it if the job dies
at hour 40.

Sharding splits the **tiling** into N independent jobs that run at the same
time on N machines, plus a merge measured in minutes — and makes a failed job
one `sbatch --array=7` away from fixed instead of a day thrown away. What it
does **not** do yet is make the first job cheap: read
[what sharding costs](#what-sharding-actually-buys-you) before planning a
fleet around it. This page is the recipe, the honest arithmetic, and the
reasoning behind both.

## The shape of a shard

A shard is **a contiguous run of tile ids at a pivot zoom**, and with it every
descendant of those tiles at every deeper zoom.

PMTiles orders each zoom's tiles along the Hilbert curve, and a tile's
descendants occupy an *exact* contiguous id interval at every deeper zoom — not
a bounding approximation, an interval. So N runs that partition the pivot zoom
partition every deeper zoom too, and the shards' tile sets are **disjoint by
construction**.

That is the whole point, and it is what the older workaround could not do.
Splitting by `--bbox` into longitude bands includes every feature whose bbox
*intersects* the band, so a feature straddling a boundary lands in two shards
and appears twice in the merged edge tiles. Splitting by tile id instead keeps
features whole and splits the **output**: a feature crossing a seam is read by
both neighbours, clipped normally by both, and each emits only the tiles its
own range owns. Exactly as in a monolithic run — which is not a claim, it is
[an acceptance test](#is-it-really-identical).

Zooms **coarser than the pivot** belong to no shard. One coarse job owns them.
Because tile ids ascend with zoom, its ids all sort before every shard's, so
coarse and shards merge in one step with nothing special done for either.

## The workflow

### 0. Cut the plan (seconds)

```bash
tylertoo shard-plan fields.parquet --shards 16 --pivot 6 -o shards.json
```

This reads parquet **footers only** — no data page is touched — so it takes
seconds even on a planet-scale input. It estimates rows per pivot tile from
each row group's bbox, spread over the tiles it covers and weighted by the
group's row count, then cuts 16 runs of roughly equal estimated rows.

Equal *id width* would be a terrible cut: the tile space is uniform and data
never is. The output shows what you actually got:

```
Cut 16 shard(s) at pivot z6 for fields.parquet
  shard 0    tiles 1365..=1402  (   38 pivot tile(s), ~98412331 row(s), 6.2%)
  shard 1    tiles 1403..=1418  (   16 pivot tile(s), ~99104772 row(s), 6.3%)
  ...
```

If it warns that rows could not be placed, the input's row groups carry no
usable bbox statistics and the cut has degraded towards equal width. Run the
input through `gpio optimize` first — the Hilbert sorting and covering columns
it adds make the estimate good, and make every shard's read pruning bite.

**Pick the pivot so each shard holds a few tiles' worth of data.** Too coarse
and there are not enough tiles to balance a large fleet; too fine and the
coarse job is doing most of the build on its own. z4–z8 covers every realistic
fleet size.

### 1. The coarse job (one run, the whole input)

```bash
tylertoo tiles fields.parquet coarse.pmtiles \
    --min-zoom 0 --max-zoom 14 \
    --shard coarse --shard-plan shards.json \
    --save-plan convert.plan
```

This job does two things: it builds the zooms below the pivot (z0–z5 here), and
it writes `convert.plan` — the artifact every shard then consumes.

**It reads the whole input, but it only builds its own levels** (#541). The
level assignment is dataset-global, so pass 1 and the assignment run over
every row at every level — that is what makes `convert.plan` a complete
artifact the shards can consume. Pass 2 then stops at the pivot: the levels at
and past it are never coalesced, assembled, buffered, spilled or written, and
the verbatim canonical level — the largest of all, and the one that otherwise
costs a whole second read of the input — is not built at all.

With `--keep-overview`, the intermediate it keeps is a **partial** overview:
its footer records the ceiling, `validate` reports it as incomplete, and
`export-pmtiles` refuses it unless given a `--zoom-ceiling` at or below the
recorded one. If every feature first appears at or past the pivot, the coarse
job writes an empty archive and exits 0, like an empty data shard. Under
`--no-streaming` the coarse job falls back to building every level (same
tiles, more work).

The plan is **byte-identical** to what an uncapped coarse job writes; the
ceiling is deliberately outside its fingerprint. That is the invariant the
fleet rests on, and the parity oracle asserts it directly.

Do **not** try to get the same effect with a shallower `--max-zoom`: the
convert plan *is* fingerprinted on the level plan, so a coarse job run that
way produces a plan every shard refuses.

### 2. The shards (N runs, in parallel)

```bash
tylertoo tiles fields.parquet shard-$i.pmtiles \
    --min-zoom 0 --max-zoom 14 \
    --shard $i/16 --shard-plan shards.json \
    --plan convert.plan
```

Each shard reads only the row groups whose bbox reaches its range, replays the
shared assignment onto them, and emits only the tiles its range owns. The jobs
never talk to each other; nothing is shared but the two plan files, both of
which are small and read-only.

On Slurm that is one array job:

```bash
#!/bin/bash
#SBATCH --job-name=tylertoo-shard
#SBATCH --array=0-15
#SBATCH --cpus-per-task=48
#SBATCH --mem=128G
#SBATCH --time=08:00:00

tylertoo tiles "$INPUT" "$OUT/shard-${SLURM_ARRAY_TASK_ID}.pmtiles" \
    --min-zoom 0 --max-zoom 14 \
    --shard "${SLURM_ARRAY_TASK_ID}/16" \
    --shard-plan "$OUT/shards.json" \
    --plan "$OUT/convert.plan" \
    --profile bounded --spill-dir "$TMPDIR"
```

### 3. Merge (minutes)

```bash
tylertoo merge fields.pmtiles coarse.pmtiles shard-*.pmtiles
```

The merge is a **blob copy**: every tile is written out exactly as it came in,
still compressed, never decoded and never re-encoded. There is no dedup pass
because there is nothing to dedup, and no external tool — this is what replaces
tippecanoe's `tile-join`, and the reason a tylertoo pyramid no longer needs
tippecanoe installed.

Disjointness is *validated*, not assumed, per tile id as the merge runs. A
mis-specified fleet — the same shard listed twice, a shard left over from an
earlier run — is an error naming both archives, not an archive whose tiles
silently shadow each other.

## What sharding actually buys you

Being precise about this, because the cost model is not obvious.

The coarse job's **floor** is one pass-1 scan plus the level assignment over
the whole dataset. That cannot be sharded — it is the thing that makes the
fleet agree with itself (see the next section) — so the fleet's wall clock is
bounded below by it.

Above that floor, the coarse job still makes **one full-width read of the
input in pass 2**: every selected row group, every column the output carries,
decoded — most of it only to be discarded, because the rows that reach a
coarse level are picked *after* decode. What it saves is everything past the
read, for the levels at and past the pivot:

- pass 2 generalizes, buffers and writes only the levels below the pivot,
  which on a thinned pyramid is a small fraction of the rows;
- the ladder cascade's **fine steps are still computed** — a coarse level's
  geometry is canonical geometry folded through every finer level's GSD in
  turn (#218), and skipping those steps would change the coordinates — but
  only for the rows that reach a coarse level, which after thinning is a small
  fraction of the input;
- the canonical level's second read of the input, and the verbatim write of
  every row with every property, are **gone** — so pass 2 is one read, not
  two (this is a duplicating-mode saving; `tiles` always converts in
  duplicating mode).

How much that buys depends on how hard the coarse levels thin. With
`--no-drop` or a loose density budget they keep most rows, and the coarse job
costs nearly a full convert.

What you get for that:

- **The export and the shard converts parallelize.** Each shard reads only the
  row groups its range reaches (on `gpio`-optimized input that is a real
  fraction of the file) and exports only its own tiles, so the finest, most
  expensive zooms are built N-ways in parallel.
- **Restartability, which is the bigger prize at this scale.** A monolithic
  run that dies at hour 40 has nothing to show for it. Here a failed shard is
  one array-task re-run, against plan files that are already on disk.
- **Bounded memory and disk for the data shards**, so most of the fleet fits
  scheduling windows and node limits that one enormous job does not. The
  coarse job is the exception: its peak is the monolithic pass-1/assign peak
  whatever `N` is (see the next section).

Budget the coarse job as *pass 1 + assign over the whole input, plus one
full-width read of the input in pass 2 (decoded, mostly discarded), plus
generalization and writing for the coarse levels only* — cheaper than a full
convert in time and disk, not in peak memory.

## Sizing the coarse job's memory

The coarse job's irreducible cost is the **pass-1 feature table**: one
`AssignFeature` per input row, held resident from pass 1's scan through the
level assignment. It costs **64 bytes/row** — measured from the struct's
actual layout (`size_of::<AssignFeature>()`; two of its fields are `Option`s,
so alignment padding costs more than the payload alone would suggest) and
cross-checked against a field incident: a 1.58B-row coarse job logged
`[rss] pass1 scan: 96836 MiB` right after the pass-1 scan —
`96,836 MiB ÷ 1.58B rows ≈ 64.3 bytes/row` — and was OOM-killed later, during
the winner-grid wave build. 64 bytes/row is a floor, not the scan's whole
peak: smaller per-row vectors (ranking keys, and depending on options
accumulate values, polygon areas, line geometries) coexist with it during
the scan.

That feature table is only part of what the coarse job holds at once — the
level-assignment winner grids (per-level, budgeted separately) and pass 2's
buffered output also need memory, concurrently with (or right after) it.
**Rule of thumb: budget the coarse job at ≳ (rows × 64 bytes) × 2.5.** For
the field incident above (1.58B rows, a 94.2 GiB floor), that is ≳235 GiB:
the job OOM'd on a 192 GiB box — only ~2.04× the floor — 25 minutes in, and
ran on 360 GiB. That incident predates #541, when the coarse job's pass 2
still built every level; since #541 its pass 2 builds only the levels below
the pivot, so its pass-2 buffers are smaller and the ×2.5 multiplier is
conservative for it. The pass-1 floor itself is unchanged.

**The remedy is a bigger box.** Sharding does not lower this floor: the
coarse job runs the full pass 1 over the whole input, whatever `N` is. Only a
`--plan` replay skips pass 1 (it re-reads the saved 1-byte/row winner table
instead), and that plan must first be cut — once — on a machine big enough
for the full pass 1.

tylertoo checks this automatically and cheaply: before pass 1 reads a single
row, it estimates `selected rows × 64 bytes` from the input's footers (no
data pages read) and compares it to the process's memory figure — the same
container-aware probe `auto` uses, but read fresh and attributed to its
source:

- **Warning** when the realistic need (floor × 2.5) exceeds the figure,
  naming the numbers, the figure's source (`MemAvailable`, cgroup
  `memory.max` or `memory.high`, or the `TYLERTOO_AUTO_MEM_LIMIT_BYTES`
  override) and this sizing rule. The incident above would have warned.
- **Hard error** only when the floor alone exceeds a **hard** cgroup limit
  (`memory.max`, or v1 `memory.limit_in_bytes`) — the kernel would kill the
  job anyway, so it fails in seconds instead of after the scan.
  `MemAvailable` (momentary, excludes swap), `memory.high` (a throttle, not
  a kill) and the override are advisory and only ever warn.
- With `--bbox` or `--filter` the footer count is an unpruned upper bound
  (rows that miss either never enter the table), so the check only warns.

If the estimate is wrong for your setup (an unusual cgroup nesting, or a box
you already know can page through it), set
`TYLERTOO_SKIP_MEMORY_PREFLIGHT=1` (also `true`, `yes` or `on`) to downgrade
the hard error to the warning and proceed anyway. Off Linux there is no
cgroup or `MemAvailable` to read, so the check does nothing unless the
override is set. A `--plan` replay skips it.

This check does not (yet) make the coarse job's floor any smaller —
[#543](https://github.com/geoparquet-io/tylertoo/issues/543) tracks shrinking
it (a narrower struct-of-arrays layout, or spilling the table between scan
and assign) — it only fails fast instead of after a long scan.

## Why a shard must consume the convert plan

`--shard I/N` **without `--plan` is a hard error**, and that is not a
formality.

The level assignment — which zoom each feature first appears at — is not a
per-feature function. It threads dataset-wide state:

- the density budget water-fills a 128 × GSD super-cell budget over *every*
  candidate of a level;
- the level walk runs coarse → fine carrying a running kept count;
- `--magnitude-ladder` dense-ranks the **global** distinct values of its
  column;
- the automatic class ranking picks its column from a **global** vocabulary
  scan.

A shard that recomputed any of that over its own subset would reach a
different answer, and neighbouring shards would disagree about which features
exist at which zoom. The seams would not line up — subtly, in a way no tile
count would reveal. So the assignment is computed **once**, by the coarse job,
and every shard replays it.

The **convert plan** (`convert.plan`) is fingerprinted and checksummed: an
xxh3-64 over its payload, plus a fingerprint pinning the tylertoo version,
every thinning option, and each input part's path, size, mtime, row count and
row-group layout. A shard given the wrong plan, a stale plan, or a plan for a
since-rewritten input fails immediately and by name.

The **shard plan** (`shards.json`) is a different, smaller artifact and binds
itself differently: a `format` discriminator and a version, a structural check
that its ranges tile the pivot zoom exactly (no gap, no overlap), and a
per-part input binding (path, row count, row-group count). It carries no
checksum — it is small, human-readable JSON, and a corrupted one fails the
structure check rather than passing unnoticed.

The two are tied together so a fleet cannot straddle two different cuts: the
coarse job stamps an xxh3-64 digest of the cut — the pivot zoom and the lo/hi
sequence, nothing else — into the convert plan's fingerprint, and every shard
must present a `shards.json` with the same digest. Re-cutting the plan
mid-build is therefore an error naming both digests, not a fleet whose
archives overlap at some seams and leave holes at others.

## How a shard replays a plan it only partly reads

This is the one genuinely delicate part, and worth understanding before
trusting the output.

The plan's winner table is **one byte per input row, addressed by row position
within the row stream the plan was saved over**. A shard that prunes row groups
streams *fewer* rows, so its own row 0 is not the plan's row 0 — it might be
the plan's row four million. Handing the plan over unchanged would read the
wrong winner byte for nearly every feature: not a crash, a quietly wrong
pyramid.

tylertoo re-addresses the plan instead. The plan records the row-group
selection it was saved over, and row-group row counts are footer facts, so a
shard can compute exactly where each of its groups sits in the plan's stream
and rewrite the winner table, the geometry kinds, the carriers and the cluster
tables into its own addressing before pass 2 runs. Values are never
recomputed, only moved. You can watch it happen:

```
[convert] shard 21..41 (z3, 21 pivot tiles): reading 13/20 input row groups
[convert] shard: re-addressed the convert plan onto 936 of its 1440 row(s) in
          1 contiguous run(s); 936 feature(s) to build
```

The fingerprint gains exactly one relaxation for this: in shard mode the
per-part row-group term is a **subset** test rather than an equality. A shard
may narrow the plan's selection; it may not widen it, and a row group the plan
never saw is refused by name.

## v1 restrictions

**Line coalescing is not supported with `--shard`.** A coalesced chain is a
*new* geometry spanning every row it merged, so no single input row group's
bbox bounds it. The shard holding the chain's row would emit tiles past its own
range while its neighbour, which never reads that row, emitted none — a gap at
the seam. A shard run against a plan that carries chains is refused, naming
`--no-coalesce-lines` as the fix. Turn it off for the **whole fleet**,
including the coarse job, so the plan matches.

Point clustering, `--accumulate-attribute` and tiny-polygon carriers **are**
supported. They key off a single input row whose own bbox bounds it, so the
row-group argument that makes ordinary features safe covers them unchanged: a
representative near a seam is read by both neighbours, and each emits only its
own tiles.

**A shard that owns no rows succeeds.** The cut has to tile the pivot zoom with
no gap, so `shard-plan` cuts N ranges whatever the data looks like and a
concentrated dataset leaves some of them empty. Those jobs exit 0 and write a
valid, tile-less archive; `merge` skips it for the zoom range, the bounds and
the layer declarations alike. `shard-plan` warns at cut time when it produces
such ranges, which is the signal that a smaller `--shards` would balance the
fleet better.

**`--tile-buffer` is capped at 512 tile pixels.** A shard prunes its input to
the row groups within two pivot tiles of its range; a wider buffer could pull
geometry into one of its tiles from a row group it never read, so the tile
would come out missing geometry the monolithic run has. 512 is two full tile
widths, against a default of 8 — anything past it is refused rather than
silently wrong.

The Python bindings do not expose sharding yet, the same as the convert plan it
depends on.

## Is it really identical?

Yes, and it is checked that way. `crates/core/tests/shard_merge_parity.rs`
builds a coarse job plus N shards, merges them, and compares against a
monolithic run of the same input with the same options, asserting:

- every pair of jobs holds **disjoint** tile ids;
- together they cover the monolithic tile set **exactly** — no gap at a seam,
  no tile built twice;
- identical per-zoom tile counts;
- **byte-identical tile bodies**, tile by tile.

…and, on the archive rather than the tiles: the same declared zoom range, the
same bounds, and the same `vector_layers` as the monolithic archive.

Tile counts alone would not be enough: a seam bug moves geometry between
neighbouring tiles while keeping every count the same. Byte equality is what
catches it. Several fixtures run it — real admin polygons with seams cutting
through them, a world-spanning polygon grid whose row groups actually prune
(with every knob at its defaults, with `--collapse-square` carriers, and with
coalescing off), and a generated point grid with `--cluster
--accumulate-attribute`, so all four of the plan's row-indexed side tables are
exercised.

**What is *not* claimed: the merged FILE is not byte-identical to a monolithic
one.** The tile bodies are; the container is not. A merged archive is
assembled from N inputs, so its directory layout, its deduplication accounting
and its tile order within a zoom differ, and it currently carries **no
tilestats** — `merge` unions the inputs' `vector_layers` but does not
recompute per-layer attribute statistics. If a downstream consumer needs
tilestats, generate them from the merged archive rather than expecting them to
survive the merge.

## Choosing the pieces

| Knob | What it decides |
|---|---|
| `--shards N` | Fleet size. One job per shard plus the coarse job. |
| `--pivot Z` | Where the split falls. Shards own `[Z, --max-zoom]`; the coarse job owns `[0, Z-1]`. Aim for a few tiles of data per shard; z4–z8 in practice. |
| `--shard coarse` | The job that owns the zooms coarser than the pivot and writes the convert plan. |
| `--shard I/N` | Data shard `I`. Requires `--plan`. |

For a cut you want to choose by hand rather than have balanced for you,
`export-pmtiles --tile-range LO..HI` takes two tile ids at one zoom directly,
and `--zoom-ceiling Z` on the same command is the coarse half's complement.
(It is a *ceiling*, not a `--max-zoom`: unlike `--min-zoom`, which only widens
what the header declares, this decides which zooms are actually emitted.)
`tiles --shard` is the same mechanism with the bookkeeping done for you.
