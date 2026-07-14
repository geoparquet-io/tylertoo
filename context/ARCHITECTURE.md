# gpq-tiles Architecture

Design decisions and tippecanoe divergences for the **current** system.
Historical material (the removed per-tile pipeline, execution plans,
session triage) lives in [`context/archive/`](./archive/README.md).

Related canonical documents:

- **Format**: [`context/OVERVIEWS_SPEC.md`](./OVERVIEWS_SPEC.md) — the
  `geo:overviews` draft spec (single source of truth for the file format).
- **Tuning**: [`docs/OVERVIEW_TUNING.md`](../docs/OVERVIEW_TUNING.md) — every
  generalization knob, its default, and its direction.
- **Benchmarks**: [`benchmarks/overview/RESULTS.md`](../benchmarks/overview/RESULTS.md)
  (storage/access numbers) and
  [`benchmarks/overview/PROFILE.md`](../benchmarks/overview/PROFILE.md)
  (performance methodology + history).

## Decision Record: Legacy Tiles Pipeline Removed (#177, 2026-07-03)

The legacy per-tile pipeline (`pipeline.rs`, `Converter`, the streaming
external-sort/bucketed tiler and its quality features) was **removed**. The
overview pipeline (`overview convert` → `export-pmtiles`) supersedes it for
the project's core workflow: it is faster (Moldova full pipeline < 2 min),
memory-bounded (convert ~0.4 GB for a z0–6 pyramid, ~1.4 GB for z0–14; export ~0.89 GB), and carries the quality
ladder (ranking, density budget, clustering, coalescing) the tile path never
got. See `context/TILE_SIMPLIFY_POSTMORTEM.md` for why the tile-path quality
work had already been excised.

What survives:

- **The `tiles` CLI subcommand** (and the bare `gpq-tiles in.parquet
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
[`context/archive/LEGACY_TILES_ARCHITECTURE.md`](./archive/LEGACY_TILES_ARCHITECTURE.md).

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

The one safety valve is `--tile-size-limit`: an oversized tile gets a
**single, non-iterative** drop pass shedding its largest-geometry features,
then is re-encoded once.

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

`gpq-tiles validate` checks a file against spec §6.2: footer schema, level
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
| Point clustering | Winner **keeps its own geometry** and absorbs cell losers into `point_count` | Cluster centroid is the mean position | Deliberate: anchor stays a real feature; deterministic |
| Line continuity | Coalescing chains same-class segments into strokes *before* gates/thinning | `--coalesce`-family merges at tile encode time | Junctions terminate chains by default (junction-angle 0, from the Portland sweep) |
| Tile-size control (export) | Single non-iterative drop pass (`--tile-size-limit`) | Iterative threshold retry loop | Overview levels are already budgeted; the valve is a backstop, not the mechanism |
| Polygon clipping (export) | Sutherland–Hodgman f64 + i_overlay fallback | Sutherland–Hodgman integer tile coords | Same algorithm family, different coordinate space (below) |

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
`--simple-clip-fastpath`) skips the boundary-edge gate for simple features,
recovering that ~94% as wasted work avoided (~18% faster end-to-end on Natural
Earth admin z0–11, up to ~50% on the finest levels). It is gated on
per-feature simplicity (`geometry_is_simple`), so self-intersecting input still
takes the fallback and the #94 fix is preserved.

**DIVERGENCE (staging):** the fast path changes output bytes (the SH ring is
stored rotated, not reshaped), so it is **opt-in** today to keep the default
byte-identical to prior releases. It is intended to become the default once the
frozen-hash export anchors are re-baselined against the fast-path output.

## Input Contract: gpio-Optimized GeoParquet

The converter assumes (and `gpq-tiles` recommends) input prepared with
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
│   ├── check.rs        #   Spec §6.2 validation (gpq-tiles validate)
│   ├── cluster.rs      #   Point clustering + attribute accumulation (§12)
│   ├── coalesce.rs     #   Line network coalescing (§13)
│   ├── convert.rs      #   convert_to_overviews() orchestration
│   ├── export.rs       #   Overview GeoParquet → PMTiles export
│   ├── hostile.rs      #   Hostile-input hardening tests
│   ├── level.rs        #   Footer metadata model, SPEC_VERSION
│   ├── reader.rs       #   Overview file reader (level-banded row groups)
│   ├── simplify.rs     #   World-space RDP simplification (GSD tolerance)
│   ├── pipeline.rs     #   Single-read pass-2 engine (#213): fans each batch to all levels
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
├── pmtiles_writer.rs   # PMTiles v3 writer (StreamingPmtilesWriter)
├── compression.rs      # gzip/brotli/zstd compression
├── dedup.rs            # Tile deduplication (XXH3)
├── quality.rs          # CRS extraction + WGS84 validation
└── wkb.rs              # WKB round-trip helpers

crates/cli/src/main.rs  # Subcommands: tiles (facade), overview, validate,
                        # export-pmtiles
crates/python/src/lib.rs # pyo3 bindings: convert (facade), overview,
                        # export_pmtiles, validate
```
