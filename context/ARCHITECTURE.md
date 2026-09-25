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
2. **Pass 2** reads the input once more and fans each Arrow batch to *all*
   levels at once (the single-read pipelined engine, `overview/pipeline.rs`, #213): a
   reader thread streams batches over a bounded channel while a consumer
   parallelizes every `(level × feature)` simplification across cores; each
   level's output drains to the writer in level order, canonical last. A
   serial engine that re-reads the input once per level is retained as the
   equivalence-tested reference.

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
with a log line, matching how `overview`/`tiles` treat their own outputs; the
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

### Validate (`overview/check.rs`)

`tylertoo validate` checks a file against spec §6.2: footer schema, level
banding/row-group alignment, canonical fidelity, monotonicity, cluster
`point_count` sum invariant (§12.1), coalescing `coalesced_count` rules
(§13), bbox covering.

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

The writer's archive assembly (sort entries → header + directory + metadata →
copy tile data) lives in a non-consuming `write_archive`, written to a sibling
`<output>.partial` and atomically renamed over the target. `finalize` runs it
once then drops the temp file; `checkpoint` (#229) runs it repeatedly without
consuming the writer, so a kill mid-write never corrupts a previously
checkpointed archive. Tile ids are unique, so re-sorting entries between
checkpoints is deterministic — the final bytes are identical regardless of how
many checkpoints ran.

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
├── pmtiles_writer.rs   # PMTiles v3 writer (StreamingPmtilesWriter)
├── compression.rs      # gzip/brotli/zstd compression
├── dedup.rs            # Tile deduplication (XXH3)
├── quality.rs          # CRS extraction + WGS84 validation
└── wkb.rs              # WKB round-trip helpers

crates/cli/src/main.rs  # Subcommands: tiles (facade), overview, validate,
                        # export-pmtiles, decode, pyramid
crates/python/src/lib.rs # pyo3 bindings: convert (facade), overview,
                        # export_pmtiles, validate
```
