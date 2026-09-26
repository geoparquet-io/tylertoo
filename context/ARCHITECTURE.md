# tylertoo Architecture

Design decisions and tippecanoe divergences for the **current** system.
Historical material (the removed per-tile pipeline, execution plans,
session triage) lives in [`context/archive/`](https://github.com/geoparquet-io/tylertoo/blob/main/context/archive/README.md).

Related canonical documents:

- **Format**: [`context/OVERVIEWS_SPEC.md`](https://github.com/geoparquet-io/tylertoo/blob/main/context/OVERVIEWS_SPEC.md) — the
  `geo:overviews` draft spec (single source of truth for the file format).
- **Tuning**: [`docs/OVERVIEW_TUNING.md`](https://github.com/geoparquet-io/tylertoo/blob/main/docs/OVERVIEW_TUNING.md) — every
  generalization knob, its default, and its direction.
- **Benchmarks**: [`benchmarks/overview/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/overview/RESULTS.md)
  (storage/access numbers) and
  [`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/overview/PROFILE.md)
  (performance methodology + history).

## Decision Record: Legacy Tiles Pipeline Removed (#177, 2026-07-03)

The legacy per-tile pipeline (`crates/core/src/pipeline.rs`, `Converter`, the streaming
external-sort/bucketed tiler and its quality features) was **removed**. The
overview pipeline (`overview convert` → `export-pmtiles`) supersedes it for
the project's core workflow: it is faster (Moldova full pipeline < 2 min),
memory-bounded (convert ~0.4 GB for a z0–6 pyramid, ~1.4 GB for z0–14; export ~0.89 GB), and carries the quality
ladder (ranking, density budget, clustering, coalescing) the tile path never
got. See `context/TILE_SIMPLIFY_POSTMORTEM.md` for why the tile-path quality
work had already been excised.

`crates/core/src/overview/pipeline.rs` is a different, live file: the
single-read pass-2 engine added in #213. Only the crate-root
`crates/core/src/pipeline.rs` was deleted.

What survives:

- **The `tiles` CLI subcommand** (and the bare `tylertoo in.parquet
  out.pmtiles` form) as a ~90-line facade: overview convert into a temporary
  GeoParquet file → export-pmtiles to the requested output. One-shot
  "GeoParquet in, PMTiles out" UX is preserved; the legacy tuning flags are
  gone (use `overview` + `export-pmtiles` directly for knobs).
- **The Python `convert()` binding**, re-pointed at the same facade path with
  a deprecation note steering users to `overview()` / `export_pmtiles()`.
- **Shared infrastructure** the overview pipeline builds on (tile math,
  clipping, MVT encoding, the PMTiles v3 writer, GeoArrow batch decoding).

Consequences: #102 (row-group bbox filtering for the tiles pipeline) lost
its remaining scope; the legacy pipeline's architecture notes were moved to
[`context/archive/LEGACY_TILES_ARCHITECTURE.md`](https://github.com/geoparquet-io/tylertoo/blob/main/context/archive/LEGACY_TILES_ARCHITECTURE.md).

## Design Principles

1. **Overview-first**: the product is the `geo:overviews` GeoParquet format;
   PMTiles is an *export* of it, not a parallel pipeline.
2. **Arrow-first I/O**: geometries are decoded within Arrow batch scope;
   memory is bounded by read batch + per-feature tables, never by the dataset.
3. **Reference implementations**: generalization behavior is calibrated
   against tippecanoe output on the shared corpus; divergences are documented
   below and in the spec.
4. **PMTiles writer**: the `pmtiles` crate is read-only; we implement our own
   v3 writer (`pmtiles_writer.rs`, streaming, deduplicating).
5. **Defaults should look right**: default knob values are chosen from
   rendered sweeps on the corpus (see `corpus/SWEEPS.md`), not guessed.

## The Overview Pipeline

### Convert (`overview convert`, `crates/core/src/overview/`)

Turns a (gpio-optimized) GeoParquet file into a level-banded overview file.
Per non-canonical level: line coalescing (on by default) → visibility gates →
cell-winner thinning (ranked) → density budget (Q2) → world-space RDP
simplification → level-banded write. The canonical (finest) level is always
verbatim (spec §2.4).

**Streaming is the default** (`stream.rs`, two passes):

0. **Pass 0 (remote input only)** stages the selected row groups to the local
   disk spill up front (#286/#287). A row group's column chunks are a
   *contiguous* byte span, so each selected row group is fetched as **one**
   coalesced range request (several in flight per part, bounded by a
   memory budget) and sliced back into the per-column-chunk spill entries the
   reader already serves from. Both passes below then read entirely from local
   disk. This removes the two latency-bound patterns high-TTFB hosts exposed:
   the reader keeping ~1 range request in flight per column chunk per pass
   (#287), and pass 2 re-fetching, cold, the property columns pass 1's
   geometry+ranking projection skipped (#286). Only selected row groups are
   staged, so pruned groups are still never touched and total network traffic
   stays ≈1× the object (#219); local inputs skip pass 0 entirely.
1. **Pass 1** streams the input once, keeping only a small per-feature record
   (bbox, kind, ranking key). Level assignment + density budget run over
   those records to produce per-level **winner tables** (~1 byte/feature).

   The assignment is parallel *within* the phase, not only across levels
   (#534). Running each level's cell-winner grid on its own thread (#264) is
   the obvious decomposition, but it evaporates at dataset scale: the #306
   grid RAM budget packs the levels into waves, and a wave of one level has
   nothing to overlap — which is how a 55.5M-row convert came to spend 107s
   single-threaded here, more than the whole scan. So a level's grid is
   **shard-partitioned**: threads classify disjoint feature ranges and bucket
   each placement by the shard owning its cell, then reduce the shards in
   parallel, one map per task. No locks, no merge tail, and one entry per
   occupied cell either way, so the #306 estimate is unchanged. The wave's
   winner fold splits by position range, and the density budget's candidate
   scan and super-cell partition go wide while its admission fold (which
   threads a running kept count coarse→fine) stays serial. All of it is
   scheduling: the assignment, and therefore the `--save-plan` artifact, is
   byte-identical at any thread count (`thread_count_determinism.rs`).
2. **Pass 2** reads the input once more and fans each Arrow batch to *all*
   levels at once (the single-read pipelined engine, `overview/pipeline.rs`, #213): a
   reader thread streams batches over a bounded channel while a consumer
   parallelizes every `(level × feature)` simplification across cores; each
   level's output drains to the writer in level order, canonical last. A
   serial engine that re-reads the input once per level is retained as the
   equivalence-tested reference.

   Since #494 the read side of that sentence is wider than one thread. Pass 2
   splits the selected row groups into ordered **segments** (`input_set.rs`,
   runs of one part's row groups capped at what a reader can buffer ahead) and
   hands them to up to N **reader workers** (`--read-workers`, auto =
   `min(cores / 4, 4)`) round-robin, each with its own bounded read-ahead
   channel sized against a modelled slice of the memory budget. The producer
   thread **merges** the workers' output in segment order and re-chunks it
   (`Regrouper`) into exactly the batch sequence one sequential reader would
   have produced — batch boundaries are load-bearing, because the writer issues
   one column-writer call per slice and parquet checks its page limits per
   call — then pushes it through the same bounded pipe to the consumer. On the
   sink side each level's `bounded`-profile spill is written by its own
   spill-writer thread behind a `bounded(2)` queue, so the consumer hands the
   batch over instead of encoding it. Output is byte-identical for every worker
   count; remote inputs read sequentially regardless (their parts share one
   chunk cache that concurrent readers would evict).

Pass 1 and the assignment can be **persisted and replayed** (`plan_state.rs`,
`--save-plan` / `--plan`). Two reasons:

- *Resume.* Pass 1 plus the assignment is most of the wall time on a large
  input and depends on nothing the write side does, so re-running with
  different write-side knobs should not pay for it twice.
- *Sharding consistency (the load-bearing one).* The assignment is **not** a
  per-feature function: `apply_density_budget` water-fills a 128 × GSD
  super-cell budget over *every* candidate of a level, `assign_levels_bounded`
  walks levels coarse → fine carrying a running `kept_count`, the entry-zoom
  ladder dense-ranks the *global* distinct values of its column, and the Q1
  auto-detection picks its column from a global vocabulary scan. A shard that
  recomputed the assignment locally would fold over its own subset and reach a
  different answer, so a sharded build MUST consume one global plan.

The artifact is an 8-byte magic plus an xxh3-64 checksum, then a single Arrow
IPC file: the row-indexed winner table as a `UInt8` section, the side tables
(per-row kinds, tiny-polygon carriers, cluster tables, the coalesce line
scratch as WKB) as further sections, and the scalars, ranking provenance and
fingerprint as JSON in the schema metadata under a versioned key.

A plan is read as **hostile input** (#512), to the bar #417/#489/#430 hold
PMTiles reading to: the checksum is verified before a byte reaches an arrow
decoder, and every structural check past it — one-row batch, level count
inside `MAX_LEVELS`, `checked_mul` on the cluster stride, known geometry-kind
codes, no nulls in a non-nullable section — is an error naming `--plan` and
the path, never a panic.

A checksum is an *integrity* check, never a *consistency* one — anyone who
can forge a plan can re-checksum it — so the row-indexed sections are also
checked against each other and against the run. `kinds` must be exactly as
long as `min_levels` (both are addressed by raw row position in pass 2's
batch fan-out), and the plan's row domain is compared against the sum of
`num_rows()` over the row groups *this* run selected. That comparison runs
unconditionally: it was once gated on an unpruned-footer total, which meant
`--bbox` / `--filter` turned it off entirely and a forged plan reached an
out-of-bounds index in pass 2. The remaining sections cannot index out of
bounds by construction — `carriers` is bounded by the level count and only
`binary_search`ed, and the coalesce and cluster sections are length-matched
at rebuild and keyed through a `HashMap`.

Loading also verifies the fingerprint (tylertoo version, every
thinning-relevant option field by field, and each input part's identity) and
hard-errors naming the offending field rather than running stale. Input
identity is, for local files **and** remote objects alike, the path/URL, the
byte size, the footer **row count**, the footer row-group count and the
selected row groups, plus mtime for a local file (#511). The row count is the
load-bearing term: the winner table is addressed by row *position*, so it is
the only cheap fact that catches a swapped remote object — `fs::metadata` on
an `s3://` display name sees nothing. There is no ETag or object mtime term
(neither is plumbed past `RemoteSource::connect`), so a remote part gets a
one-line warning on save and load naming what is and is not pinned.

Both paths are preflighted in `validate_options` (#513), alongside the #272
`--spill-dir` check: `--plan` must be readable and carry the plan magic, and
`--save-plan` must be writable — its parent must exist and accept a write,
*and*, when the target file already exists, that file itself must open for
writing. `--save-plan` is written only *after* pass 1 and the assignment, so
a bad target used to cost the whole scan. An existing plan is overwritten
with a log line, matching how `overview` treats its output (`tiles` refuses
an existing archive unless given `-f/--force`); the
preflight only ever observes, so it never truncates the plan it probes. The
magic's last byte is the format version and is matched separately from the
`TTPLAN\0` prefix, so a plan from a future tylertoo reports its version
rather than "bad magic bytes".

Peak memory is `O(read batch + winner tables)` — Moldova (632k polygons,
38M vertices) converts to a z0–14 pyramid in ~45 s / ~1.4 GB peak RSS on a 16-core machine
(a default z0–6 pyramid is ~7 s / ~0.4 GB).
`--no-streaming` keeps the in-memory pipeline as the equivalence-tested
reference implementation.

Simplification (`overview/simplify.rs`) is Ramer–Douglas–Peucker in world
space with tolerance = `simplify_factor × gsd(level)`. By default it
**cascades** (#218): each coarser level simplifies the next-finer level's
already-simplified output (tippecanoe-style), and an invalid RDP candidate is
repaired in a single boolean-overlay pass. `--no-cascade` restores the
non-cascaded path, where an invalid candidate instead retries at
`eps/2, eps/4, eps/8` before falling back to the original geometry (counted,
logged at debug level). Either way the validity check is capped at 2048
vertices on oversized candidates (#242) to avoid an O(V²) stall, and the
canonical level is always verbatim.

### Export (`export-pmtiles`, `overview/export.rs`)

Batch PMTiles export **from** an overview file. The overview file already
holds thinned/simplified/ranked features per level, so export is mechanical
and single-pass per zoom: resolve each level to a Web Mercator zoom, stream
the level band, split each feature into its tiles, clip to buffered tile
bounds (bbox fast path skips the clip when fully inside), MVT-encode
(rayon-parallel), and stream finished tile partitions into the
`StreamingPmtilesWriter`. No global external sort, no per-tile budget retry
loop.

Feature→tile splitting is a **top-down recursive quadtree cascade** (#226,
tippecanoe's tiling model): starting from the feature's covering tile, each
pyramid level clips the parent's already-reduced geometry into its four child
regions down to the target zoom. A vertex therefore takes part in `O(depth)`
clips instead of `O(tiles_spanned)`, so cost scales with output size + depth
rather than `Σ_features (tiles_spanned × vertices)` — the earlier per-feature
`tiles_for_bbox` loop clipped the full geometry once per covered tile and blew
up to billions of clip-vertex ops on large admin polygons (adm4 export DNF'd
at 3h13m). The cascade is a proper superset chain (`child ± buffer ⊆ parent ±
buffer`), so each leaf's clip equals the direct clip — interior features pass
through byte-identical; seam-crossers match modulo float/ring-normalization
noise. The recursion is bounded by the same `tile_ranges_for_bbox` math
`tiles_for_bbox` uses, so the emitted tile set is unchanged.

The one safety valve is `--tile-size-limit` (default `500K`, tippecanoe
parity — issue #280; `0` disables it): an oversized tile gets a **single,
non-iterative** drop pass, then is re-encoded once. Which features survive
depends on the tile's geometry (`select_kept_members`): polygon/line tiles
keep their largest-vertex features (the visual signal); point-dominated
tiles — where vertex count carries no signal — instead keep a uniform stride
across member (Hilbert) order, so the surviving dots stay spatially spread
rather than clumped in one corner.

Border duplication is the expected delta between overview level counts and
export per-zoom feature totals (a feature spanning a tile seam appears in
every tile it touches): 0% while a level fits one tile, ~7% at z14 on
Portland roads.

**Progress logging & salvageable output (#229).** Long exports get stuck in the
finest level's scan/clip (adm4 DNF'd at 3h13m with nothing written), so export
emits `[export]`-prefixed `log::info!` lines — visible under the CLI's default
`info` filter — at three granularities: a `scan complete` marker, a per-level
`done` summary (level *i/N*, zoom, feats, tiles, partitions, elapsed), and a
throttled within-level **wave counter** (`WAVE_LOG_INTERVAL`, 30 s) so a stuck
level is diagnosable in minutes (counter frozen) vs merely slow (counter
advancing). After each finished level — throttled to `CHECKPOINT_INTERVAL`
(60 s), so fast exports that finish in one `finalize` pay nothing —
`writer.checkpoint()` snapshots a valid archive capped at that zoom, so an
interrupted run keeps its finished coarse zooms instead of losing hours of
compute. `checkpoint` and `finalize` route through the same assembler, so the
final archive is **byte-identical** whether or not any checkpoints were taken
(verified on madagascar-adm4).

The snapshot is **`<output>.partial`**, not `<output>`: since #459 export uses
the tail-directory layout (below), where that file is the live archive rather
than a scratch copy. The guarantee is stated precisely, because the middle case
is the dangerous one:

* **No tile added since the last checkpoint** — the file is a complete,
  readable archive capped at the zooms finished so far.
* **Tiles added since** — the file is **detectably invalid**. The first append
  after a checkpoint overwrites the metadata and leaf sections the header
  points at, so it first zeroes the prefix's magic; every reader then rejects
  the file instead of following stale leaf pointers into tile bytes and
  returning plausible garbage. The next checkpoint rebuilds prefix and tail
  from the (untouched) tile data and the file is an archive again.

So an interrupted run salvages back to the last checkpoint or to nothing —
never to something that reads but lies. Each checkpoint logs the path. A
`<output>.partial` left by an earlier run is moved to `<output>.partial.prev`
when the next run opens the same output rather than being truncated, so a
scripted rerun does not destroy the crashed run's only recoverable output; a
successful finalize removes it.

### Validate (`overview/check.rs`)

`tylertoo validate` checks a file against spec §6.2: footer schema, level
banding/row-group alignment, canonical fidelity, monotonicity, cluster
`point_count` sum invariant (§12.1), coalescing `coalesced_count` rules
(§13), bbox covering.

### Sharded builds (`shard.rs`, `tiles --shard`, #498)

A shard is a contiguous run of **pivot-zoom tile ids**, and with it every
descendant of those ids at every deeper zoom. The Hilbert nesting property
(`tile::node_id_range`) makes a node's descendants an *exact* contiguous
interval at every deeper zoom, so N runs partitioning the pivot zoom partition
every deeper zoom — shards are disjoint by construction rather than by bbox
intersection, and `merge.rs` puts them back together by blob copy.

The consequence that matters: features stay **whole** through convert and
clip. A feature crossing a seam is read by both neighbouring shards and
clipped normally by both; each emits only the tiles its own range owns. That
is what the `--bbox`-band workaround #498 describes could not do (it includes
every feature whose bbox intersects the band, so a straddler appears twice in
the merged edge tiles).

Four architectural decisions are load-bearing:

1. **A shard MUST consume a convert plan.** The level assignment threads
   dataset-wide fold state (see the `plan_state.rs` note above), so a shard
   that recomputed it locally would disagree with its siblings at the seams —
   invisibly, since no tile count would change. `--shard` without `--plan` is
   refused in `validate_options`. One coarse job owns zooms `[0, pivot-1]`,
   reads the whole input, and is the run that writes the plan.
2. **Row indices are the plan's, not the shard's.** Every row-indexed plan
   section is addressed by row *position within the stream the plan was saved
   over*. A shard prunes row groups, so its stream is shorter and its own
   `row_offset` counts a different sequence. `plan_state::rebase_plan_for_shard`
   re-addresses the plan onto the shard's stream — the plan records its own
   selection, row-group row counts are footer facts, so the shard's rows are a
   handful of contiguous runs and the winner table, kinds, carriers and
   cluster tables move with them. Values are moved, never recomputed. The
   fingerprint's one relaxation (`SelectionRule::SubsetAllowed`) exists for
   this and nothing else: a shard may narrow the plan's row-group selection,
   never widen it.
3. **The restriction is applied at export PLAN time**, to `LevelScan::tile_counts`
   before `plan_partitions` cuts it — so an out-of-range tile is never
   clipped, encoded or hashed, and each wave's band read prunes with it. A
   shard costs its share of the export, not all of it.
4. **The fleet is bound to one cut by the convert plan.** `ShardPlan::cut_digest`
   is an xxh3-64 over the canonical cut (pivot zoom + the lo/hi sequence, and
   nothing advisory), and the CLI stamps it into the convert plan's
   fingerprint options map under `shard_plan_digest` — on the coarse job,
   which writes the plan, and on every data shard, which must present the
   same one. "Same convert plan ⇒ same cut" is therefore true by
   construction: a `shards.json` re-cut mid-build is a named error rather
   than a fleet whose archives overlap at some seams and leave holes at
   others. `ConvertOptions::shard` itself is deliberately *not* fingerprinted
   — it is per-job, and every shard of a fleet has a different one.

**The coarse job caps pass 2 at the pivot — full assign, partial ladder**
(#541). `ConvertOptions::zoom_ceiling` (set by the CLI for `--shard coarse`,
mirroring `ExportOptions::zoom_ceiling`) truncates the level set pass 2
materializes to the levels the coarse job actually exports. Three things make
this safe, and each is load-bearing:

1. **Pass 1 and the assignment stay full-range**, and so does the artifact
   `--save-plan` writes. The assignment is dataset-global (the density budget
   water-fills a super-cell over every candidate of a level, the level walk
   carries a running kept count, tie-breaks hash global row indices), so a
   coarse job that assigned only its own zooms would hand its shards a
   different pyramid. The ceiling is therefore **excluded from
   `options_digest`**: a capped coarse job and a full run save a
   byte-identical plan, asserted directly in `tests/shard_merge_parity.rs`.
2. **The cascade's fine steps are still computed.** A coarse level's geometry
   is canonical geometry folded through every finer level's GSD in turn
   (#218), so the chain is built over the WHOLE planned ladder and only the
   *materialization* is truncated. In the pipelined engine the steps finer
   than the deepest buffered level become a `prefix` that
   `process_batch_cascade` folds first, from canonical geometry — exactly
   what `simplify_cascade` does for that level on the Serial path. The prefix
   is empty for an uncapped run, so nothing about a non-sharded build
   changes. What *is* skipped for free: the cascade superset narrows from
   "member of the finest non-canonical level" (nearly every row) to "member
   of the deepest kept level" (a thinned fraction), so far fewer geometries
   are decoded and folded at all.
3. **The finest kept level joins the buffered set.** The pipelined engine
   streams its last level separately only because it is the verbatim
   canonical one, far too large to buffer. Under a ceiling the deepest level
   is an ordinary simplified one, so in duplicating mode `run_pass2_levels`
   buffers every level and pass 2 costs **one** read of the input instead of
   two (the ceiling is duplicating-only: `validate_options` refuses it with
   partitioning, and with a data shard range).

What it does **not** skip, stated precisely because the cost model is easy to
overstate: pass 2's one read is still a full-width read of the input — every
row group the run selected, every projected column, decoded (there is no
`RowSelection`; the cascade-superset narrowing happens *after* decode, and
most of what is decoded is then discarded). So the coarse job costs *pass 1 +
assign over the whole input, plus one full-width read of the input in pass 2,
plus generalization/encode/write for the coarse levels only*. Its peak memory
is the monolithic pass-1/assign peak, not a bounded per-job figure (size it
with #549's preflight). The savings scale with how aggressively the coarse
levels thin: under `--no-drop` or a loose density budget the coarse levels
hold most rows and little is saved.

**The capped file describes itself** (#541 review). When the ceiling truncates
the plan, the footer records `generalization.zoom_ceiling`; the validator
reports the file as non-conforming (`complete_pyramid`: the finest level is
not the canonical one, so §2.4 cannot hold — `canonical_level` still reads
`L-1` because §3.4 requires it), and `export_pmtiles` refuses it unless its
own ceiling is at or coarser than the recorded one. A `--keep-overview` of a
coarse job therefore cannot pass for a complete pyramid. A ceiling that leaves
nothing to write returns `ConvertError::NothingAtOrBelowCeiling` (after any
`--save-plan`), which the CLI turns into an empty coarse archive, exactly as
it does for an empty data shard.

**`coalesced_count` publication is decided from the conversion, not the
file** (#541 review). The export withholds the counter when it never left 1
(#379). That used to be read from row-group statistics of whichever levels
the file holds, so a capped coarse job — whose merges typically all happen
finer than the ceiling — withheld a column the monolithic build published,
and the coarse tiles differed. The converter now records
`coalescing.merged` (any chain > 1 at any planned non-canonical level; the
unmaterialized levels' chain tables are built, checked and dropped,
short-circuiting at the first merge) and the export prefers it, falling back
to the statistic for older files. Data shards are unaffected in practice:
they refuse any plan carrying coalesce rows, so no shard ever merges.

**An empty data shard succeeds.** The cut must tile the pivot zoom with no
gap, so a concentrated dataset leaves some ranges owning no rows. Those jobs
write a valid tile-less archive (`export::write_empty_archive`) and exit 0;
`merge` excludes a tile-less input from the zoom union, the bounds union and
the layer declarations alike.

**v1 restriction: line coalescing.** A coalesced chain is a new geometry
spanning every row it merged, which no single input row group's bbox bounds.
The shard holding the chain's row would emit tiles past its range while its
neighbour emitted none — a gap at the seam. A shard against a plan carrying
chains is refused, naming `--no-coalesce-lines`. Clustering, accumulation and
tiny-polygon carriers are supported: each keys off one input row whose own
bbox bounds it, so the row-group argument that makes ordinary features safe
covers them unchanged.

The acceptance test is `crates/core/tests/shard_merge_parity.rs`: coarse + N
shards + one merge vs. a monolithic run, asserting pairwise-disjoint ids,
exact coverage, identical per-zoom counts and **byte-identical tile bodies**.
Counts alone would not do — a seam bug moves geometry between neighbouring
tiles while keeping every count the same.

## Known Divergences from Tippecanoe (overview pipeline)

| Area | Our approach | Tippecanoe | Notes |
|------|--------------|------------|-------|
| Generalization space | World-space, per **level**, stored in the file | Tile-space, per tile, at encode time | The core format difference: levels are reusable, exact, SQL-queryable |
| Simplification | RDP, tolerance = factor × level GSD, **cascading** by default (#218) with boolean-overlay validity repair (vertex-capped, #242) | `douglas_peucker` in tile pixel space | Canonical level always verbatim; `--no-cascade` reverts to per-level eps-halving |
| Density drop rate | `--drop-rate 1.65`, budget anchored on full canonical count `N` | `-r`/`--drop-rate` 2.5, anchored on per-tile basezoom count | Same geometric ladder; different anchor ⇒ different numeric default (see `corpus/SWEEPS.md`) |
| Spatial fairness | `--drop-gamma` per super-cell allocation ∝ population^(1/γ) | gamma dot-dropping in dense areas | Same idea, applied per super-cell so per-level totals are unchanged |
| Sub-pixel polygons at coarse zooms | Hard-dropped by default; opt-in dispositions: `--collapse` → representative Point (spec Q4), `--collapse-square` → area-dithered ~1×GSD placeholder square (#279) | Tiny-polygon reduction ON by default: accumulates dropped area serially per tile, emits placeholder squares | Type-preserving drop default keeps renderers unsurprised and is unchanged pending the #259-fixture sweep (#279 tracks the default decision). Two mechanisms share `T = tol²` (#384): an **accumulator** over every polygon a level does not carry (gate, thinning, budget), per 32×GSD patch rather than per tile since levels have no tile scope, run once on the pass-1 feature table in input order so all three engines read one carrier set (`overview/accumulate.rs`); and a **per-feature dither** (deterministic hash of the anchor coordinates, keep probability `area/tol²`) for members that collapse at write time. Disjoint sets, byte-identical across engines and thread counts. tippecanoe only accumulates rings with area ≤ `tiny_polygon_size²` and keeps larger rings as geometry; we clamp each polygon's contribution to one placeholder instead of skipping large ones (a non-member was already dropped by the gate/thinning here), so a polygon contributes at most one placeholder of area and a patch keeps < 1 `T` unemitted. tippecanoe places the placeholder at the ring's first vertex with side `tiny_polygon_size` (default 2 px); ours sits at the representative point with side `factor × GSD`. Ladder-placed features (#364) are never accumulated. Accumulator is duplicating-mode only (and `point` bands never accumulate); in partitioning mode neither mechanism runs. Old per-tile-pipeline accumulator design: #85, removed with #177; structural fix: #246 |
| Per-zoom-band representation (#317) | `--representation "0-7:point,8-14:geom"` (or `…:square`): one run, one archive, representation switches per zoom band; point bands bypass the polygon visibility gate and thin on the point grid | `--convert-polygons-to-label-points` applies at EVERY zoom and emits one label point per intersecting tile; zoom-banded representation needs two tilesets merged with `tile-join` | Overview levels are tile-free (a level is a parquet row band), so a per-tile label point is not representable; we emit one deterministic per-feature centroid and the band replaces the two-archive merge. Point flavor: centroid (with bbox-center → first-vertex fallbacks), matching the existing `--collapse` path; planetiler exposes centroid / point-on-surface / innermost-point per layer, tippecanoe leaves the label-point flavor unspecified |
| Point clustering | Winner **keeps its own geometry** and absorbs cell losers into `point_count` | Cluster centroid is the mean position | Deliberate: anchor stays a real feature; deterministic |
| Line continuity | Coalescing chains same-class segments into strokes *before* gates/thinning | `--coalesce`-family merges at tile encode time | Junctions terminate chains by default (junction-angle 0, from the Portland sweep) |
| Tile-size control (export) | Single non-iterative drop pass (`--tile-size-limit`) | Iterative threshold retry loop | Overview levels are already budgeted; the valve is a backstop, not the mechanism |
| Polygon clipping (export) | Sutherland–Hodgman f64 + i_overlay fallback | Sutherland–Hodgman integer tile coords | Same algorithm family, different coordinate space (below) |
| Tile buffer axis (export, #341) | `--tile-buffer` is converted to degrees from the tile's LONGITUDE width (`tile_width x buffer_px / 256`) and that one value is applied to both axes | `--buffer` is tile pixels on both axes | Exact on x; on y the effective buffer is `buffer_px x sec(lat)` pixels, because a Mercator tile's latitude span shrinks as `cos(lat)` while its longitude span does not — 8 px at the equator, ~16 px at 60 deg, ~92 px at 85 deg. Bounded: the over-draw stays under one tile height below ~88.2 deg, outside the Mercator domain, and it errs towards carrying MORE geometry across the seam. A per-axis buffer has to be threaded through `bbox_within_buffered` and `clip_geometry_simple` as well, which moves tile bytes again, so it is deferred. Antimeridian seam continuity is separately out of scope: membership widening is clamped to the lon/lat domain and a feature at lng 179.99 does not reach tile x=0 |
| Untileable input (#429) | Pass 1 tallies, in ONE traversal of the feature bboxes, the #188 antimeridian suspects plus two losses: features outside the declared CRS's coordinate range (`bbox_out_of_crs_range`) and features with valid lon/lat wholly outside the Web Mercator tiling domain (latitude beyond ±85.05°, `bbox_unprojectable`). Both land on `ConvertReport` (`out_of_range_features`, `unprojectable_features`), each warns once, and when together they account for **≥99%** of the input the convert FAILS with `ConvertError::AllFeaturesOutOfRange` instead of writing an empty archive | No CRS gate: GeoJSON is lon/lat by contract, so out-of-range coordinates are clamped or dropped at projection time and a wrong-CRS input yields an empty tileset with exit 0 | The counts are taken at the END OF PASS 1 rather than at the end of convert: pass 1 already holds every feature bbox, so failing there costs no second pass and leaves no half-written overview behind. The gate is a share, not exactly 100%, because a million-row wrong-CRS file with a dozen `POINT(0 0)` placeholder rows would otherwise sail through. The two losses are counted (and worded) separately because their fixes differ: a reprojection for the first, nothing at all for the second — Mercator does not reach the poles. The projected-CRS diagnosis and its `gpio convert reproject` hint are gated on coordinate MAGNITUDE (any offending coordinate above 1000 in absolute value), so one stray 0–360°-convention longitude gets neutral wording rather than an accusation about the whole file |
| Polygon cleanup after tile quantization (export, #383) | Exact integer checks first (every ring simple, no two rings crossing/overlapping — plane sweeps); only a polygon that fails them is repaired: rings noded and split at pinch vertices, pieces regrouped by their own original sense, then an even-odd overlay (bounded rounds) for what still fails; multipolygon parts that actually interact (exteriors meeting, or one part's vertex inside another's fill) are unioned under NonZero, the rest pass through untouched | wagyu positive-fill union on every polygon after snapping (`tile.cpp`), unconditionally | The common case — a clean polygon — is a check, not an overlay, and its vertices come out exactly as snapped (no re-noding or ring rotation). Bowtie lobes: the pinch path keeps the pieces that share the ring's original sense (the larger lobe's, when the net area is zero) and drops the reversed ones — the same lobe wagyu's positive fill keeps; a ring that still crosses after pinch-splitting goes to the even-odd overlay, which keeps both lobes
| PMTiles header zoom range (export, #529/#522/#554) | Header `min_zoom`/`max_zoom` = the zooms that actually hold tiles; a wider requested range (`--min-zoom 0` over levels that generalized to nothing, a pyramid band's declared range, a shard set's union) lives only in `vector_layers[].minzoom`/`maxzoom` | Stamps the requested `-Z`/`-z` range in the header even when the coarse zooms are empty (golden `tests/fixtures/golden/open-buildings.pmtiles`: header z0..z10) | `go-pmtiles verify` rejects a header wider than the directory ("header MinZoom does not match min tile z"). Renderers that build TileJSON from the header (the pmtiles JS `getTileJson`) therefore see the narrower range; `vector_layers` zooms are informational to them. Acceptable: the empty zooms render identically (nothing either way), and an honest `maxzoom` lets MapLibre overzoom from the deepest real tile instead of requesting empty ones. `merge` and the pyramid band check read the declared range as header ∪ `vector_layers` so tylertoo's own archives keep round-tripping |

## Decision Record: MVT Winding Fix + PMTiles Decode (#112, 2026-07-04)

While building the PMTiles → GeoParquet decoder (`decode.rs`), its
spec-strict ring classifier exposed an encoder bug: `orient_polygon_for_mvt`
used geo's `Direction::Default` (exterior CCW in geographic coordinates),
reasoning visually that "geographic CCW appears clockwise after the Y-flip".
Visually true — but MVT spec 4.3.3.3 defines exterior rings by a POSITIVE
surveyor's-formula area on the stored tile coordinates, and a Y-flip NEGATES
that sign. Our exteriors therefore carried negative area (holes positive) —
inverted relative to the spec and to tippecanoe. Fixed to
`Direction::Reversed`; `mvt::tests::test_encoded_exterior_ring_has_positive_tile_area`
pins the convention at the command-stream level. Archives written by older
releases have inverted windings; winding-agnostic renderers (even-odd fill)
draw them correctly, but spec-strict consumers (including our own decoder)
classify their holes as exteriors — re-export to fix.

The decoder itself follows tippecanoe-decode's model: no deduplication
(every feature from every selected tile, with `zoom`/`layer`/`mvt_id`
provenance columns for filtering), coordinates lifted through tippecanoe's
32-bit world-coordinate transform (write_json.cpp), degenerate MVT content
(zero-area rings, one-point linestrings, leading interior rings) dropped.

## Polygon Clipping: Sutherland-Hodgman

**DIVERGENCE**: Tippecanoe uses Sutherland-Hodgman in integer tile
coordinates (0-4096). We use the same Sutherland-Hodgman algorithm but
operate in f64 coordinates to avoid conversion overhead.

**Why Sutherland-Hodgman instead of a general boolean-ops engine:**

- Tile clipping is always against axis-aligned rectangles
- SH is O(n) per polygon ring; Vatti-style engines are O(n log n)
- A 316k-coordinate polygon clips in 0.02s with SH vs 10.4s with Wagyu
  (500x faster)
- SH matches tippecanoe's clip.cpp approach

**Known behavior difference:** SH does not split disconnected clipping
results into separate polygons (a U-shape clipped across its opening yields
one self-touching polygon, not two). Acceptable for tile rendering and
matches tippecanoe. For cases SH cannot handle robustly, `ioverlay_clip.rs`
provides an [i_overlay](https://crates.io/crates/i_overlay)-based fallback
(`clip.rs` dispatches).

### When the i_overlay fallback fires — and the simple-clip fast path (#239)

`clip.rs` routes an SH result to the i_overlay fallback when it detects a
structural issue OR a *boundary-connecting edge* — an SH edge running along the
tile boundary, which is how SH signals it bridged a gap (the #94 U-shape case).
That boundary-edge gate is deliberately coarse: an ordinary polygon straddling a
tile always produces one boundary edge, so the gate over-triggers on ~94% of
fine-zoom polygon clips even though SH was correct.

For a feature whose rings are already **simple**, the over-trigger is pure cost:
the self-touching SH ring is nonzero-winding-equivalent to the i_overlay split —
same enclosed area, same regions filled (verified in `clip.rs` tests
`fastpath_u_render_equivalent` and `fastpath_comb_render_equivalent`, and on the
corpus: identical tile counts at every zoom, geometry differing only by ring
start-vertex rotation). The `simple_clip_fastpath` option (`ExportOptions`,
default `true`) skips the boundary-edge gate for simple features, recovering
that ~94% as wasted work avoided (~18% faster end-to-end on Natural Earth admin
z0–11, up to ~50% on the finest levels). It is gated on per-feature simplicity
(`geometry_is_simple`), so self-intersecting input still takes the fallback and
the #94 fix is preserved.

**Default output note (#256):** the fast path changes output bytes (the SH ring
is stored rotated, not reshaped), so making it the default changed the canonical
tile bytes versus prior releases — render-equivalent, but downstream consumers
that hash raw tiles will see different hashes. It can be disabled with
`--no-simple-clip-fastpath` (`ExportOptions { simple_clip_fastpath: false }`)
when byte-stable output is required. The frozen-hash export anchor in
`export.rs` is unaffected: its fixture polygon never crosses a tile boundary, so
the fast path does not diverge there; the fast path's render-equivalence is
guarded instead by the `clip.rs` `fastpath_*_render_equivalent` tests.

## Input Contract: gpio-Optimized GeoParquet

The converter assumes (and `tylertoo` recommends) input prepared with
[geoparquet-io](https://github.com/geoparquet-io/geoparquet-io): WGS84
(EPSG:4326 — enforced, with a helpful error otherwise), Hilbert-sorted,
bbox-covered, sane row-group sizing. Hilbert order within each level comes
from the sorted-input contract — the pipeline never re-sorts.

## Output Layout: Footer Discipline

The writer suppresses Parquet min/max statistics on the WKB geometry column
and high-cardinality string/binary property columns by default
(`--full-column-stats` opts back in); the bbox covering struct and `level`
column always keep full stats — they are the pruning index. Row groups are
sized **per level** (`--row-group-size` is a per-level cap; levels never
share a row group, spec §4.2). Rationale and numbers: the H1 revision note
in `benchmarks/overview/RESULTS.md` (a 631k-feature file's footer dropped
8.84 MB → 0.24 MB).

## StreamingPmtilesWriter

The writer's archive assembly (sort entries → header + directories + metadata →
place tile data) lives in a non-consuming `write_archive`. `finalize` runs it
once and publishes; `checkpoint` (#229) runs it repeatedly without consuming the
writer. Tile ids are unique, so re-sorting entries between checkpoints is
deterministic — the final bytes are identical regardless of how many
checkpoints ran.

**Two layouts (#459).** Which one a writer uses is fixed at construction.

*Packed* (`new` / `with_temp_dir`): tiles spool to a scratch file in `TMPDIR`
and assembly writes `header | root | metadata | leaves | tile data` into a
sibling `<output>.partial`, then renames it over the target. Each assembly
copies the whole spool, so a run with *n* checkpoints writes the tile data
*n+1* times. Used by `tylertoo merge` and the pyramid builder, which have no
output path to reserve against when the writer is created.

*Tail-directory* (`with_tail_layout`, what export uses): tiles append straight
into `<output>.partial` after a reserved 16 KiB prefix and are **never copied
again**.

```text
[0]      header (127 B)
[127]    root directory
[..]     zero padding        <- the one slack go-pmtiles verify tolerates
[16384]  tile data           <- written once
[..]     json metadata       }  rebuilt each checkpoint; the next tiles
[..]     leaf directories    }  simply overwrite them
```

A checkpoint truncates the old tail, appends a fresh metadata + leaves at the
data end, then rewrites the 16 KiB prefix — O(directory), independent of how
much tile data is on disk. Under the packed layout a planet-scale run
(~100 GB of tiles, a dozen checkpoints) spent hundreds of gigabytes of write
I/O on salvage alone; here it is a few megabytes per checkpoint.

Three things make it legal. Directory entry offsets are relative to
`tile_data_offset`, so moving the section rewrites nothing inside the
directories (dedup back-references included). `make_root_leaves` already sizes
the root against `16384 - 127` and spills into leaves to stay there, so
`header + root` fits the prefix by construction — the writer asserts it anyway
and falls back to the packed layout rather than overrunning into tile data.
And go-pmtiles `verify` (verify.go v1.31.2, L84-89) accepts a file whose length
is either `127 + root + metadata + leaves + tile_data` **or**
`16384 + metadata + leaves + tile_data`, and nothing else: the prefix padding
is the only gap an archive may contain, which is exactly what this layout uses.

The cost is a 16 KiB floor on archive size (a 1.6 KB export becomes 17.8 KB)
and a spool that lives beside the output rather than in `TMPDIR`. Ordering
gives crash-consistency for free: the tail is flushed before the prefix that
points at it, and the prefix lives entirely below offset 16384, so a torn
prefix write cannot touch a tile byte — the next checkpoint rebuilds both.
Symmetrically, the first tile appended after a checkpoint zeroes the prefix's
magic before writing a byte, which is what keeps the salvage file either valid
or *detectably* invalid rather than valid-looking and wrong (see the checkpoint
section above). One `open`+`write` per checkpoint interval, not per tile.

Export's PMTiles v3 writer streams tile data to a temp file, builds the
directory incrementally, and deduplicates tiles by XXH3 hash → file offset,
so writer memory stays in the low MB regardless of tile count. Tiles are
gzip-compressed (the PMTiles-viewer-safe default; export has no compression
knob).

## Module Structure

The overview pipeline is the product; the remaining top-level modules are
the shared infrastructure it builds on.

```
crates/core/src/
├── lib.rs              # Public API surface + Error type
├── overview/           # THE PRODUCT: GeoParquet multi-resolution overviews
│   ├── mod.rs          #   Subtree docs
│   ├── assign.rs       #   Per-level cell-winner thinning + density budget
│   ├── check.rs        #   Spec §6.2 validation (tylertoo validate)
│   ├── cluster.rs      #   Point clustering + attribute accumulation (§12)
│   ├── coalesce.rs     #   Line network coalescing (§13)
│   ├── convert.rs      #   convert_to_overviews() orchestration
│   ├── export.rs       #   Overview GeoParquet → PMTiles export
│   ├── hostile.rs      #   Hostile-input hardening tests
│   ├── level.rs        #   Footer metadata model, SPEC_VERSION
│   ├── reader.rs       #   Overview file reader (level-banded row groups)
│   ├── simplify.rs     #   World-space RDP simplification (GSD tolerance)
│   ├── pipeline.rs     #   Single-read pass-2 engine (#213): fans each batch to all levels
│   ├── plan_state.rs   #   Convert plan artifact (--save-plan/--plan): the persisted
│   │                   #   pass-1 + assignment result, Arrow IPC + fingerprint
│   ├── stream.rs       #   Two-pass streaming orchestration (pass-1 scan; pass-2 → pipeline.rs)
│   └── writer.rs       #   Level-banded GeoParquet writer
├── input.rs            # Input source abstraction: local file or remote
│                       # object (s3/https/gs) via byte-range reads (#210)
├── input_set.rs        # ConvertSource: one file/object or an ordered set of
│                       # partitions read as one dataset (v0.7); owns the
│                       # per-part footer cache, the sequential SourceStream,
│                       # and the pass-2 reader segments (#494)
├── batch_processor.rs  # GeoArrow batch → geo::Geometry decoding
├── clip.rs             # Geometry clipping (dispatcher)
├── ioverlay_clip.rs    # i_overlay-based robust polygon clipping
├── sutherland_hodgman.rs # O(n) polygon clipping for axis-aligned rectangles
├── covering.rs         # bbox covering metadata, row-group bounds
├── tile.rs             # TileCoord, TileBounds
├── world_coord.rs      # Integer world-coordinate space
├── mvt.rs              # MVT encoding
├── decode.rs           # PMTiles → GeoParquet decoding (#112)
├── pyramid.rs          # Multi-band pyramids: merge per-band PMTiles archives
│                       # with disjoint zoom ranges into one archive (#345)
├── shard.rs            # Cut a dataset's tile space into N disjoint shards
│                       # (#498): density-balanced pivot-zoom ranges from
│                       # footer statistics alone, the TileRange a shard's
│                       # convert/export is restricted to, and the small
│                       # checked JSON plan every job of a fleet shares
├── merge.rs            # Concatenate PMTiles archives holding DISJOINT tile
│                       # ids into one, by blob copy — the second half of a
│                       # sharded build (#498). Disjointness is validated
│                       # per tile id as the k-way merge runs (shard id sets
│                       # are disjoint but NOT contiguous ranges);
│                       # overlapping inputs are pyramid.rs's job
├── archive_index.rs    # Read a PMTiles archive's header + directories and
│                       # fetch tile bodies by offset; what lets pyramid.rs
│                       # and merge.rs work on archives bigger than RAM
├── pmtiles_writer.rs   # PMTiles v3 writer (StreamingPmtilesWriter)
├── compression.rs      # gzip/brotli/zstd compression
├── dedup.rs            # Tile deduplication (XXH3)
├── quality.rs          # CRS extraction + WGS84 validation
└── wkb.rs              # WKB round-trip helpers

crates/cli/src/main.rs  # Subcommands: tiles (facade), overview, validate,
                        # export-pmtiles, decode, pyramid, merge, shard-plan
crates/python/src/lib.rs # pyo3 bindings: convert (facade), overview,
                        # export_pmtiles, validate
```
