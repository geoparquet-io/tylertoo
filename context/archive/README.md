# context/archive — historical record

Frozen point-in-time documents. Nothing here describes the current
system; each file is kept as the paper trail for a decision or a
development phase. Internal cross-references between archived files
use their original (pre-archive) paths.

Current-state documentation lives in `context/ARCHITECTURE.md`,
`context/OVERVIEWS_SPEC.md`, `context/OVERVIEW_TUNING.md`, and the
top-level README / DEVELOPMENT / CONTRIBUTING docs.

## Index

| File | What it was |
|------|-------------|
| `OVERVIEWS_PLAN.md` | Agent-driven execution plan for the overview format build-out (2026-07-02). All phases through V4/H3 shipped; remaining launch items are tracked as GitHub issues (#175, #179). |
| `CARRYOVER.md` | Module-by-module audit (task G2) mapping the legacy tile pipeline's code to reuse/adapt/shelve fates for the overview pipeline. The modules it audits were largely deleted in #189. |
| `CI_TRIAGE.md` | Offline reproduction and triage of the 4 failing CI jobs on `feat/geoparquet-overviews` (task H2, 2026-07-03). Superseded by the #182 CI overhaul (PR #191). |
| `LEGACY_TILES_ARCHITECTURE.md` | Architecture notes for the removed per-tile pipeline (density dropping, adaptive thresholds, tiny-polygon accumulation, tile-time clustering/accumulation, external-sort streaming). Extracted from `context/ARCHITECTURE.md` after the #177 removal decision. |
| `plans/2026-02-23-streaming-design.md` | Design doc for the legacy pipeline's external-sort streaming modes (see also `context/adr/001`). |
| `plans/adaptive-threshold-iteration.md` | Plan for the legacy pipeline's tippecanoe-style adaptive tile-size thresholds. |
| `plans/2026-03-10-integer-coordinate-system.md` | Migration plan to i32 world coordinates (the surviving `world_coord.rs` came from this). |
| `plans/2026-03-13-drop-smallest-as-needed.md` | Implementation plan for the legacy `--drop-smallest-as-needed` flag (removed in #189). |
| `plans/2026-04-08-geometry-coalescing-design.md` | Design for legacy tile-time geometry coalescing (`coalesce.rs`, removed in #189; distinct from the current `overview/coalesce.rs` line coalescing). |
| `plans/2026-04-10-zoom-dependent-simplification.md` | Plan for per-tile zoom-dependent simplification (PR #158) — excised; see `context/TILE_SIMPLIFY_POSTMORTEM.md`. |
| `bench/H3_NOTES.md` | H3(a)/(b) engineering notes: two-pass streaming convert (−94% peak RSS) and the bounded-memory export restructure. Summarized in `benchmarks/overview/PROFILE.md`. |
| `bench/H3C_PROFILE.md` | H3(c) wall-time profile of convert/export on Moldova, the ranked optimization levers, and the post-lever results (convert 12.8× faster). Summarized in `benchmarks/overview/PROFILE.md`. |
| `bench/EXPORT_NOTES.md` | E0 `export-pmtiles` design + validation notes (border-duplication accounting, the single-pass tile-size safety valve). Summarized in `benchmarks/overview/PROFILE.md`. |
| `corpus/V1_RESULTS.md` | V1 correctness-suite snapshot (2026-07-02, pre-Q2/H1/H3 numbers). Regenerate a current report with `corpus/verify.sh` (writes a fresh `corpus/V1_RESULTS.md`). |
| `corpus/SWEEP_NOTES.md` | Portland roads lt×sf sweep + Q2 drop-rate calibration notes. Decisions consolidated into `corpus/SWEEPS.md`. |
