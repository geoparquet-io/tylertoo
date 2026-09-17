//! Attribute-driven entry zoom — the magnitude ladder (#364).
//!
//! Every thinning mechanism in [`super::assign`] ranks on *geometry*: the
//! visibility gate drops a feature whose bbox diagonal is below a multiple of
//! the level GSD, and cell-winner thinning keeps one feature per grid cell.
//! For nested-band data that is backwards. Concentric change contours carry
//! their strongest signal in the **physically smallest** ring, so a coarse
//! level keeps the big weak outer rings and drops the small strong cores —
//! the opposite of what the map should show zoomed out.
//!
//! `--sort-key` cannot close this: it chooses between features *competing for
//! a cell*, and the gate has already dropped the small ones on size. Measured
//! on 3.1M contour polygons (#364), ranking lifts high-magnitude survivors
//! 6.6x at z8 but only reaches 3 of 181 at z6, because a 0.5 contour is metres
//! across and never survives to compete.
//!
//! A ladder answers a different question — not "which of these wins a cell"
//! but "how early may this feature appear at all". Each distinct value of a
//! column gets an **entry zoom**; a feature appears at every level from its
//! entry zoom inward and at none before it, exempt from the gate and from
//! thinning throughout. Nothing is deleted: the canonical level still carries
//! every feature.
//!
//! # Divergence from tippecanoe
//!
//! None in mechanism — this is tippecanoe's per-feature `tippecanoe.minzoom`,
//! which is the one attribute-driven thinning lever it offers. tippecanoe
//! takes the minzoom as an input attribute; we additionally *derive* one from
//! a column ([`EntryZoomLadder::dense_rank`]), which is what callers were
//! doing by hand upstream of it.

use std::collections::BTreeMap;

/// Why a ladder specification could not be built.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LadderError {
    /// `--ladder-step 0` would put every rank at the same zoom, which is a
    /// ladder with one rung — almost certainly not what was meant.
    #[error("ladder step must be >= 1 (0 would give every value the same entry zoom)")]
    ZeroStep,
    /// An explicit entry-zoom spec that named no values.
    #[error("entry-zoom spec named no values: expected COLUMN:VALUE=ZOOM[,VALUE=ZOOM...]")]
    Empty,
    /// A non-finite value cannot be matched against column data.
    #[error("entry-zoom value {0} is not finite")]
    NonFiniteValue(f64),
}

/// Maps a column value to the zoom at which features carrying it may first
/// appear.
///
/// Lookup is by **exact** value, because the ladder's rungs are the column's
/// own distinct values. Stored sorted so a lookup is a binary search rather
/// than a hash of an `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryZoomLadder {
    /// `(value, entry_zoom)`, ascending by value.
    rungs: Vec<(f64, u8)>,
}

