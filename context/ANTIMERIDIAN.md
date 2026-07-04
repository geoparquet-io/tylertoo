# Antimeridian handling — decision memo (issue #188)

**Status:** decided (research memo; pins in tree)
**Date:** 2026-07-04
**Follow-up to:** #173 / PR #186 (H4 hostile-input hardening)

The convert pipeline stores geometries verbatim by design (spec: no
reprojection or clipping at convert time). PR #186 pinned that
antimeridian/polar inputs never crash. This memo answers the open
questions from #188 empirically: what verbatim storage actually does
downstream, whether splitting is warranted, and where it would belong.
Every finding below is pinned by a test in the tree.

## TL;DR

Verbatim storage of an antimeridian-crossing geometry **does** degrade
every downstream stage — level assignment, bbox pruning, and export —
but by exactly the mechanism the spec already anticipates: §7.2 makes
pre-splitting a **producer (input data) responsibility** and the
validator SHOULD-warns on world-spanning bboxes. The failures are
quality/size degradations, not crashes or corruption. **Recommendation:
do not add splitting to the pipeline.** Keep the verbatim contract,
rely on spec §7.2, and upgrade the user-facing surface (convert-time
warning) rather than mutating geometry. Export-time splitting is the
only defensible location if field evidence ever demands it.

## Findings

Test fixture throughout: a polygon whose author intends a 0.2° × 0.2°
box straddling ±180° (vertices at lng −179.9 and +179.9), stored
verbatim in EPSG:4326.

### F1. Bboxes are inflated, never wrapped

`geometry_bbox` (convert/stream) and `scan_level` (export) both use
`geo::bounding_rect` — plain min/max. The fixture's bbox is
`[-179.9, -0.1, 179.9, 0.1]`: 359.8° wide, `lng_min < lng_max` always.

- A **wrapped** bbox (`lng_min > lng_max`) never arises anywhere in our
  code paths. `tiles_for_bbox` has a correct wrapped-bbox branch
  (tile.rs:191), but it is unreachable from the pipeline's own bboxes —
  it can only trigger on caller-supplied bounds.
- Pinned by: `overview::hostile::antimeridian_bbox_is_inflated_never_wrapped`,
  `tile::tests::antimeridian_inflated_bbox_covers_full_world_row`.

### F2. Level assignment: real degradation, two mechanisms

Assignment consumes only the bbox (center + diagonal), so the inflated
bbox distorts both:

1. **GSD/visibility inflation.** The 0.2°-wide feature has a ~359.8°
   bbox diagonal (~4.0e7 m), so it clears every visibility gate and is
   promoted to the **coarsest** level (`min_level = 0`). At its true
   extent it is gated to the **finest** level. Worst-case
   miscategorization in both directions of the spec's intent: a
   sliver-sized feature is rendered at global zooms, and its huge
   `diag²` priority lets it out-compete genuinely large features.
2. **Wrong-hemisphere representative point.** The bbox center is
   lng ≈ 0 — the prime meridian, ~180° from the feature. It competes in
   (and, with its inflated priority, **wins**) grid cells there,
   displacing genuine prime-meridian features to finer levels.

- Pinned by: `overview::assign::tests::antimeridian_inflated_bbox_assigned_to_coarsest_level`,
  `overview::assign::tests::antimeridian_center_lands_on_prime_meridian_and_displaces_neighbor`.

### F3. Export: full world-row smearing, not just seam artifacts

The stored rectangle passes through lng 0 in coordinate space, so it
genuinely intersects **every** tile column. End-to-end (convert at
z2–z6, then `export_pmtiles`), the single 0.2°-wide polygon produces:

| zoom | tiles written | wrap-aware expectation |
|------|--------------:|-----------------------:|
| z2   | 8             | ~2                      |
| z3   | 16            | ~2–4                    |
| z4   | 32            | ~2–4                    |
| z5   | 64            | ~2–4                    |
| z6   | 128           | ~2–4                    |

