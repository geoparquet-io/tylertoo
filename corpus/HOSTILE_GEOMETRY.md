# Hostile-geometry clipping evaluation — i_overlay vs wagyu-rs (#205)

The scorecard behind the clipping-engine decision in
`context/ARCHITECTURE.md` ("Decision Record: Clipping engine (#205)").
Snapshot of `target/hostile_geometry_eval/SCORECARD.md` from the
`full_scorecard` run on 2026-09-27 (release build, wagyu columns folded in
from `corpus/hostile_wagyu`), plus the criterion bench. Re-run instructions
at the end; the generated files are not committed.

**Decision: keep i_overlay 9 behind Sutherland–Hodgman. wagyu-rs 0.2.1 is
not adopted.** One production defect surfaced and was fixed on the way (the
exterior-only fast-path bbox, below).

## Setup

- **Harness:** `crates/core/tests/hostile_geometry_eval.rs`. Every fixture is
  clipped at three zooms — the coarsest where its bbox spans about one tile,
  then two and four finer — against up to 24 tiles per zoom (4 for the real
  fixtures), each buffered like export (8 px of 256). 2,289 (fixture × zoom
  × tile) cases.
- **Engines.** `production`: `clip::clip_geometry_simple` with export's
  defaults (per-feature simplicity hint, #239 fast path on).
  `production-strict`: `clip::clip_geometry`, fast path off
  (`--no-simple-clip-fastpath`). `sh`: raw Sutherland–Hodgman, no gate.
  `ioverlay`: raw i_overlay 9 (`ioverlay_clip`). `wagyu-i64-mvt` /
  `wagyu-i64-world`: wagyu-rs 0.2.1 `Intersection`, EvenOdd/EvenOdd, on the
  tile's 4096-unit MVT grid and on tippecanoe's `2^(32-z)` world grid, run
  out of process (see `corpus/hostile_wagyu/README.md` for why). No
  degrees-as-is wagyu column: `Wagyu<f64>` snap-rounds input to integers, so
  a sub-degree square collapses to nothing (pinned in
  `corpus/hostile_wagyu/tests/adapter_sanity.rs`, which also pins that
  integer squares clip correctly under both fill rules and both windings —
  the adapter is not the reason for the numbers below).
- **Oracles**, per case × engine: caught panic; *drops* (empty where the
  i_overlay reference has area) and *phantoms* (the reverse); *past buffer*
  (largest distance any output vertex lies outside the buffered bounds, in
  MVT units of that tile, per axis — the tile's height, not its width, for
  the latitude terms, since a Mercator tile's latitude span shrinks by
  ~cos(lat); 0.5 is where it stops quantizing onto the boundary); `geo::Validation` on the output (outputs over 20k vertices are
  *unchecked*: geo's ring self-intersection check is quadratic);
  *self-crossing* via the production sweep (`clip::geometry_is_simple`);
  *mixed winding* (exteriors disagree, or a hole matches its exterior — the
  MVT encoder re-orients rings later, so this column is informational);
  *area≠ref* (relative area against the i_overlay reference beyond 1e-6, or
  5e-3 for the integer engines; `geo::BooleanOps` — geo's own vendored
  i_overlay 4.5 — is recorded as the third opinion on valid input);
  *area>input* (a clip can never add area; checked on valid inputs only);
  vertices; wall time (single run, indicative — the bench is authoritative);
  peak heap (counting allocator; the two sweeps take a lock so they never
  run concurrently, and the only other test in the binary just parses
  fixtures, so a concurrent allocation can at most inflate one reading;
  not available for the out-of-process wagyu). *Errors* are cases an engine
  refused: for wagyu, coordinates the runner would not scale onto its
  integer grid (past ±2^40 units, where `as i64` saturates).
- **Corpora.** `tests/fixtures/geometry-test-data` (chrieke's
  geojson-invalid-geometry, 76 files: 35 usable geometries, 41 structurally
  broken files listed under "Inputs" — recorded, not skipped silently); a
  32-shape synthetic suite (bowtie, three-lobe figure eight, zero-width
  spike, holes crossing / outside / co-wound with their exterior, nested
  holes, duplicate and collinear vertices, unclosed / two-vertex / zero-area
  / empty rings, clockwise exterior, U-shape and 8-tooth comb across the
  tile edge, antimeridian rings both literal-wide and beyond ±180°, polar
  cap and a ring through the south pole, out-of-domain and 1e15 / 1e-12
  coordinates, a sub-MVT-unit sliver and a near-bowtie that only crosses
  after snapping, a 1k-vertex circle with and without an edge-crossing hole,
  overlapping and corner-touching multipolygons, degenerate / self-crossing
  / tile-edge lines); the 316k-vertex Antarctica ring and the Tielt-Winge
  admin polygon (`tests/fixtures/realdata`).

## Scorecard

69 fixtures (35 corpus, 32 synthetic, 2 real), 2289 cases, 41 corpus files
without usable geometry. 52 inputs valid per `geo::Validation`; invalid:
7× exterior self-intersection, 2× fewer than three distinct points, 3× hole
outside its exterior, 1× holes overlapping each other, 1× overlapping
multipolygon parts, 2× degenerate line; 1 unchecked (Antarctica, over the
validation cap).

### geometry-test-data

| engine | cases | panics | errors | drops | phantoms | past buffer >½ MVT | max excursion (MVT) | invalid (geo) | unchecked | self-crossing | mixed winding | area≠ref | max rel Δarea | area>input | vertices | time | peak heap |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| production | 1147 | 0 | 0 | 1 | 0 | 0 | 0.00 | 60 | 0 | 0 | 19 | 4 | 8.5e-1 | 0 | 7342 | 2.7 ms | 108.2 KiB |
| production-strict | 1147 | 0 | 0 | 1 | 0 | 0 | 0.00 | 1 | 0 | 0 | 0 | 1 | 4.3e-1 | 0 | 6976 | 12.8 ms | 208.0 KiB |
| sh | 985 | 0 | 0 | 1 | 0 | 0 | 0.00 | 84 | 0 | 6 | 19 | 13 | 8.5e-1 | 0 | 7299 | 0.4 ms | 38.2 KiB |
| ioverlay | 1147 | 0 | 0 | 0 | 0 | 0 | 0.00 | 0 | 0 | 0 | 0 | 0 | 0.0e0 | 0 | 6985 | 8.9 ms | 153.3 KiB |
| wagyu-i64-mvt | 985 | 0 | 0 | 65 | 8 | 26 | 156678.00 | 18 | 0 | 16 | 1 | 66 | 1.3e3 | 7 | 6534 | 29.6 ms | n/a |
| wagyu-i64-world | 985 | 0 | 3 | 65 | 8 | 102 | 157321.75 | 18 | 0 | 16 | 1 | 61 | 1.3e3 | 7 | 6533 | 27.1 ms | n/a |

- `production` invalid outputs: 40× exterior ring and interior ring intersect on a line; 1× exterior and second interior ring intersect on a line; 15× exterior ring has a self-intersection; 4× interior ring not contained within the exterior
- `production-strict` invalid outputs: 1× interior ring not contained within the exterior
- `sh` invalid outputs: as `production` plus 17× self-intersection and 7× fewer than three distinct points
- `wagyu-i64-mvt` / `-world` invalid outputs: 16× exterior ring has a self-intersection; 1× part with a self-intersecting exterior; 1× overlapping parts

### synthetic hostile

| production | 1121 | 0 | 0 | 0 | 44 | 0 | 0.00 | 101 | 0 | 0 | 34 | 36 | 2.8e0 | 0 | 8014 | 0.8 ms | 31.3 KiB |
| production-strict | 1121 | 0 | 0 | 0 | 3 | 0 | 0.00 | 14 | 0 | 0 | 2 | 11 | 2.8e0 | 0 | 7362 | 5.2 ms | 109.7 KiB |
| sh | 1026 | 0 | 0 | 0 | 44 | 0 | 0.00 | 144 | 0 | 7 | 34 | 55 | 2.8e0 | 0 | 8009 | 0.4 ms | 46.9 KiB |
| ioverlay | 1121 | 0 | 0 | 0 | 0 | 0 | 0.00 | 0 | 0 | 0 | 0 | 0 | 0.0e0 | 1 | 7329 | 3.9 ms | 175.7 KiB |
| wagyu-i64-mvt | 1026 | 0 | 41 | 55 | 8 | 17 | 43579.95 | 12 | 0 | 8 | 4 | 69 | 2.1e3 | 2 | 6475 | 9.3 ms | n/a |
| wagyu-i64-world | 1026 | 0 | 41 | 29 | 8 | 17 | 43580.16 | 12 | 0 | 8 | 4 | 68 | 2.1e3 | 2 | 7241 | 10.6 ms | n/a |

- `production` invalid outputs: 65× exterior ring and interior ring at index 0 intersect on a line; 18× exterior ring has a self-intersection; 1× interior ring at index 0 and interior ring at index 1 intersect on an area; 7× interior ring at index 0 is not contained within the polygon's exterior; 10× polygons at indices 0 and 1 overlap

- `production` invalid outputs: 65× exterior and interior ring intersect on a line; 18× exterior ring has a self-intersection; 1× holes overlap; 7× interior ring not contained within the exterior; 10× overlapping parts (the input's own)
- `production-strict` invalid outputs: 2× self-intersection; 1× holes overlap; 1× interior not contained; 10× overlapping parts
- `wagyu-i64-mvt` / `-world` invalid outputs: 7× self-intersecting exterior (2 top-level, 5 parts) plus 1 more part; 1× overlapping parts; 2× parts touching on a line; 1× parts 0 and 7 overlap
- `wagyu-i64-mvt` / `-world` errors: 41× the runner refused the case — the 1e15° `huge-coordinates` ring scales past ±2^40 grid units, where `as i64` would saturate and wagyu would be scored on runner-made input (the world grid also refuses 3 corpus cases of `problematic_crs_defined`, whose projected-metre coordinates land near 1e14 units at z1–z5)

### real

| sh | 21 | 0 | 0 | 0 | 0 | 0 | 0.00 | 1 | 5 | 0 | 0 | 0 | 5.9e-8 | 0 | 506799 | 13.4 ms | 14.5 MiB |
| ioverlay | 21 | 0 | 0 | 0 | 0 | 0 | 0.00 | 0 | 5 | 0 | 0 | 0 | 0.0e0 | 0 | 506372 | 460.3 ms | 53.8 MiB |
| wagyu-i64-mvt | 21 | 0 | 0 | 0 | 0 | 0 | 0.32 | 5 | 0 | 3 | 4 | 2 | 1.3e-1 | 0 | 36316 | 9992.1 ms | n/a |
| wagyu-i64-world | 21 | 9 | 0 | 0 | 0 | 0 | 0.01 | 0 | 0 | 0 | 0 | 0 | 1.1e-5 | 0 | 767 | 19909.5 ms | n/a |

- `production` invalid outputs: 1× polygon at index 0 is invalid: exterior ring has a self-intersection
- `sh` invalid outputs: 1× polygon at index 0 is invalid: exterior ring has a self-intersection
- `wagyu-i64-mvt` invalid outputs: 2× polygon at index 0 is invalid: exterior ring has a self-intersection; 1× polygon at index 1 is invalid: exterior ring has a self-intersection; 1× polygons at indices 0 and 10 overlap; 1× polygons at indices 2 and 16 overlap

- `production` / `sh` invalid output: 1× a part with a self-intersecting exterior — Tielt-Winge at z8, an S-H self-touching ring the sweep does not count as a proper crossing (self-crossing column: 0)
- `wagyu-i64-mvt` invalid outputs: 3× self-intersecting exteriors, 2× overlapping parts; `wagyu-i64-world` panics: 9× `INFINITE LOOP DETECTED in vatti main loop at iteration 100001` on Antarctica

## Reading the numbers

**The incumbent.** Zero panics on 2,289 cases for every in-tree engine.
Nothing the production path emits reaches past the buffered bounds by more
than half an MVT unit — after the fix below; before it, two synthetic cases
reached 1,263 and 4,904 units past. No production output has a proper
self-crossing, and no clip of a valid input gained area. i_overlay on its
own is clean on every column: OGC-valid output on all 2,289 cases, so the
"renders correctly but not valid" gap #205 attributed to it did not
materialize on this corpus.

Production's 162 `geo::Validation` failures are all Sutherland–Hodgman
results on *invalid* input kept by the #239 fast path (production-strict,
which routes those to i_overlay, has 15): a hole clipped along the same tile
edge as its exterior ("intersect on a line", 105 of them), the input's own
self-touching ring or overlapping parts passed through, a hole crossing its
exterior clipped ring by ring. The encoder's even-odd/nonzero fill renders
each the way the unclipped invalid polygon would render, which is the #239
contract. The one production *drop* is a z21 tile inside the lobe of a hole
that crosses its exterior: S-H, like tippecanoe's positive fill, treats a
hole as subtractive and emits nothing; i_overlay's even-odd fills it. The 44
production *phantoms* are the reference failing, not production: i_overlay
returns empty for coordinates around 1e15 (outside the lon/lat domain, which
#429 rejects at convert), where S-H clips them normally.

Area disagreements between production and the reference are all on invalid
input (a hole crossing its exterior read as subtractive vs even-odd;
overlapping multipolygon parts kept vs unioned), except the sub-MVT-unit
sliver, where i_overlay's float-to-integer grid — set by the joint bbox of
subject and clip — puts ~0.1 % noise on a shape 1e-6 of a tile wide
(sub-pixel), and the 1e-12° square, which the absolute 1e-10° duplicate-
vertex epsilon in `clip::has_structural_issues` routes to the overlay
(4.7 % of 1e-24 square degrees; 0.005 MVT units at z22).

**The challenger.** wagyu-rs 0.2.1 emits vertices up to 157,321 grid units
past the clip box — beyond the *subject's* own extent — on 43 (MVT grid) and
119 (world grid) cases; inflates a valid polygon's area 13× on
`invalid_interior_not_cw` (a hole wound like its exterior, which is valid
under even-odd); produces self-crossing rings on 27 / 24 cases and
overlapping parts, against a contract of OGC-valid output; drops 120 / 94
cases that every other engine keeps; trips its own infinite-loop detector on
the real 316k-vertex Antarctica ring in tippecanoe's own clip space; and
costs ~10 s (MVT grid) to ~20 s (world grid) per Antarctica clip against
63 ms for i_overlay and 1.1 ms for S-H (bench below). Both correctness
findings and the dead-dependency blocker are filed upstream
(nlebovits/wagyu-rs#113, #114) with the reproducer. The hybrid #205 floated
(i_overlay pre-clip, wagyu post-quantization cleanup) is moot: the
post-quantization step is exactly the MVT-grid column, which is the one that
emits out-of-bounds vertices and self-crossings.

**#205's quantization argument.** Clipping on the integer grid would close
the "snap creates a bowtie" class by construction — but only with an
integer-space engine that is itself sound. The #383 repair already runs on
the integer grid after snapping (exact checks first, overlay only on
failure) and the `near-bowtie-snap-crosses` and `sliver-sub-mvt-unit`
synthetic cases pass every production invariant, so that path stays.

## Production defect found and fixed

`clip_polygon` and `clip_multipolygon` gated their fully-inside fast path on
`geo`'s `Polygon::bounding_rect`, which covers the exterior ring only. A
polygon whose exterior sat inside the tile came back verbatim with every
interior ring it carried — including a hole wholly outside the exterior and
the tile (synthetic `hole-outside-exterior`, 4,904 MVT units past the
buffer) or crossing it (`hole-crosses-exterior`, 1,263). Invalid input, but
the convert pass carries it verbatim (#188) and the tile must stay inside
its window. Fixed by gating on a bbox over every ring
(`clip::polygon_rect_all_rings`), which is the exterior's rect for any valid
polygon, so valid output is byte-identical (regression tests in `clip.rs`;
the golden guard and the line-clip pins are unchanged).

## Bench (`cargo bench -p tylertoo-core --bench hostile_geometry`)

Criterion, release, one cutting tile per shape two zooms below the one that
holds it, buffered like export. `production` is the export path
(`clip_geometry_simple` with the per-feature simplicity hint); `sh` and
`ioverlay` are the raw engines on the same buffered window. wagyu-rs cannot
be linked into the workspace, so its per-case wall times are the ones the
runner records (scorecard "time" column): ~10 s per Antarctica clip on the
MVT grid, ~20 s on the world grid.

| tier | shape | production | sh | ioverlay |
|---|---|--:|--:|--:|
| clean | square (5 verts) | 134 ns | 122 ns | 779 ns |
| clean | circle-1k | 4.7 µs | 3.6 µs | 39 µs |
| clean | circle-100k | 450 µs | 340 µs | 6.3 ms |
| invalid | bowtie | 2.3 µs | 161 ns | 2.2 µs |
| invalid | star-1k-crossing | 216 µs | 3.6 µs | 209 µs |
| invalid | corpus self-intersection-large | 2.8 µs | 148 ns | 2.6 µs |
| pathological | comb-64 | 1.8 µs | 1.4 µs | 28 µs |
| pathological | comb-1024 | 19 µs | 13 µs | 568 µs |
| pathological | antarctica-316k | 1.54 ms | 1.06 ms | 63 ms |

`production` on invalid input is the cost of the gate plus i_overlay, as
designed (the star pays the #241 sweep, 216 µs, once per feature in export,
not per tile). The 10–60× gap between S-H and i_overlay on clean and
pathological shapes is why S-H stays first and i_overlay stays the fallback.

## Re-run

```bash
# quick tier (corpus + synthetic; asserts the production invariants)
cargo test -p tylertoo-core \
  --test hostile_geometry_eval smoke_scorecard

# full scorecard (slow set; writes target/hostile_geometry_eval/SCORECARD.md)
cargo test --release -p tylertoo-core \
  --test hostile_geometry_eval full_scorecard

# wagyu columns (reads cases-full.jsonl), then re-run full_scorecard
# above to fold them in
cargo run --release \
  --manifest-path corpus/hostile_wagyu/Cargo.toml

# timings
cargo bench -p tylertoo-core --bench hostile_geometry
```