impl EntryZoomLadder {
    /// Derive a ladder from the distinct values of a column: rank them
    /// **descending** (largest value = strongest = earliest), then place rank
    /// `r` at `base_zoom + r * step`, clamped to `max_zoom`.
    ///
    /// Ranking over *distinct values* rather than the values themselves — SQL
    /// `DENSE_RANK`, as the reference pipeline does — is what makes the ladder
    /// scale-free. Mapping a raw magnitude linearly onto the zoom range
    /// strands everything in the upper zooms whenever the values occupy a
    /// narrow part of their nominal scale, which real data usually does: the
    /// motivating set is 0.2-0.5 of a nominal 0-1.
    ///
    /// Non-finite values are ignored; features carrying them get no entry zoom
    /// and fall through to the ordinary gate.
    pub fn dense_rank(
        values: impl IntoIterator<Item = f64>,
        base_zoom: u8,
        step: u8,
        max_zoom: u8,
    ) -> Result<Self, LadderError> {
        if step == 0 {
            return Err(LadderError::ZeroStep);
        }
        // BTreeMap over bits would order NaN oddly; filter first, then sort
        // with total_cmp, which is a total order over the finite values left.
        let mut distinct: Vec<f64> = values.into_iter().filter(|v| v.is_finite()).collect();
        distinct.sort_by(|a, b| b.total_cmp(a)); // descending: strongest first
        distinct.dedup_by(|a, b| a.total_cmp(b).is_eq());

        let total = distinct.len();
        let rungs: Vec<(f64, u8)> = distinct
            .into_iter()
            .enumerate()
            .map_while(|(rank, value)| {
                let offset = (rank as u32).saturating_mul(step as u32);
                let zoom = (base_zoom as u32).saturating_add(offset);
                // Past the finest zoom the ladder has nothing to say. Clamping
                // onto `max_zoom` instead would give every surplus value an
                // entry level of "finest only" — and because a laddered
                // feature is authoritative (it is removed from every winner
                // grid and pinned to its entry level), that silently empties
                // the coarse pyramid for any high-cardinality column. No rung
                // means no ladder opinion, which leaves those features to the
                // ordinary gate and thinning.
                (zoom <= max_zoom as u32).then_some((value, zoom as u8))
            })
            .collect();
        if rungs.len() < total {
            log::warn!(
                "[assign] entry-zoom ladder: {} of {total} distinct value(s) rank \
                 past zoom {max_zoom} at step {step} and get no entry zoom — they \
                 keep the ordinary visibility gate and thinning. Widen the zoom \
                 range, lower --ladder-step, or name the rungs with --entry-zoom.",
                total - rungs.len()
            );
        }
        Ok(Self::sorted(rungs))
    }

    /// Build from an explicit `value -> zoom` map, for callers who want the
    /// rungs placed by hand rather than evenly.
    pub fn explicit(pairs: impl IntoIterator<Item = (f64, u8)>) -> Result<Self, LadderError> {
        let rungs: Vec<(f64, u8)> = pairs.into_iter().collect();
        if rungs.is_empty() {
            return Err(LadderError::Empty);
        }
        if let Some((v, _)) = rungs.iter().find(|(v, _)| !v.is_finite()) {
            return Err(LadderError::NonFiniteValue(*v));
        }
        Ok(Self::sorted(rungs))
    }

    fn sorted(mut rungs: Vec<(f64, u8)>) -> Self {
        rungs.sort_by(|a, b| a.0.total_cmp(&b.0));
        rungs.dedup_by(|a, b| a.0.total_cmp(&b.0).is_eq());
        Self { rungs }
    }

    /// The entry zoom for a column value, or `None` when the value is absent,
    /// non-finite, or not a rung of this ladder.
    ///
    /// `None` means "this feature has no ladder opinion" — the caller leaves
    /// it to the ordinary visibility gate and thinning, so a partially
    /// populated column degrades to today's behaviour instead of hiding rows.
    pub fn entry_zoom(&self, value: Option<f64>) -> Option<u8> {
        let v = value?;
        if !v.is_finite() {
            return None;
        }
        self.rungs
            .binary_search_by(|(rv, _)| rv.total_cmp(&v))
            .ok()
            .map(|i| self.rungs[i].1)
    }

    /// The rungs as `value -> zoom`, for provenance and logging.
    ///
    /// Keys are the rendered values, so iteration order is lexicographic
    /// rather than numeric (`"1000000"` sorts before `"5000"`). That matches
    /// how `ranking.ranks` is already serialized (§3.5); the zoom carried by
    /// each key is what a reader needs, not the key order.
    pub fn to_map(&self) -> BTreeMap<String, u8> {
        self.rungs
            .iter()
            .map(|(v, z)| (format_value(*v), *z))
            .collect()
    }

    /// Number of rungs.
    pub fn len(&self) -> usize {
        self.rungs.len()
    }

    /// Whether the ladder has no rungs.
    pub fn is_empty(&self) -> bool {
        self.rungs.is_empty()
    }
}

/// Render a rung value for provenance/logs without trailing float noise.
fn format_value(v: f64) -> String {
    v.to_string()
}