That is `2^z × 2` tiles per zoom (full world row × two equator rows) —
248 tiles total where a wrap-aware exporter would write ~15. Each tile
carries a horizontal band polygon: the rendered output shows a
world-wide smear at the feature's latitude. Tiles adjacent to ±180° do
receive content (the geometry is present where the author intended it),
but so does everything in between.

- Pinned by: `overview::hostile::antimeridian_polygon_export_smears_world_row`,
  `clip::tests::antimeridian_polygon_smears_into_prime_meridian_tile`,
  `clip::tests::antimeridian_polygon_clips_at_edge_tile`.

### F4. Reader-side bbox pruning is defeated (by inspection)

The `bbox` covering column stores the same inflated bbox, so every
viewport query at the feature's latitude fetches its row group — the
exact failure mode spec §7.2 names ("a feature whose bbox spans ~360°
of longitude defeats the covering index"). Consistent with F1–F3; no
new pin needed beyond the bbox pin (F1).

## Decision

### (a) Is splitting warranted in gpq-tiles? **No.**

- The degradations are real (F2/F3) but they are **input-data defects
  under the spec's own contract**: §7.2 already says geometries MUST NOT
  cross the antimeridian in a way that defeats bbox pruning and that
  *producers SHOULD split before writing*. GeoParquet upstream takes the
  same stance (RFC 7946 §3.1.9-style splitting is the data author's
  job). Silently mutating geometry inside a converter whose core
  contract is "verbatim" would be a bigger correctness risk than the
  defect it fixes (splitting polygons correctly at ±180° — holes,
  multi-part, winding — is a well-known bug farm; cf. the dedicated
  `antimeridian` libraries).
- No field evidence of real-world breakage has been presented; the
  ticket's bar for reopening the verbatim contract ("evidence of a real
  downstream failure") is met only by synthetic fixtures that are
  already spec-non-conformant inputs.
- The cheap, contract-preserving improvement instead: **warn at convert
  time** when a feature bbox spans > 180° of longitude (the H4 warning
  channel already exists for skipped rows), mirroring the validator's
  SHOULD-warn. Tracked as follow-up; not part of this memo's pins.

### (b) If splitting ever becomes necessary, export time only. **Yes.**

Export already transforms geometry (tile clipping), already owns
`tiles_for_bbox` with a working wrapped-bbox branch, and produces
derived artifacts (PMTiles) rather than the stored format — so a future
opt-in `ExportOptions` flag ("treat >180°-wide features as wrapped:
split into two lobes before tile assignment") would leave the
convert-time verbatim contract fully intact. Convert-time splitting is
rejected outright: it mutates stored geometry, changes feature counts,
and breaks round-trip fidelity with the source GeoParquet.

### (c) Should the spec say more? **No — it already says the right thing.**

`context/OVERVIEWS_SPEC.md` §7.2 (normative: inputs MUST NOT cross in a
pruning-defeating way; producers SHOULD pre-split) plus the §6-family
validator SHOULD-warn is exactly the "reader's/producer's problem"
stance consistent with GeoParquet upstream. Adding converter-side
normative text would contradict the verbatim contract. No spec change.

## Test inventory (behavior pins, this memo)

All pins document **current** behavior; if any starts failing, either a
splitting feature landed (update this memo) or a regression changed
bbox/assignment/clip semantics.

- `overview::assign::tests::antimeridian_inflated_bbox_assigned_to_coarsest_level`
- `overview::assign::tests::antimeridian_center_lands_on_prime_meridian_and_displaces_neighbor`
- `clip::tests::antimeridian_polygon_smears_into_prime_meridian_tile`
- `clip::tests::antimeridian_polygon_clips_at_edge_tile`
- `tile::tests::antimeridian_inflated_bbox_covers_full_world_row`
- `overview::hostile::antimeridian_bbox_is_inflated_never_wrapped`
- `overview::hostile::antimeridian_polygon_export_smears_world_row`

Pre-existing (PR #186): `overview::hostile::antimeridian_and_pole_features_convert`
(no-crash pin); `tile::tests::test_tiles_for_bbox_antimeridian_crossing`
(wrapped-bbox branch of `tiles_for_bbox`, caller-supplied bounds only).
