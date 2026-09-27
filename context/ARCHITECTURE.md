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
6. **Integer overflow is a bug, never a wrap** (#432): `[profile.release]`
   sets `overflow-checks = true` for the workspace crates, so an unchecked
   expression in our code that overflows aborts in release exactly as it
   does in debug instead of writing wrong bytes. Dependencies are excluded
   (`[profile.release.package."*"] overflow-checks = false`): a third-party
   crate with a latent wrap that is benign for it would otherwise abort the
   binary and surface as a `PanicException` in the Python wheel, and those
   sites are not ours to fix or test. That is the backstop, not the policy: at untrusted-input
   boundaries (CLI values, PMTiles/plan/GeoParquet bytes) arithmetic on
   user-controlled numbers uses `checked_*` / `saturating_*` and returns a
   typed error, because a panic there is still a denial of service. Shifts
   are masked regardless of this setting, so a shift amount derived from
   input (zoom levels) needs its own range check.

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
   occupied cell either way, so the #306 grid estimate still describes the
   grids. The one new term is the pending placements between shard reduces,
   which the estimate does not count; it is bounded instead, per wave rather
   than per level (≈ 2 MiB per pool thread across the whole wave, floor 1 MiB
   per level, exact-size buffers), because the planner packs many tiny coarse
   levels into one wave. The wave's winner fold splits by position range, and
   the density budget's candidate scan and super-cell partition go wide while
   its admission fold (which threads a running kept count coarse→fine) stays
   serial. All of it is scheduling: the sharded reduce preserves position
   order per cell, so every cell resolves its contests exactly as the serial
   build does, and the budget's sorts are by a strict total order. The unit
   test `oracle_fixture_reproduces_main` holds a 150k-feature adversarial
   fixture — above the sharded-build and parallel-sort cutoffs — to golden
   digests produced by the pre-#534 serial code, at 1/2/7/16 threads and
   unbounded vs. 1-byte grid budgets; `thread_count_determinism.rs` checks the
   end-to-end artifact across thread counts, though its fixtures are below the
   sharded-build size.
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
with a log line (unlike the subcommand outputs themselves: `overview`,
`tiles`, `export-pmtiles` and `decode` all refuse an existing output unless
given `-f/--force`, #427); the
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

**GeometryCollections and encode-time drops (#431).** An MVT feature is
single-type (spec §4.3.4), so a `GeometryCollection` member cannot be one
feature. `LayerBuilder::add_feature` flattens it (recursively) with
`mvt::flatten_geometry_collection` and writes one feature per geometry kind
present — every polygonal part as one `MultiPolygon`, every linear part as one
`MultiLineString`, every point part as one `MultiPoint`, in that draw order —
each carrying the collection's id and tags (the tags are interned once).
`Line`, `Rect` and `Triangle` encode as the line/polygon they are. Anything
that still produces no feature is tallied by the builder in one of two
counters, split on whether the input had coordinates at all:
`dropped_features` — nothing to encode (an empty geometry, an empty
collection) — and `quantized_features` — coordinates that collapse at the
tile extent (a polygon ring of zero area, a line of fewer than two points;
routinely a clip sliver at a buffered tile edge). They are summed per zoom
into `ZoomReport`/`ExportReport::encode_dropped_features` and
`encode_quantized_features`. Only the unencodable count is content loss: it
is named by one aggregate `log::warn!` at the end of the export (the #429
pattern, so the CLI's `env_logger` shows it) and qualifies the CLI summary
line. The quantized count is expected on any polygon export (open-buildings
and fieldmaps-boundaries both produce a few) and is printed as a separate
informational note, never a warning. A tile whose members all encode to
nothing is not written; before #431 the collection fell through
`encode_geometry`'s catch-all and the empty-commands early return with no log
and no counter. Note that `clip::clip_geometry` still passes a collection
through unclipped on a bbox intersection (its parts are not clipped
individually), so a seam-crossing collection's parts extend past the tile
buffer; the #406 coordinate clamp keeps that encodable.

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

Every divergence below has a measured footprint: `.github/workflows/tippecanoe-compare.yml`
runs `benchmarks/e2e/compare_tippecanoe.py` (#420) on every PR and weekly,
tiling the fixtures-v1 inputs with tylertoo (quality-matched: `--verbatim
--simplify-factor 1.0`) and pinned tippecanoe 2.79.0 under matched flags, and
gating the per-zoom tile / feature / distinct-id / vertex / byte ratios
against the bands in `benchmarks/e2e/tippecanoe_tolerances.toml`. The bands
are a ratchet: each carries the measured ratio and names the divergence that
explains it (sub-pixel handling, simplification units, the y-axis buffer,
empty coarse tiles). Closing a divergence in this table moves a ratio and
therefore its band, in the same PR, with the new measurement in the comment.
`corpus/METRICS.md` §2 defines what is compared.

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
| Polygon clipping (export) | Sutherland–Hodgman in f64 degrees, gated: a ring the O(n) checks or the #241 sweep call non-simple, or an S-H result with a boundary bridge (unless the #239 fast path applies), goes to the direct `i_overlay` 9 instead (`clip.rs` → `ioverlay_clip.rs`); lines clip on that same `i_overlay` (#435); after tile quantization the #383 repair runs on it too. Evaluated against wagyu-rs in #205 and kept (decision record below) | Sutherland–Hodgman in integer tile coords, then a wagyu positive-fill union of every polygon after snapping (`tile.cpp`) | Same clip algorithm, different coordinate space and a different cleanup engine. Fully-inside fast path is gated on a bbox over EVERY ring (#205): `geo`'s `Polygon::bounding_rect` is exterior-only, and an interior ring outside its exterior used to ride the fast path into the tile unclipped |
| Tile buffer axis (export, #341) | `--tile-buffer` is converted to degrees from the tile's LONGITUDE width (`tile_width x buffer_px / 256`) and that one value is applied to both axes | `--buffer` is tile pixels on both axes | Exact on x; on y the effective buffer is `buffer_px x sec(lat)` pixels, because a Mercator tile's latitude span shrinks as `cos(lat)` while its longitude span does not — 8 px at the equator, ~16 px at 60 deg, ~92 px at 85 deg. Bounded: the over-draw stays under one tile height below ~88.2 deg, outside the Mercator domain, and it errs towards carrying MORE geometry across the seam. A per-axis buffer has to be threaded through `bbox_within_buffered` and `clip_geometry_simple` as well, which moves tile bytes again, so it is deferred. Antimeridian seam continuity is separately out of scope: membership widening is clamped to the lon/lat domain and a feature at lng 179.99 does not reach tile x=0 |
| Tile buffer cap and extent validation (export, #433) | `--tile-buffer` is capped at `MAX_TILE_BUFFER_PX` = 256 (one full tile width) and `extent` must be positive (non-power-of-two warns), both checked by `ExportOptions::validate` before any I/O; `decode` refuses a layer declaring `extent: 0` | `--buffer` has no documented upper bound; the extent is set as a power of two through `-d`/`-D`/`-m` detail bits, so zero is unreachable there | A buffer past one tile width has no output that is not reachable below it, and since the buffer is what makes a feature belong to more than one tile, an unbounded one is an `O(features × tiles)` blow-up (`--tile-buffer 100000` ≈ 390 tile widths). `extent = 0` quantizes everything to the origin and writes a layer every consumer divides by. The sharded read-pruning margin (two pivot tiles = 512 px, #498) is wider than the cap, and a `const` assertion in `shard.rs` keeps it so; `ExportError::TileBufferTooWideForShard` is retained for API compatibility but no longer raised |
| Untileable input (#429) | Pass 1 tallies, in ONE traversal of the feature bboxes, the #188 antimeridian suspects plus two losses: features outside the declared CRS's coordinate range (`bbox_out_of_crs_range`) and features with valid lon/lat wholly outside the Web Mercator tiling domain (latitude beyond ±85.05°, `bbox_unprojectable`). Both land on `ConvertReport` (`out_of_range_features`, `unprojectable_features`), each warns once, and when together they account for **≥99%** of the input the convert FAILS with `ConvertError::AllFeaturesOutOfRange` instead of writing an empty archive | No CRS gate: GeoJSON is lon/lat by contract, so out-of-range coordinates are clamped or dropped at projection time and a wrong-CRS input yields an empty tileset with exit 0 | The counts are taken at the END OF PASS 1 rather than at the end of convert: pass 1 already holds every feature bbox, so failing there costs no second pass and leaves no half-written overview behind. The gate is a share, not exactly 100%, because a million-row wrong-CRS file with a dozen `POINT(0 0)` placeholder rows would otherwise sail through. The two losses are counted (and worded) separately because their fixes differ: a reprojection for the first, nothing at all for the second — Mercator does not reach the poles. The projected-CRS diagnosis and its `gpio convert reproject` hint are gated on coordinate MAGNITUDE (any offending coordinate above 1000 in absolute value), so one stray 0–360°-convention longitude gets neutral wording rather than an accusation about the whole file |
| Polygon cleanup after tile quantization (export, #383) | Exact integer checks first (every ring simple, no two rings crossing/overlapping — plane sweeps); only a polygon that fails them is repaired: rings noded and split at pinch vertices, pieces regrouped by their own original sense, then an even-odd overlay (bounded rounds) for what still fails; multipolygon parts that actually interact (exteriors meeting, or one part's vertex inside another's fill) are unioned under NonZero, the rest pass through untouched | wagyu positive-fill union on every polygon after snapping (`tile.cpp`), unconditionally | The common case — a clean polygon — is a check, not an overlay, and its vertices come out exactly as snapped (no re-noding or ring rotation). Bowtie lobes: the pinch path keeps the pieces that share the ring's original sense (the larger lobe's, when the net area is zero) and drops the reversed ones — the same lobe wagyu's positive fill keeps; a ring that still crosses after pinch-splitting goes to the even-odd overlay, which keeps both lobes
| PMTiles header zoom range (export, #529/#522/#554) | Header `min_zoom`/`max_zoom` = the zooms that actually hold tiles; a wider requested range (`--min-zoom 0` over levels that generalized to nothing, a pyramid band's declared range, a shard set's union) lives only in `vector_layers[].minzoom`/`maxzoom` | Stamps the requested `-Z`/`-z` range in the header even when the coarse zooms are empty (golden `tests/fixtures/golden/open-buildings.pmtiles`: header z0..z10) | `go-pmtiles verify` rejects a header wider than the directory ("header MinZoom does not match min tile z"). Renderers that build TileJSON from the header (the pmtiles JS `getTileJson`) therefore see the narrower range; `vector_layers` zooms are informational to them. Acceptable: the empty zooms render identically (nothing either way), and an honest `maxzoom` lets MapLibre overzoom from the deepest real tile instead of requesting empty ones. `merge` and the pyramid band check read the declared range as header ∪ `vector_layers` so tylertoo's own archives keep round-tripping |
| `--max-zoom auto` (#444) — inspired by `-zg`, not a port | `overview::auto_zoom` (`MaxZoom::resolve`): a deterministic systematic sample of at most 200k rows (`AUTO_ZOOM_SAMPLE_CAP`) from the row groups `--bbox`/`--filter` select, each sampled row's bbox read from the GeoArrow scalar (no `geo::Geometry` conversion; non-sampled rows are never converted), then the per-feature bbox/filter tests. Signals, in Web Mercator meters (EPSG:4326 latitude deltas scaled by `1/cos φ`): the **median bbox diagonal** over features with a positive extent, and the **10th percentile** of Morton-order gaps between distinct centers (× `sqrt(sample fraction)` for subsample thinning). `resolvable = min(...)`; the zoom is the finest whose **256 px** tile pixel is ≤ `resolvable / 2`, clamped to `[min_zoom, 16]`. No signal → `ConvertError::AutoZoomNoSignal`; `min_zoom > 16` → `AutoZoomMinAboveCeiling` | `-zg` (`main.cpp`, the `guess_maxzoom` block, ~L2278–2440): sorts features by a 32-bit quadkey index and takes the log of consecutive **index deltas** (Welford mean/σ), `nearby = exp(mean − 1.5σ)` converted to "feet" empirically (`sqrt(nearby) / 33`), halved ("one zoom level beyond"); lines/polygons add the **geometric mean of intra-feature vertex spacing / 8** (`want2 = exp(dist_sum / dist_count) / 8`), which only raises the zoom. Both resolve at the full **4096-unit** tile extent: `ceil(log2(360 / want_deg) − full_detail)`, `full_detail = 12`; capped at `32 − full_detail` (z20). With no distinct locations and no vertex spacing it exits: "Can't guess maxzoom (-zg) without at least two distinct feature locations" (unless `--smallest-maximum-zoom-guess`) | Every measurement differs, deliberately. (1) Resolution: tippecanoe resolves `want` at 4096 units/tile, which lands 4 zooms finer than resolving the same distance at 256 px; that is what makes `-zg` archives famously deep. We resolve "half the typical detail" at 256 px — an archive a feature is legible in, not one that preserves vertex spacing — and cap at z16 rather than z20. (2) Extent vs vertex spacing: a median bbox diagonal is one scalar per feature from the sampled bbox, while vertex spacing needs every coordinate of every feature; median + positive-only keeps points from zeroing it (mixed inputs). (3) Spacing: a percentile of real Morton-ordered center gaps in meters instead of log index deltas and an empirical feet conversion. (4) Footer-only statistics (whole-file bbox + row count) were measured and rejected: on `open-buildings` they imply ~566 m spacing vs a ~28 m median building, a >20× error from how populated the bbox is. (5) Estimated before the conversion rather than from pass 1's own bbox scan, so `LevelPlan`/`check_zoom_ceiling` (#371) keep a concrete zoom; the cost is one extra read of the geometry column of the selected row groups (bounded conversion, not bounded I/O — a GeoParquet bbox covering column could make it cheaper; follow-up). Measured picks: madagascar-adm4 z8, road-detections z13, open-buildings z14 (pinned in `real_fixtures_choose_pinned_zooms`) |
| GeometryCollections (export, #431) | Split at MVT encode (`LayerBuilder::add_feature`) into at most three single-type features per tile — one `MultiPolygon`, one `MultiLineString`, one `MultiPoint` — with the collection's id and tags; nested collections flatten; `Line`/`Rect`/`Triangle` encode as line/polygon. Whatever still encodes to nothing is counted — `encode_dropped_features` (nothing to encode: empty geometry or collection; warned about) and `encode_quantized_features` (collapsed at the tile extent; expected, informational) | Splits a GeoJSON `GeometryCollection` at **read** time into one feature **per member** with the same id and properties (`read_json.cpp`, `serialize_geojson_feature`; cited from memory — the tippecanoe source is not vendored here) | Grouping by kind emits fewer features and shares a `--feature-id` across at most three of them instead of one per member; the rendered content is the same. Splitting at encode rather than at read keeps the overview file's `GeometryCollection` rows intact (the convert already carries them through, `hostile.rs::geometry_collection_passes_through`) |
| Stable MVT feature ids (export, `--feature-id`, #443) | Only integer (`Int8`..`Int64`, `UInt8`..`UInt64`) and `DECIMAL(p,0)` columns are accepted; strings, floats, scaled decimals and dictionary-encoded columns are rejected at resolution. Every row of the overview file (all levels) is checked **up front, before the output is created**, in one column-projected read that skips row groups whose Parquet statistics prove validity (zero nulls, non-negative min); a null, negative or >`u64::MAX` value is `ExportError::InvalidFeatureId` naming the overview-file row (not the source row: the convert reorders) and its level. Also rejected: the `--feature-order` column and any `--accumulate-attribute` column (from the file's clustering provenance at export; from the flags on `tiles` before the convert). Aggregates: a cluster carries its representative's id, a coalesced chain its highest-priority member's, a #384 carrier its own; duplicates are not checked | `--use-attribute-for-id` accepts any numeric attribute, parsing numeric strings and integral doubles; it only *warns* on a non-numeric or fractional id, and a numeric string with a leading `-` goes through `strtoull`, whose two's-complement reinterpretation is accepted whenever printing it back reproduces the input (`serial.cpp`), so `"-5"` silently becomes id `18446744073709551611` | Issue #443 asks for hard errors instead of that silent reinterpretation. Strings are not parsed because the common string id (Overture GERS) has no lossless `u64` form, and a float column's integrality is a per-value accident: cast or hash to an integer with `gpio`/DuckDB first. Matched: the column is moved to the feature id and never *also* published as a property (mapbox/tippecanoe#738), independent of `--include-property`/`--exclude-property` naming it. The no-flag default (tile-local `i as u64` in `build_mvt`) is unchanged pending #443's open question (omit the id entirely?). Follow-ups: `pyramid` has no `--feature-id`, and the deprecated Python `convert()` facade hard-codes `None` |

## Decision Record: Geometry-Engine Dependency Bumps (#558, 2026-09-26)

### i_overlay 9.0 changed clipping output — main's baseline moved at 5c123fa

**Recorded as fact, and flagged for maintainer sign-off: nobody has yet
ratified this as the intended behavior.**

`5c123fa` (merged as #539) bumped `i_overlay` 8.1.2 → 9.0.0 (pulling `i_float`
4 → 5). That is **our own direct dependency**, not `geo`'s: `geo` 0.33 vendors
`i_overlay` 4.5.2 for its `BooleanOps`, and both versions coexist in
`Cargo.lock` (since #435 nothing of ours calls the vendored one — see "One
boolean-ops engine" below). Ours is the engine behind `ioverlay_clip.rs` — the
boundary-bridge fallback in `clip_geometry` (`clip.rs`, reached on every
non-simple ring and, with `--no-simple-clip-fastpath`, on all of them), the
line clipper (#435), and the #383 post-quantization polygon repair in
`export.rs`. So the bump moved the clipper directly. A three-way A/B on the
Brazil 2025 coarse job (27.98M rows, 588 tiles, identical shard plan and
options) measured:

| comparison | differing tiles |
|---|---|
| `main@6b9c3ff` vs `main@b64daf8` (the #539 merge) | **18** |
| `main@b64daf8` vs `main` after #542/#549/#550/#554 | **0** |

The 18 tiles carry ±1-MVT-unit coordinate shifts in clipped boundaries across
z5–z8, correspondingly different areas, and one small polygon flipping across
its collapse threshold (tilestats −1 of 2.5M). Everything merged after the bump
is byte-clean.

The new behavior is **main's baseline as of 5c123fa** — that is a statement
about what the tree does, not an endorsement. The case for accepting it is that
i_overlay 9 is a correctness-oriented major release and the delta is ~3 features
in 1.7M at z8; the case against has not been made, because the change was never
noticed at merge time. The golden committed in
`tests/fixtures/guard/br-clip-divergence.golden.txt` encodes the post-bump
output, so accepting it is now also the default outcome of doing nothing.
A maintainer deciding otherwise should say so on #558 and the golden should be
regenerated from whatever they decide.

### Policy: geometry-engine bumps are not green-CI merges

A bump to `geo`, `geo-types`, `i_overlay`, `i_float`, `i_shape` or `earcut` can
change rendered geometry at **any** semver level. These are *behavioral* inputs
to every tile we write, not build details, and there are two routes in: our
direct `i_overlay` (the clip fallback and the #383 repair) and the copy `geo`
vendors for its own `BooleanOps`/triangulation — so a `geo` bump can move
clipping without `i_overlay` appearing in the diff at all. Such a bump must not
be merged on green CI alone, nor auto-merged. `dependabot-automerge.yml`
already leaves majors for a human; that is not the safeguard, because a patch
release of the same crate would be auto-merged and can move coordinates just as
easily.

The mechanical enforcement is the golden tile guard
(`crates/core/tests/convert_guard_golden.rs`): it pins per-tile digests of a
full `convert` → `export` build over real clipper-stressing geometry, and **fails on any output change**, so a bump
that moves a coordinate cannot be green. A golden diff is therefore the signal
to stop and decide, and a PR that carries one must say in its body why the
output moved. Discrimination is verified, not assumed: rolling `i_overlay` back
to 8.1.2 fails it on 4 of 384 guarded tiles (`8/96/138`, `8/97/134`, in both
cases) — a thin margin, recorded with the recipe in
`tests/fixtures/guard/README.md`.

What makes it a gate is where it runs. It is in the slow set in
`.config/nextest.toml`, so it runs in `Slow Tests (ubuntu-latest)` and
`Slow Tests (macos-latest)` (`ci.yml`) — both **required** checks in `main`'s
branch protection (verified). The `Convert regression guard` job in
`bench.yml` is *not* a required check, so it does not gate merges and the
golden does not run there. If the golden ever leaves the nextest slow set, or
Slow Tests stops being required, the guard silently stops gating.

Why the pre-existing guard did not do this, though it ran (green) on #539:
`benchmarks/overview/ci_guard.py` runs `tylertoo overview` and compares
per-level feature/vertex counts. Clipping happens in export, which that guard
never runs, and counts are blind to coordinates that move without appearing or
disappearing — the exact shape of an overlay-engine change. The two guards run
in different places (structural in `bench.yml`, golden in Slow Tests); neither
replaces the other (the structural one is cheap, covers three geometry classes,
and catches pass-1 regressions the golden's single fixture does not).

### One boolean-ops engine: line clipping moved to the direct i_overlay (#435)

Until #435 the two clippers ran on two engines. Polygons went through
`ioverlay_clip.rs` on our direct `i_overlay` (9.0); lines went through
`geo::BooleanOps::clip` in `clip.rs`, which is `geo` 0.33's vendored
`i_overlay` 4.5.2 — four major versions of robustness fixes apart, so a line
and a polygon sharing a tile edge could in principle be cut by engines that
disagree about where that edge is (the inconsistency class #205 is about),
plus duplicate compile and binary weight for `i_overlay`/`i_float`/`i_shape`/
`i_tree`. `clip.rs` now calls `ioverlay_clip::clip_multilinestring_ioverlay`,
the same operation on the direct engine (`FloatClip::clip_by`,
`FillRule::EvenOdd`, `ClipRule { invert: false, boundary_included: true }`,
open clip contour — exactly what `geo`'s `clip_with_fill_rule` did). No code
of ours references `BooleanOps` any more.

**Behaviour:** pinned before the switch in `crates/core/tests/line_clip_pinned.rs`
through the public `clip::clip_geometry` — 17 hand-built cases with literal
coordinates (double crossings, corner grazes, edge-collinear runs, boundary
vertices, degenerate segments, bbox-overlap-only) and a digest sweep over the
`road-detections` fixture at z12 and z14. Every hand-built case is
coordinate-identical on the new engine, including the old engine's quirks (a
re-entering run broken at an interior vertex, parts emitted out of input
order, consecutive duplicates collapsed, zero-length lines dropped). The
real-data sweep is structurally identical (same kept/part/vertex counts) but
not bit-identical: about half the vertices differ by **exactly one unit of
i_overlay's float→integer grid** (2⁻³⁴ ° at the z12 tile scale, 2⁻³⁵–2⁻³⁶ ° at
z14; max 2.3e-10 °), in y only, at interior vertices as well as boundary
intersections. Neither engine returns the input vertex bit-exactly — the old
path was already snapping every line vertex to that grid; the new one lands
on the adjacent cell. That is four to five orders of magnitude below one MVT
unit at z14 (≈5.4e-6 °), so a quantized tile coordinate can only move on an
exact rounding tie. An end-to-end A/B (main vs this branch, `tiles` over `road-detections.parquet`, z0–z14, 78 tiles) produced byte-identical archives. The sweep digests in `line_clip_pinned.rs` were
re-pinned to the new engine for this reason; the counts did not change. The
counts are pinned on every platform; the bit-exact digest only on Linux,
because tile bounds come from `sinh().atan()` and libm differs by an ulp
across platforms, which reaches the clipped intersections (macOS produced a
different z14 digest with identical counts). A grid-rounded digest was
rejected: over ~12k coordinates a 1-ulp shift straddles a rounding boundary
often enough to flake.

**Input guard:** i_float 5's adapter panics ("Invalid adapter bounds") when
the subject's extent is non-finite or a coordinate magnitude exceeds its
documented f64 limit of 2^500, where the `geo` path returned an empty
result. `clip_multilinestring_ioverlay` checks every coordinate (finite and
below `IOVERLAY_MAX_ABS_COORD` = 1e150) and returns empty before calling the
engine, so a NaN/inf/1e300 vertex still yields "nothing to clip" rather than
a panic. The polygon entry points (`clip_polygon_ioverlay` and friends) are
not changed by #435 and carry no such guard; whether they need one is a
separate question.

**Residual duplicate:** `i_overlay` 4.5.2 stays in `Cargo.lock` because `geo`
0.33.1 (the latest release at the time of writing) depends on it
(`i_overlay = "4.5.1, < 4.6.0"`) for its own `BooleanOps` and triangulation.
It leaves the lockfile only when a `geo` release depends on `i_overlay` 9 and
we take that bump — which, per the policy above, is a golden-guarded decision,
not a green-CI merge. Nothing of ours calls the vendored copy now, so that
future bump cannot move line or polygon clipping through `geo`; it could still
move `earcut`/triangulation and the `Simplify`/`Area` family, which is why the
policy stays.

**Compression duplicates (same PR):** `brotli` and `zstd` were pinned at 9 and
0.14 while `parquet` 59 needs 8 and 0.13, so both were compiled twice.
`compression.rs` uses only the stable surface (`Decompressor::new`,
`CompressorWriter::with_params`, `BrotliEncoderParams.quality`, `encode_all`,
`stream::read::Decoder`), which is identical across those majors, so core now
requires `brotli = "8"` and `zstd = "0.13"` and follows `parquet`'s versions.
When `parquet` moves, move these with it.

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

## Polygon Clipping: Sutherland-Hodgman + i_overlay

**DIVERGENCE**: Tippecanoe uses Sutherland-Hodgman in integer tile
coordinates (0-4096), then cleans every polygon with a wagyu union. We use
the same Sutherland-Hodgman algorithm in f64 degrees, and route to the
direct [i_overlay](https://crates.io/crates/i_overlay) 9 only where S-H is
known to be wrong.

The export clip path today (`clip::clip_geometry_simple`, called per tile
from `overview::export`):

1. **Bbox gates.** Reject when the feature's bbox misses the buffered tile;
   return the geometry verbatim when the bbox is inside it. Both bboxes
   cover EVERY ring — `geo`'s `Polygon::bounding_rect` is exterior-only,
   and until #205 an interior ring outside its exterior (invalid input the
   convert pass carries verbatim, #188) rode the fast path into the tile
   unclipped, thousands of MVT units past the buffer.
2. **Input validity.** O(n) degenerate/duplicate-vertex checks plus, unless
   the feature was proven simple once per feature (`geometry_is_simple`,
   #237), the O((n+m) log n) self-crossing sweep (#241). A ring that fails
   goes straight to `ioverlay_clip::clip_polygon_ioverlay`
   (`OverlayRule::Intersect`, `FillRule::EvenOdd`).
3. **Sutherland–Hodgman** (`sutherland_hodgman::clip_polygon_sh`), exterior
   and holes clipped ring by ring.
4. **Output gate.** The S-H result is re-checked (step 2's cheap checks,
   plus the sweep for non-simple features) and, unless the #239 fast path
   applies, for edges running along the tile boundary — the U-shape bridge
   (#94). Either sends the ORIGINAL polygon to i_overlay.
5. **Lines** clip on the same i_overlay (`clip_multilinestring_ioverlay`,
   #435), so one engine decides where the tile edge is for every type.
6. **After quantization** to the MVT grid, `mvt.rs` runs the #383 cleanup:
   exact integer checks first, and only a polygon that fails them is
   repaired through `ioverlay_clip::repair_polygon_ioverlay`.

**Why Sutherland-Hodgman first instead of a general boolean-ops engine:**

- Tile clipping is always against axis-aligned rectangles
- SH is O(n) per polygon ring; Vatti-style engines are O(n log n)
- The 316k-vertex Antarctica ring clips in ~1 ms with SH vs ~63 ms with
  i_overlay and ~10 s with wagyu-rs (`benches/hostile_geometry.rs`,
  `corpus/HOSTILE_GEOMETRY.md`)
- SH matches tippecanoe's clip.cpp approach

**Known behavior difference:** SH does not split disconnected clipping
results into separate polygons (a U-shape clipped across its opening yields
one self-touching polygon, not two). Acceptable for tile rendering and
matches tippecanoe. For cases SH cannot handle robustly, `ioverlay_clip.rs`
provides the i_overlay fallback (`clip.rs` dispatches).

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

## Decision Record: Clipping engine (#205, 2026-09-27)

**Decision: keep the current pipeline — Sutherland–Hodgman first, the direct
`i_overlay` 9 as the fallback, line clipper and #383 repair engine. wagyu-rs
0.2.1 is not adopted, neither as the fallback nor as a post-quantization
cleanup; the hybrid is moot while the challenger fails the basic oracles.**

**Method.** `crates/core/tests/hostile_geometry_eval.rs` runs every candidate
over the same inputs at three zooms per fixture with export's 8 px buffer and
scores each (case × engine) with the same oracles: caught panics, empty
output where the i_overlay reference has area (drops) and the reverse
(phantoms), the largest distance any output vertex lies past the buffered
bounds in MVT units, `geo::Validation` on the output, proper self-crossing via
the production sweep, ring-orientation consistency, area against the
i_overlay reference (with `geo::BooleanOps` — geo's own vendored i_overlay
4.5 — as the third opinion on valid input), area against a valid input's own
area, vertex count, wall time and peak heap. Corpora: the 76 files of
chrieke/geojson-invalid-geometry (35 usable geometries; the 41 structurally
broken files are listed, not skipped silently), a 32-shape synthetic suite
(bowties, spikes, holes crossing or outside their exterior, degenerate and
unclosed rings, combs and U-shapes across the tile edge, antimeridian and
polar rings, sub-MVT-unit slivers, huge and tiny coordinates), and the
316k-vertex Antarctica ring plus the Tielt-Winge admin polygon. 2,289 cases.
wagyu-rs runs out of process (`corpus/hostile_wagyu`) in two integer spaces —
the tile's 4096-unit MVT grid and tippecanoe's `2^(32-z)` world grid — because
its dead `geo 0.32` dependency cannot resolve beside our geo 0.33
(nlebovits/wagyu-rs#113), and because `Wagyu<f64>` snap-rounds input to
integers, so a degrees-as-is column would score a misuse. The adapter is
pinned by `corpus/hostile_wagyu/tests/adapter_sanity.rs`. Full tables and
commentary: `corpus/HOSTILE_GEOMETRY.md`.

**Scorecard headline** (2,289 cases; production = export's default path):

| engine | panics | drops | past buffer >½ MVT (max) | invalid output | self-crossing | Antarctica clip |
|---|--:|--:|--:|--:|--:|--:|
| production (S-H + gated i_overlay) | 0 | 1 | 0 (0.00) | 162, all on invalid input | 0 | ~1.5 ms |
| production-strict (fast path off) | 0 | 1 | 0 (0.00) | 15, all on invalid input | 0 | ~63 ms |
| i_overlay 9 alone | 0 | 0 | 0 (0.00) | 0 | 0 | ~63 ms |
| wagyu-rs 0.2.1, MVT grid | 0 | 120 | 43 (156,678) | 35 | 27 | ~10 s |
| wagyu-rs 0.2.1, world grid | 9 (infinite-loop detector on Antarctica) | 94 | 119 (157,321) | 30 | 24 | ~20 s |

The one production drop is a z21 tile lying in the lobe of a hole that
crosses its exterior: S-H (like tippecanoe's positive fill) treats the hole
as subtractive and emits nothing, even-odd fills it. A semantic choice on
invalid input, not a defect. Production's "invalid" outputs are S-H results on
invalid input — a hole clipped along the same boundary as its exterior
("intersect on a line"), self-touching rings, an input's own overlapping parts
passed through — that the encoder's even-odd/nonzero fill renders as the
unclipped invalid polygon would render; none has a proper self-crossing and
none reaches past the buffer.

**What the challenger would have had to show** to displace i_overlay: no
panics, no output past the buffer, OGC-valid output where the incumbent is
merely render-correct, and a cost within an order of magnitude on the
pathological tier. wagyu-rs 0.2.1 emits vertices beyond both its subject and
its clip box, inflates a valid polygon's area 13× (`invalid_interior_not_cw`),
produces self-crossing rings, trips its own infinite-loop detector on a real
316k ring in tippecanoe's own clip space, and is 160–320× slower than i_overlay there (four orders of magnitude slower than S-H). Both
correctness findings are filed upstream (nlebovits/wagyu-rs#114) with the
reproducer.

**Issue #205's three gaps, in this light.** (1) Output validity: i_overlay's
even-odd intersection produced OGC-valid output on every one of the 2,289
cases, so on this corpus the "renders correctly, not valid" gap did not
materialize for the fallback; the production path's invalid outputs come from
S-H on invalid input, by design (#239). (2) Quantization-induced invalidity:
the #383 repair already runs on the integer grid after snapping; the
integer-space clipper that would close the class by construction is the one
that failed the evaluation, so #383 stays. (3) Antimeridian: the
antimeridian and polar rings clipped identically on every engine (no engine
splits; all smear, as pinned by `overview_hostile`), so an export-time split
is a separate feature with no engine dependency and stays deferred per the
#188 decision.

**Production defect found and fixed here:** the fully-inside fast path in
`clip_polygon`/`clip_multipolygon` gated on `geo`'s exterior-only
`bounding_rect`, so a polygon whose exterior sat inside the tile came back
verbatim with any interior ring it carried — including one wholly outside
the exterior and the tile (4,904 MVT units past the buffer on the synthetic
case). Fixed by gating on a bbox over every ring
(`clip::polygon_rect_all_rings`); byte-identical for valid input, where holes
lie inside the exterior. Regression tests in `clip.rs`
(`hole_outside_its_exterior_is_clipped_not_fast_pathed`,
`hole_crossing_its_exterior_is_clipped_to_the_window`,
`valid_polygon_with_holes_still_takes_the_fast_path_verbatim`); the golden
guard (`convert_guard_golden`) and the line-clip pins are unchanged.

**Limitations noted, not fixed.** `clip::has_structural_issues` uses an
absolute 1e-10° duplicate-vertex epsilon, so a polygon under ~1e-9° across
(0.1 mm) is routed to i_overlay and picks up its grid rounding (4.7 % area
on a 1e-12° square; 0.005 MVT units at z22 — invisible). i_overlay's
float-to-integer grid is set by the joint bbox of subject and clip, so a
sliver 1e-6 of a tile wide sees ~0.1 % area noise (sub-pixel). i_overlay
returns empty for coordinates around 1e15 (outside the lon/lat domain; #429
rejects such input at convert), where S-H clips them fine. `geo::Validation`
is quadratic on a ring's self-intersection check, so the harness reports
outputs over 20k vertices as "unchecked" rather than scoring them.

**Re-run:** `cargo test -p tylertoo-core --test hostile_geometry_eval`
(quick tier: corpus + synthetic, asserts the production invariants — no
panics, nothing past the buffer, no area gain on valid input, no
self-crossing on simple input); `full_scorecard` (slow set) adds the real
fixtures and writes `target/hostile_geometry_eval/SCORECARD.md`;
`corpus/hostile_wagyu/README.md` for the wagyu columns;
`cargo bench -p tylertoo-core --bench hostile_geometry` for timings.

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