/// How a caller asked for the ladder to be built.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryZoomKind {
    /// Derive it: rank the column's distinct values descending, one rung per
    /// `step` zooms, starting at the coarsest level's zoom.
    DenseRank {
        /// Zooms between consecutive rungs.
        step: u8,
    },
    /// Use these `(value, zoom)` rungs verbatim.
    Explicit(Vec<(f64, u8)>),
}

/// A `--magnitude-ladder` / `--entry-zoom` request: which column, and how.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryZoomSpec {
    /// Column whose values the rungs are drawn from.
    pub column: String,
    /// How the rungs are placed.
    pub kind: EntryZoomKind,
}

/// Build the ladder a spec asks for, given the column's values and the level
/// plan's zooms (coarse → fine).
///
/// A derived ladder spans the archive: rung 0 sits at the coarsest level's
/// zoom and nothing is placed past the finest.
pub fn build_ladder(
    spec: &EntryZoomSpec,
    values: &[Option<f64>],
    level_zooms: &[Option<u8>],
) -> Result<EntryZoomLadder, LadderError> {
    match &spec.kind {
        EntryZoomKind::Explicit(pairs) => {
            EntryZoomLadder::explicit(snap_to_observed(pairs, values, &spec.column))
        }
        EntryZoomKind::DenseRank { step } => {
            let base = level_zooms.iter().flatten().copied().min().unwrap_or(0);
            let max = level_zooms
                .iter()
                .flatten()
                .copied()
                .max()
                .unwrap_or(u8::MAX);
            EntryZoomLadder::dense_rank(values.iter().flatten().copied(), base, *step, max)
        }
    }
}

/// Relative tolerance for matching a hand-written rung against column data.
///
/// A value stored as `f32` widens to an `f64` that is not bit-equal to the
/// decimal a caller types: `0.425_f32` becomes `0.42500001192092896`. The
/// widening error is bounded by `2^-24` (~6e-8) relative, so a 1e-6 window
/// admits it with three orders of magnitude to spare while staying far tighter
/// than any spacing a human writes between rungs.
const RUNG_MATCH_REL_TOLERANCE: f64 = 1e-6;

/// Replace each hand-written rung value with the column value it names.
///
/// Rungs are matched against the data rather than compared bit-exactly, then
/// stored as the observed value, so the later lookup can stay an exact
/// binary search. A rung naming no row is dropped with a warning — silently
/// keeping it would leave a spec that looks applied and is not.
fn snap_to_observed(pairs: &[(f64, u8)], values: &[Option<f64>], column: &str) -> Vec<(f64, u8)> {
    let mut observed: Vec<f64> = values
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    observed.sort_by(f64::total_cmp);
    observed.dedup_by(|a, b| a.total_cmp(b).is_eq());
    if observed.is_empty() {
        // Nothing to snap against (empty input, or an all-null column). The
        // ladder is inert either way; keep the spec as written.
        return pairs.to_vec();
    }

    let mut out = Vec::with_capacity(pairs.len());
    for &(want, zoom) in pairs {
        let nearest = observed
            .iter()
            .copied()
            .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()));
        match nearest {
            Some(v) if (v - want).abs() <= RUNG_MATCH_REL_TOLERANCE * want.abs().max(1.0) => {
                out.push((v, zoom));
            }
            _ => log::warn!(
                "[assign] entry-zoom rung {want} matches no value in column \
                 {column:?}, so nothing enters at zoom {zoom}; check the spec \
                 against the column's actual values"
            ),
        }
    }
    out
}

