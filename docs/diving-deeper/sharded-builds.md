# Sharded builds: one dataset, a fleet of machines

A single `tylertoo tiles` run is one process on one machine. That is the right
shape up to a surprising size — a 19.5M-polygon country converts in about an
hour on a 48-core node — but it does not survive the jump to a planet. The
2025 FTW field-boundary dataset is 1.58 billion features, and a monolithic
convert of it projects to somewhere between one and three days: past every
normal HPC scheduling window, and with nothing to show for it if the job dies
at hour 40.

Sharding splits that one run into N independent jobs that can run at the same
time on N machines, plus a merge measured in minutes. This page is the recipe
and the reasoning behind it.

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
it writes `convert.plan` — the artifact every shard then consumes. It reads the
whole input, because the level assignment has to.

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

The projected shape for a planet-scale build: coarse job ~1 h, 16 shards in
parallel ~1–2 h each, merge ~minutes. What used to be "days, and everything is
lost if it dies" becomes "an afternoon, and a failed shard is one `sbatch
--array=7` away from fixed".

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

The plan is [fingerprinted and checksummed](../OVERVIEW_TUNING.md), so a shard
given the wrong plan, a stale plan, or a plan for a since-rewritten input
fails immediately and by name.

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

Tile counts alone would not be enough: a seam bug moves geometry between
neighbouring tiles while keeping every count the same. Byte equality is what
catches it. Two fixtures run it — real admin polygons with seams cutting
through them, and a world-spanning grid whose row groups actually prune, so
both the clipping path and the plan-re-addressing path are covered.

## Choosing the pieces

| Knob | What it decides |
|---|---|
| `--shards N` | Fleet size. One job per shard plus the coarse job. |
| `--pivot Z` | Where the split falls. Shards own `[Z, --max-zoom]`; the coarse job owns `[0, Z-1]`. Aim for a few tiles of data per shard; z4–z8 in practice. |
| `--shard coarse` | The job that owns the zooms above the pivot and writes the convert plan. |
| `--shard I/N` | Data shard `I`. Requires `--plan`. |

For a cut you want to choose by hand rather than have balanced for you,
`export-pmtiles --tile-range LO..HI` takes two tile ids at one zoom directly,
and `--max-zoom` on the same command is the coarse half's complement. `tiles
--shard` is the same mechanism with the bookkeeping done for you.
