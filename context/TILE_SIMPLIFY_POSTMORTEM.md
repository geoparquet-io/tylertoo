# Postmortem — per-tile zoom-dependent simplification (PR #158)

> **Status: historical.** This is a point-in-time decision record. It describes
> the per-tile zoom-dependent simplification of #158 (excised in commit
> `d897110`) and the per-tile pipeline that hosted it (removed in #177) — it does
> **not** describe current code. In particular there is no top-level
> `crates/core/src/simplify.rs` anymore; the live simplifier is
> `crates/core/src/overview/simplify.rs` (world-space RDP, see
> [ARCHITECTURE.md](ARCHITECTURE.md)). Kept only for the reasoning it captured.

Date: 2026-07-03
Decision: **EXCISED** (removed, not repaired) by maintainer during Phase 5 / H2.
Removal commit: `d897110` (`refactor(tiles): remove unproven zoom-dependent
simplification (#158)`) on `feat/geoparquet-overviews`.

## What the feature was

Zoom-dependent geometry simplification performed **inside the per-tile
generation loop**. As a tile was produced at zoom `z`, its clipped geometry was
run through Douglas-Peucker at a pixel tolerance scaled to `z`, with the goal of
shrinking tile sizes for linear/polygon features at coarse zooms while keeping
fidelity at the max zoom.

Surface it exposed:
- `tiles` CLI: `--simplify` (bool) + `--simplify-factor` (f64).
- `TilerConfig::simplify_factor: Option<f64>` + `with_simplify(factor)`.
- Python: `convert(..., simplify_factor=...)`.
- core `simplify.rs`: `simplify_geometry_for_tile` (unified entry, re-exported
  from `lib.rs`), boundary-preserving `simplify_world_linestring` /
  `simplify_world_ring` / `*_preserve_boundaries`, `is_on_tile_boundary`, and
  the later WIP `simplify_coalesced_linestring` + `remove_noop_multilinestring`.
- pipeline integration in `pipeline.rs` (clip → simplify → MVT), gated on
  `simplify_factor`.

## How far it got — state of each piece

| Piece | State |
|-------|-------|
| Tile-boundary detection (`is_on_tile_boundary`) | worked as library code |
| Boundary-preserving linestring simplification | worked as library code (unit-tested) |
| Boundary-preserving polygon-ring simplification | worked as library code (unit-tested) |
| Unified `simplify_geometry_for_tile` entry | worked at the unit level |
| Pipeline integration (per-tile clip→simplify→encode) | wired, but **never validated end-to-end** — no proof it improved real tilesets without visual regressions |
| Coalesced-linestring simplify + noop removal (`61e9c17`) | **WIP**, committed from a dirty working tree; incomplete |
| Non-coalesced path sub-pixel dropping (`min_extent_px`) | **never implemented** — the known gap; the coalesced path dropped sub-threshold linestrings, the non-coalesced path did not. A deliberately-failing "red" test (`test_non_coalesced_tiny_linestrings_should_be_dropped`) documented this and was the direct cause of the Test-job CI failure |

The fundamental problem: **all of `simplify.rs`'s per-tile functions route
through a tile-local pixel-space transform** (see
`context/archive/CARRYOVER.md` §"simplify.rs — ADAPT"), so simplification quality was
coupled to the tile grid and the whole per-tile approach was hard to validate or
tune. `pipeline.rs` is described in CARRYOVER as "working but stuck … the stuck
subsystem."

## Why the pivot (why it was excised, not fixed)

The overview architecture moved generalization **out of the tile loop
entirely**:

- `crates/core/src/overview/simplify.rs` does **world-space GSD simplification**
  — RDP run directly on input coordinates at a ground-sample-distance tolerance,
  independent of any tile grid. This is proven by the V1/V2 correctness suites
  and the V3 benchmarks.
- `export-pmtiles` (E0) emits already-simplified tiles **from the overview
  file**, so the tile step no longer needs to simplify at all.

Together these make the per-tile simplification path **redundant**: the
generalization it was trying to do is now done earlier, in a coordinate space
that actually works, and validated. Repairing the per-tile path (finishing the
coalesce WIP, implementing the non-coalesced `min_extent_px` gap, then
validating end-to-end) would have been substantial work to reach a capability
the overview path already delivers. The maintainer chose excision.

## Where the code lives if ever needed

- Feature commit range on this branch: **`c91c9a1..61e9c17`**
  (`c91c9a1` tile boundary detection → … → `788acdc` TilerConfig field →
  `f2d5e73` unified helper → `7b250ec` pipeline integration →
  `2418549` Python binding → `c65e39a` CLI flags → `61e9c17` coalesce/noop WIP).
- These commits remain in history (the excision is a forward `git checkout
  origin/main -- <files>` revert in `d897110`, **no history rewrite**), so the
  full implementation is recoverable via `git show <sha>` / `git checkout
  <sha> -- <path>`.
- **PR #158** (the tile-simplification PR) is closed, superseded by **PR #168**
  (the overview branch, retargeted to `main`).

## What was removed vs kept

Removed: the entire surface above — CLI flags, `TilerConfig` field +
`with_simplify`, Python kwarg, the `simplify_geometry_for_tile` re-export, the
feature-added `simplify.rs` functions (incl. the WIP + boundary-preserving
helpers), and the feature tests (`simplification_integration.rs`, the WIP unit
tests, and the non-coalesced known-gap red test).

Kept in `crates/core/src/simplify.rs` (main's 6 functions — still used by the
tile pipeline's **default** path, which was on main all along and is unchanged):
`simplify_for_zoom`, `simplify_in_tile_coords`, `simplify_world_linestring`,
`simplify_world_ring`, `world_simplified_vertex_count`, `simplify_to_tile_coords`.

The overview path (`overview/simplify.rs`) and E0 `export`/`mvt.rs` are
self-contained and were not touched; the `tiles` subcommand now behaves exactly
as `main` does today.