/// Map each feature's column value to the **level index** it may first appear
/// at, parallel to `values`.
///
/// An entry *zoom* becomes an entry *level* by taking the coarsest level whose
/// zoom is at least that zoom — the level the feature is first allowed into.
/// A zoom below every level clamps to the coarsest level, above every level to
/// the finest.
///
/// Returns all-`None` (the ladder is inert) when the plan carries no zooms —
/// a GSD-only plan has nothing to anchor an entry zoom to, and silently
/// guessing would move features for reasons the caller could not see.
pub fn entry_levels(
    ladder: &EntryZoomLadder,
    values: &[Option<f64>],
    level_zooms: &[Option<u8>],
) -> Vec<Option<u8>> {
    if ladder.is_empty() || level_zooms.iter().all(|z| z.is_none()) {
        if !ladder.is_empty() {
            log::warn!(
                "[assign] entry-zoom ladder ignored: the level plan has no zooms \
                 (a --gsd plan cannot anchor an entry zoom); use --min-zoom/--max-zoom"
            );
        }
        return vec![None; values.len()];
    }
    let finest = (level_zooms.len().saturating_sub(1)) as u8;
    values
        .iter()
        .map(|v| {
            let zoom = ladder.entry_zoom(*v)?;
            let level = level_zooms
                .iter()
                .position(|lz| lz.is_some_and(|z| z >= zoom))
                .map(|i| i as u8)
                .unwrap_or(finest);
            Some(level)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #364 S1: an explicit rung must match a value stored as `f32`.
    ///
    /// `0.425_f32` widens to `0.42500001192092896_f64`, which is not
    /// bit-equal to the `0.425_f64` a caller types. Matching rungs with
    /// `total_cmp` therefore matched nothing on a Float32 column — the common
    /// storage for contour and magnitude data — and the PR's own documented
    /// example silently fell back to gate-and-thin for every rung except the
    /// ones that happen to be exact in binary (0.5 is, 0.425 and 0.2 are not).
    #[test]
    fn explicit_rungs_match_values_widened_from_f32() {
        let column: Vec<Option<f64>> = [0.5f32, 0.425, 0.2, 0.425]
            .iter()
            .map(|v| Some(*v as f64))
            .collect();
        let spec = EntryZoomSpec {
            column: "level".to_string(),
            kind: EntryZoomKind::Explicit(vec![(0.5, 8), (0.425, 9), (0.2, 12)]),
        };
        let ladder = build_ladder(&spec, &column, &[Some(8), Some(10), Some(12)]).unwrap();
        let got: Vec<Option<u8>> = column.iter().map(|v| ladder.entry_zoom(*v)).collect();
        assert_eq!(
            got,
            vec![Some(8), Some(9), Some(12), Some(9)],
            "every spec'd rung must match its column value despite f32 widening"
        );
    }

    /// #364 S1: more distinct values than zooms must not pin the surplus to
    /// the finest level.
    ///
    /// Clamping rank*step to `max_zoom` gave every surplus value an entry zoom
    /// of `max_zoom`. Because a laddered feature is authoritative — it appears
    /// from its entry level inward and is removed from every winner grid — that
    /// made the bulk of a high-cardinality column appear ONLY at the finest
    /// level, silently emptying the coarse pyramid. That is strictly worse than
    /// not laddering those features at all, so they get no ladder opinion and
    /// fall back to the ordinary gate and thinning.
    #[test]
    fn values_past_the_finest_zoom_get_no_ladder_opinion() {
        // 12 distinct values, step 1, zooms 0..=6 → only 7 rungs fit.
        let values: Vec<f64> = (0..12).map(f64::from).collect();
        let ladder = EntryZoomLadder::dense_rank(values.iter().copied(), 0, 1, 6).unwrap();

        // Strongest (11.0) enters at z0, each weaker one a zoom later.
        assert_eq!(ladder.entry_zoom(Some(11.0)), Some(0));
        assert_eq!(ladder.entry_zoom(Some(5.0)), Some(6));
        // Rank 7 and beyond would need z7+, which the plan does not have.
        for surplus in [4.0, 3.0, 2.0, 1.0, 0.0] {
            assert_eq!(
                ladder.entry_zoom(Some(surplus)),
                None,
                "{surplus} has no rung, so it must keep the ordinary behaviour \
                 rather than being pinned to the finest level"
            );
        }
    }

    /// The motivating case (#364): five contour magnitudes, strongest first,
    /// one zoom apart. The ladder must invert the size ordering that the
    /// visibility gate would otherwise impose.
    #[test]
    fn dense_rank_places_the_strongest_value_earliest() {
        let l = EntryZoomLadder::dense_rank([0.2, 0.275, 0.35, 0.425, 0.5], 8, 1, 14).unwrap();
        assert_eq!(l.entry_zoom(Some(0.5)), Some(8));
        assert_eq!(l.entry_zoom(Some(0.425)), Some(9));
        assert_eq!(l.entry_zoom(Some(0.35)), Some(10));
        assert_eq!(l.entry_zoom(Some(0.275)), Some(11));
        assert_eq!(l.entry_zoom(Some(0.2)), Some(12));
    }

    /// Ranks come from *distinct* values, not from the values' positions on
    /// their nominal scale. These occupy 0.2-0.5 of a nominal 0-1; a linear
    /// mapping would strand every rung in the upper half of the zoom range,
    /// which is the failure the reference pipeline calls out.
    #[test]
    fn ladder_is_scale_free_over_a_narrow_value_range() {
        let narrow_vals = [0.5, 0.425, 0.35, 0.275, 0.2]; // descending
        let wide_vals = [1e6, 90_000.0, 5000.0, 100.0, 1.0]; // descending
        let narrow = EntryZoomLadder::dense_rank(narrow_vals, 0, 1, 14).unwrap();
        let wide = EntryZoomLadder::dense_rank(wide_vals, 0, 1, 14).unwrap();

        // Walking each set strongest-first must give the same zooms, however
        // far apart the magnitudes are.
        let zooms = |l: &EntryZoomLadder, vs: [f64; 5]| -> Vec<Option<u8>> {
            vs.iter().map(|&v| l.entry_zoom(Some(v))).collect()
        };
        let expected: Vec<Option<u8>> = (0..5).map(|z| Some(z as u8)).collect();
        assert_eq!(zooms(&narrow, narrow_vals), expected);
        assert_eq!(zooms(&wide, wide_vals), expected);
    }

    /// Repeats collapse: the ladder has one rung per distinct value, so a
    /// column with a skewed distribution still gets an even ladder.
    #[test]
    fn repeated_values_collapse_to_one_rung() {
        let l =
            EntryZoomLadder::dense_rank([0.2, 0.2, 0.2, 0.2, 0.5, 0.5, 0.35], 4, 1, 14).unwrap();
        assert_eq!(l.len(), 3);
        assert_eq!(l.entry_zoom(Some(0.5)), Some(4));
        assert_eq!(l.entry_zoom(Some(0.35)), Some(5));
        assert_eq!(l.entry_zoom(Some(0.2)), Some(6));
    }

    #[test]
    fn step_widens_the_spacing_and_ranks_past_the_finest_zoom_are_dropped() {
        let l = EntryZoomLadder::dense_rank([1.0, 2.0, 3.0, 4.0], 0, 3, 7).unwrap();
        assert_eq!(l.entry_zoom(Some(4.0)), Some(0));
        assert_eq!(l.entry_zoom(Some(3.0)), Some(3));
        assert_eq!(l.entry_zoom(Some(2.0)), Some(6));
        // Rank 3 would land at 9; the archive stops at 7. Clamping it onto 7
        // would say "appears at the finest level only" — worse for that
        // feature than never laddering it, since a laddered feature is also
        // removed from every winner grid. No rung, no opinion.
        assert_eq!(l.entry_zoom(Some(1.0)), None);
        assert_eq!(l.len(), 3, "only the rungs that fit are kept");
    }

    /// The boundary: a rank landing exactly on `max_zoom` is kept.
    #[test]
    fn a_rung_landing_exactly_on_the_finest_zoom_is_kept() {
        let l = EntryZoomLadder::dense_rank([1.0, 2.0, 3.0], 0, 1, 2).unwrap();
        assert_eq!(l.entry_zoom(Some(3.0)), Some(0));
        assert_eq!(l.entry_zoom(Some(2.0)), Some(1));
        assert_eq!(
            l.entry_zoom(Some(1.0)),
            Some(2),
            "exactly max_zoom, not past it"
        );
    }

    /// A rung naming a value no row carries is dropped with a warning, not
    /// silently retained as a rung nothing can match.
    #[test]
    fn explicit_rung_naming_no_observed_value_is_dropped() {
        let column: Vec<Option<f64>> = vec![Some(0.5), Some(0.2)];
        let spec = EntryZoomSpec {
            column: "level".to_string(),
            kind: EntryZoomKind::Explicit(vec![(0.5, 8), (0.9, 9), (0.2, 12)]),
        };
        let ladder = build_ladder(&spec, &column, &[Some(8), Some(10), Some(12)]).unwrap();
        assert_eq!(ladder.len(), 2, "0.9 names no row");
        assert_eq!(ladder.entry_zoom(Some(0.5)), Some(8));
        assert_eq!(ladder.entry_zoom(Some(0.2)), Some(12));
    }

    /// Snapping must not pull a rung onto a value that is merely closest —
    /// only onto one within the f32-widening tolerance.
    #[test]
    fn explicit_rung_does_not_snap_to_a_distant_value() {
        let column: Vec<Option<f64>> = vec![Some(1.0), Some(2.0)];
        let spec = EntryZoomSpec {
            column: "level".to_string(),
            kind: EntryZoomKind::Explicit(vec![(1.5, 8)]),
        };
        let err = build_ladder(&spec, &column, &[Some(8), Some(12)]).unwrap_err();
        assert!(
            matches!(err, LadderError::Empty),
            "1.5 is nearest to both but matches neither: {err}"
        );
    }

    /// A value the ladder does not know is not an error and not a drop: the
    /// feature simply has no ladder opinion and takes the ordinary path.
    #[test]
    fn unknown_absent_and_nonfinite_values_yield_no_entry_zoom() {
        let l = EntryZoomLadder::dense_rank([1.0, 2.0], 0, 1, 10).unwrap();
        assert_eq!(l.entry_zoom(Some(1.5)), None, "not a rung");
        assert_eq!(l.entry_zoom(None), None, "null column value");
        assert_eq!(l.entry_zoom(Some(f64::NAN)), None);
        assert_eq!(l.entry_zoom(Some(f64::INFINITY)), None);
    }

    #[test]
    fn nonfinite_inputs_are_ignored_when_deriving() {
        let l = EntryZoomLadder::dense_rank([1.0, f64::NAN, 2.0, f64::INFINITY], 0, 1, 10).unwrap();
        assert_eq!(l.len(), 2, "only the finite values become rungs");
        assert_eq!(l.entry_zoom(Some(2.0)), Some(0));
        assert_eq!(l.entry_zoom(Some(1.0)), Some(1));
    }

    #[test]
    fn explicit_rungs_are_honoured_verbatim() {
        let l = EntryZoomLadder::explicit([(0.5, 8), (0.425, 9), (0.2, 12)]).unwrap();
        assert_eq!(l.entry_zoom(Some(0.5)), Some(8));
        assert_eq!(l.entry_zoom(Some(0.2)), Some(12));
        assert_eq!(l.entry_zoom(Some(0.35)), None, "unlisted values are free");
    }

    #[test]
    fn degenerate_specs_are_rejected() {
        assert_eq!(
            EntryZoomLadder::dense_rank([1.0, 2.0], 0, 0, 10),
            Err(LadderError::ZeroStep)
        );
        assert_eq!(
            EntryZoomLadder::explicit(std::iter::empty()),
            Err(LadderError::Empty)
        );
        assert!(matches!(
            EntryZoomLadder::explicit([(f64::NAN, 3)]),
            Err(LadderError::NonFiniteValue(v)) if v.is_nan()
        ));
    }

    /// An empty column (every value null) is not an error — it yields a ladder
    /// with no rungs, which leaves every feature on the ordinary path.
    #[test]
    fn empty_input_gives_an_inert_ladder() {
        let l = EntryZoomLadder::dense_rank(std::iter::empty(), 0, 1, 10).unwrap();
        assert!(l.is_empty());
        assert_eq!(l.entry_zoom(Some(1.0)), None);
    }

    #[test]
    fn provenance_map_is_ordered_by_value() {
        let l = EntryZoomLadder::dense_rank([0.2, 0.5, 0.35], 3, 1, 14).unwrap();
        let m = l.to_map();
        assert_eq!(m["0.5"], 3, "strongest enters at the base zoom");
        assert_eq!(m["0.35"], 4);
        assert_eq!(m["0.2"], 5);
        assert_eq!(m.len(), 3);
    }

    // ---- spec resolution ----------------------------------------------------

    /// z8..z12 rungs against a z6..z14 plan: each entry zoom picks the level
    /// that carries it.
    #[test]
    fn entry_zooms_resolve_to_level_indices() {
        let zooms: Vec<Option<u8>> = (6..=14).map(|z| Some(z as u8)).collect();
        let ladder = EntryZoomLadder::explicit([(0.5, 8), (0.35, 10), (0.2, 12)]).unwrap();
        let values = [Some(0.5), Some(0.35), Some(0.2), Some(0.9), None];
        let got = entry_levels(&ladder, &values, &zooms);
        assert_eq!(
            got,
            vec![Some(2), Some(4), Some(6), None, None],
            "z8/z10/z12 are levels 2/4/6 of a plan starting at z6"
        );
    }

    /// A zoom outside the plan clamps rather than escaping the level range.
    #[test]
    fn entry_zooms_outside_the_plan_clamp() {
        let zooms: Vec<Option<u8>> = (6..=9).map(|z| Some(z as u8)).collect();
        let ladder = EntryZoomLadder::explicit([(1.0, 0), (2.0, 30)]).unwrap();
        let got = entry_levels(&ladder, &[Some(1.0), Some(2.0)], &zooms);
        assert_eq!(got, vec![Some(0), Some(3)], "clamped to coarsest / finest");
    }

    /// A GSD-only plan cannot anchor an entry zoom, so the ladder goes inert
    /// rather than guessing — features keep the ordinary gate.
    #[test]
    fn ladder_is_inert_without_zooms_in_the_plan() {
        let ladder = EntryZoomLadder::explicit([(1.0, 3)]).unwrap();
        let got = entry_levels(&ladder, &[Some(1.0), Some(1.0)], &[None, None]);
        assert_eq!(got, vec![None, None]);
    }

    /// A derived ladder spans the plan: strongest at the coarsest zoom,
    /// nothing past the finest.
    #[test]
    fn derived_ladder_spans_the_level_plan() {
        let zooms: Vec<Option<u8>> = (6..=14).map(|z| Some(z as u8)).collect();
        let spec = EntryZoomSpec {
            column: "level".to_string(),
            kind: EntryZoomKind::DenseRank { step: 1 },
        };
        let values = [Some(0.2), Some(0.5), Some(0.35), None, Some(0.5)];
        let ladder = build_ladder(&spec, &values, &zooms).unwrap();
        assert_eq!(ladder.entry_zoom(Some(0.5)), Some(6), "strongest at z6");
        assert_eq!(ladder.entry_zoom(Some(0.35)), Some(7));
        assert_eq!(ladder.entry_zoom(Some(0.2)), Some(8));
    }

    #[test]
    fn explicit_spec_is_passed_through() {
        let spec = EntryZoomSpec {
            column: "level".to_string(),
            kind: EntryZoomKind::Explicit(vec![(0.5, 8), (0.2, 12)]),
        };
        let ladder = build_ladder(&spec, &[], &[Some(0), Some(14)]).unwrap();
        assert_eq!(ladder.entry_zoom(Some(0.5)), Some(8));
        assert_eq!(ladder.entry_zoom(Some(0.2)), Some(12));
    }
}
