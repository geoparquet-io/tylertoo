//! Line network coalescing for overview levels (plan Q3).
//!
//! At coarse levels, line networks (roads, rivers) degrade into scattered
//! dashes: segments whose bbox diagonal is below the visibility gate
//! (`line_visibility × GSD`, see [`super::assign`]) are dropped, and
//! cell-winner thinning keeps disconnected fragments of what remains. This
//! stage glues touching same-class segments into single "stroke"
//! LineStrings **before** the visibility gate and thinning run, so a chain
//! of individually sub-visibility segments survives as one long, visible
//! artery. That ordering — chain first, gate/thin the *chains* — is the
//! entire payoff.
//!
//! # Model (duplicating mode, non-canonical levels only)
//!
//! Per overview level:
//!
//! 1. **Group** line features by a compatibility key: the class value of
//!    the active class-ranking column (Q1 explicit `--class-rank` or the
//!    auto-detected Overture `class`/`road_class`), else all lines are
//!    compatible. Chaining never crosses groups.
//! 2. **Chain** segments within a group whose endpoints coincide, in two
//!    phases: **exact** coordinate matching first (GSD-independent — the
//!    dominant case, since OSM/Overture segments share exact node
//!    coordinates), then a **snap** pass that joins the resulting chains'
//!    free endpoints quantized to `snap_gsd_factor × GSD` grid cells
//!    (closing sub-resolution digitization gaps). Both phases join only
//!    through nodes of **degree 2** — junctions where three or more
//!    endpoints meet terminate every chain, preserving network topology.
//! 3. The merged feature takes the attributes of its **highest-priority
//!    member** (the same [`Priority`] order the cell-winner stage uses),
//!    and records the number of merged source segments
//!    (`coalesced_count`, 1 for unmerged).
//! 4. The **visibility gate** and **cell-winner thinning** then run on the
//!    merged chains (gate on the chain's bbox diagonal; one chain per
//!    `line_thinning × GSD` grid cell, best [`Priority`] wins).
//!
//! The canonical level is NEVER coalesced (spec §2.4 value fidelity), and
//! partitioning mode rejects coalescing outright (merged geometries violate
//! §2.3's feature-once / geometry-verbatim contract; see
//! `ConvertError::CoalescePartitioningUnsupported`).
//!
//! # DIVERGENCE FROM TIPPECANOE
//!
//! Tippecanoe's `--coalesce` merges *consecutive* same-attribute features
//! into multi-geometries without topological chaining (tile.cpp, coalesce
//! path); its shared-node machinery protects junction vertices during
//! simplification rather than building strokes. Our endpoint chaining
//! through degree-2 nodes matches planetiler's `mergeLineStrings`
//! behavior (the standard cartographic stroke-building operation), while
//! keeping tippecanoe's *philosophy*: never merge across attribute
//! (class) boundaries, never merge through junctions, deterministic
//! output. We also merge with a snap tolerance of one GSD — two endpoints
//! closer than one ground sample are indistinguishable at the level — where
//! planetiler uses exact matches after tile-grid quantization (an
//! equivalent idea in tile space).
//!
//! # Determinism
//!
//! Segments are processed in input order; joins are a pure function of the
//! endpoint grid; walk starts are chosen by smallest feature index; ties in
//! gating/thinning fall back to the strict [`Priority`] total order. No
//! result depends on hash-map iteration order.

use std::collections::HashMap;

use geo::{BoundingRect, Geometry, LineString};

use super::assign::{AssignConfig, AssignFeature, FeatureKind, Priority};
use super::level::Crs;

/// Name of the merged-segment-count column written when coalescing is
/// enabled (INT32, 1 for unmerged rows; always 1 at the canonical level).
pub const COALESCED_COUNT_COLUMN: &str = "coalesced_count";

/// Default endpoint snap tolerance, in GSD multiples: two endpoints within
/// one ground sample distance are indistinguishable at the level.
pub const DEFAULT_SNAP_GSD_FACTOR: f64 = 1.0;

/// Default per-level candidate-row ceiling above which coalescing is
/// skipped (bounded-memory guard; see `docs/OVERVIEW_TUNING.md`). Chaining
/// needs the level's line geometries in memory at once; levels larger than
/// this are near-canonical, where segments are individually visible and
/// coalescing matters least.
pub const DEFAULT_COALESCE_MAX_LEVEL_ROWS: usize = 2_000_000;

/// One candidate line feature for a level's coalescing pass.
#[derive(Debug, Clone)]
pub struct CoalesceInput<'a> {
    /// Caller-owned feature identifier (typically the source row index).
    pub index: usize,
    /// The feature's geometry. Only single `LineString`s participate in
    /// chaining; `MultiLineString`/`Line` features pass through as
    /// unmerged singletons (still gated and thinned as chains of one).
    pub geom: &'a Geometry<f64>,
    /// Cell-winner sort key (Q1 ranking), for priority inheritance.
    pub sort_key: Option<f64>,
    /// Compatibility group id (interned class value). Chaining never
    /// crosses groups.
    pub group: u32,
}

/// One surviving coalesced chain at a level.
#[derive(Debug, Clone, PartialEq)]
pub struct CoalescedLine {
    /// Index of the highest-priority member — the source row that donates
    /// the merged feature's attributes.
    pub rep: usize,
    /// Number of source segments merged into this chain (>= 1).
    pub count: i32,
    /// The merged geometry (a single `LineString` for merged chains; the
    /// original geometry for singletons).
    pub geom: Geometry<f64>,
}

/// End discriminant of a chainable segment: 0 = first vertex, 1 = last.
type End = u8;

/// Quantized endpoint node key, namespaced by compatibility group.
type NodeKey = (u32, i64, i64);

/// Coalesce one level's candidate lines: chain within compatibility groups
/// through degree-2 endpoint nodes, then apply the visibility gate and
/// cell-winner thinning to the merged chains. Returns the surviving chains
/// ordered by ascending `rep` index.
///
/// - `gsd_m`: the level's GSD in meters (governs snap tolerance, gate, and
///   thinning grid). Non-positive GSD returns every input as an ungated
///   singleton (degenerate; callers pass canonical levels elsewhere).
/// - `config`: the assignment configuration; `line_visibility`,
///   `line_thinning`, and `sort_direction` are used.
/// - `snap_gsd_factor`: endpoint snap tolerance in GSD multiples. `<= 0`
///   means exact coordinate matching only.
pub fn coalesce_level_lines(
    lines: &[CoalesceInput<'_>],
    gsd_m: f64,
    crs: Crs,
    config: &AssignConfig,
    snap_gsd_factor: f64,
) -> Vec<CoalescedLine> {
    if lines.is_empty() {
        return Vec::new();
    }
    let gsd_units = crs.meters_to_units(gsd_m);
    let snap_tol = if gsd_units > 0.0 {
        snap_gsd_factor * gsd_units
    } else {
        0.0
    };

    // --- 1+2. Build chains (grouped, degree-2 endpoint joins). --------------
    let chains = build_chains(lines, snap_tol);

    // --- Priorities + sort keys of the members (the cell-winner order, Q1). --
    let mut prio: HashMap<usize, Priority> = HashMap::with_capacity(lines.len());
    let mut sort_keys: HashMap<usize, Option<f64>> = HashMap::with_capacity(lines.len());
    for l in lines {
        let feat = AssignFeature {
            index: l.index,
            bbox: geom_bbox(l.geom),
            kind: FeatureKind::Line,
            sort_key: l.sort_key,
        };
        prio.insert(l.index, Priority::new(&feat, config.sort_direction));
        sort_keys.insert(l.index, l.sort_key);
    }

    // --- 3. Merge geometry + inherit the best member's attributes. ----------
    let mut merged: Vec<CoalescedLine> = Vec::with_capacity(chains.len());
    for chain in chains {
        let rep = chain
            .members
            .iter()
            .copied()
            .reduce(|best, m| {
                if prio[&m].beats(&prio[&best]) {
                    m
                } else {
                    best
                }
            })
            .expect("chain has at least one member");
        merged.push(CoalescedLine {
            rep,
            count: chain.members.len() as i32,
            geom: chain.geom,
        });
    }

    // --- 4a. Visibility gate on the CHAIN's bbox diagonal. -------------------
    let gate = config.line_visibility * gsd_units;
    let gate_sq = gate * gate;
    let mut gated: Vec<(CoalescedLine, AssignFeature)> = Vec::with_capacity(merged.len());
    for line in merged {
        let bbox = geom_bbox(&line.geom);
        let (dx, dy) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
        if gate > 0.0 && dx * dx + dy * dy < gate_sq {
            continue; // whole chain still below visibility at this level
        }
        // The chain competes in thinning with its rep member's sort key but
        // its OWN bbox: a long merged artery out-ranks the short fragments a
        // non-coalesced run would have fielded in the same cell.
        let feat = AssignFeature {
            index: line.rep,
            bbox,
            kind: FeatureKind::Line,
            sort_key: sort_keys[&line.rep],
        };
        gated.push((line, feat));
    }

    // --- 4b. Cell-winner thinning among the surviving chains. ----------------
    let cell_size = gsd_units * config.line_thinning;
    let survivors: Vec<CoalescedLine> = if cell_size > 0.0 && !cell_size.is_nan() {
        let chain_prio: Vec<Priority> = gated
            .iter()
            .map(|(_, f)| Priority::new(f, config.sort_direction))
            .collect();
        // cell -> position of the best chain so far in `gated`.
        let mut grid: HashMap<(i64, i64), usize> = HashMap::new();
        for (pos, (_, feat)) in gated.iter().enumerate() {
            let (cx, cy) = feat.center();
            let key = (
                (cx / cell_size).floor() as i64,
                (cy / cell_size).floor() as i64,
            );
            grid.entry(key)
                .and_modify(|best| {
                    if chain_prio[pos].beats(&chain_prio[*best]) {
                        *best = pos;
                    }
                })
                .or_insert(pos);
        }
        let mut keep: Vec<usize> = grid.into_values().collect();
        keep.sort_unstable();
        let mut out = Vec::with_capacity(keep.len());
        let mut gated = gated;
        // Drain winners in ascending position order (stable, deterministic).
        for (offset, pos) in keep.into_iter().enumerate() {
            out.push(gated.remove(pos - offset).0);
        }
        out
    } else {
        gated.into_iter().map(|(l, _)| l).collect()
    };

    let mut survivors = survivors;
    survivors.sort_by_key(|c| c.rep);
    survivors
}

/// `[xmin, ymin, xmax, ymax]` of a geometry (`[0;4]` when undefined).
fn geom_bbox(g: &Geometry<f64>) -> [f64; 4] {
    match g.bounding_rect() {
        Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
        None => [0.0, 0.0, 0.0, 0.0],
    }
}

/// An assembled chain before priority/gating: its ordered members and the
/// merged geometry.
struct RawChain {
    members: Vec<usize>,
    geom: Geometry<f64>,
}

/// A joinable polyline piece during chaining: one or more already-merged
/// source segments with an owned, oriented coordinate run.
struct Piece {
    /// Source feature indices ([`CoalesceInput::index`]) merged so far.
    members: Vec<usize>,
    coords: Vec<geo::Coord<f64>>,
    group: u32,
}

/// Exact node key: coordinate bit pattern (phase 1).
#[inline]
fn exact_key(group: u32, c: geo::Coord<f64>) -> NodeKey {
    (group, c.x.to_bits() as i64, c.y.to_bits() as i64)
}

/// Snapped node key: endpoints quantized to `tol`-sized grid cells (phase 2).
#[inline]
fn snap_key(group: u32, c: geo::Coord<f64>, tol: f64) -> NodeKey {
    (
        group,
        (c.x / tol).floor() as i64,
        (c.y / tol).floor() as i64,
    )
}

/// Build the chains for one level in **two phases**:
///
/// 1. **Exact matching**: chain segments whose endpoints are bit-identical
///    (the dominant case — OSM/Overture networks share exact node
///    coordinates). This is GSD-independent, so chains form even when
///    segments are far shorter than the snap tolerance (the coarse-zoom
///    norm, where a one-phase snapped graph would collapse a short
///    segment's own two endpoints into a single node and never join it).
/// 2. **Snap pass**: re-run the same degree-2 join over the *resulting
///    chains'* free endpoints, quantized to `snap_tol` grid cells —
///    closing small digitization gaps at the level's resolution. Skipped
///    when `snap_tol <= 0` (exact matching only).
fn build_chains(lines: &[CoalesceInput<'_>], snap_tol: f64) -> Vec<RawChain> {
    // Chainable = plain LineString with >= 2 coordinates. Everything else
    // (MultiLineString, Line, degenerate) is an unmerged singleton.
    let mut pieces: Vec<Piece> = Vec::new();
    let mut singles: Vec<usize> = Vec::new(); // positions in `lines`
    for (pos, l) in lines.iter().enumerate() {
        match l.geom {
            Geometry::LineString(ls) if ls.0.len() >= 2 => pieces.push(Piece {
                members: vec![l.index],
                coords: ls.0.clone(),
                group: l.group,
            }),
            _ => singles.push(pos),
        }
    }

    // Phase 1: exact endpoint matching; phase 2: snapped matching.
    let pieces = join_pieces(pieces, exact_key);
    let pieces = if snap_tol > 0.0 {
        join_pieces(pieces, |g, c| snap_key(g, c, snap_tol))
    } else {
        pieces
    };

    let mut chains: Vec<RawChain> = pieces
        .into_iter()
        .map(|p| RawChain {
            geom: Geometry::LineString(LineString::new(p.coords)),
            members: p.members,
        })
        .collect();

    // Non-chainable singletons pass through unmerged (original geometry).
    for pos in singles {
        chains.push(RawChain {
            members: vec![lines[pos].index],
            geom: lines[pos].geom.clone(),
        });
    }
    chains
}

/// One round of degree-2 endpoint joining over `pieces`, with node keys
/// produced by `key(group, endpoint)`. Nodes with exactly two incident
/// endpoints from two DISTINCT pieces join; junctions (degree >= 3) and
/// self-loops never do. Deterministic: components are walked in ascending
/// piece index, starting from a free end (or the lowest-index piece of a
/// cycle).
fn join_pieces(pieces: Vec<Piece>, key: impl Fn(u32, geo::Coord<f64>) -> NodeKey) -> Vec<Piece> {
    // Endpoint node map: node -> incident (piece idx, end).
    let mut nodes: HashMap<NodeKey, Vec<(usize, End)>> = HashMap::new();
    for (pi, p) in pieces.iter().enumerate() {
        let first = p.coords[0];
        let last = p.coords[p.coords.len() - 1];
        nodes.entry(key(p.group, first)).or_default().push((pi, 0));
        nodes.entry(key(p.group, last)).or_default().push((pi, 1));
    }

    // Joins: per piece, the (other piece, other end) connected at each end.
    let mut joins: Vec<[Option<(usize, End)>; 2]> = vec![[None, None]; pieces.len()];
    for incidents in nodes.values() {
        if incidents.len() == 2 && incidents[0].0 != incidents[1].0 {
            let (a, a_end) = incidents[0];
            let (b, b_end) = incidents[1];
            joins[a][a_end as usize] = Some((b, b_end));
            joins[b][b_end as usize] = Some((a, a_end));
        }
    }

    // Walk components deterministically (ascending piece index).
    let mut visited = vec![false; pieces.len()];
    let mut order: Vec<Vec<(usize, End)>> = Vec::new();
    for start in 0..pieces.len() {
        if visited[start] {
            continue;
        }
        // Find the walk head: follow joins backwards from `start`'s first
        // end until a free end, or detect a cycle (back at `start`).
        let (mut cur, mut cur_end) = (start, 0u8);
        loop {
            match joins[cur][cur_end as usize] {
                None => break, // (cur, cur_end) is a free end: the head
                Some((prev, prev_end)) => {
                    if prev == start {
                        // Came back around: pure cycle. Walk from `start`;
                        // the forward walk's `visited` check closes it.
                        cur = start;
                        cur_end = 0;
                        break;
                    }
                    cur = prev;
                    cur_end = 1 - prev_end;
                }
            }
        }

        // Forward walk from the head, entering each piece at `entry` and
        // exiting at the opposite end.
        let (head, head_entry) = (cur, cur_end);
        let mut walk: Vec<(usize, End)> = vec![(head, head_entry)];
        visited[head] = true;
        let (mut cur, mut entry) = (head, head_entry);
        loop {
            let exit = 1 - entry;
            match joins[cur][exit as usize] {
                Some((next, next_end)) if !visited[next] => {
                    walk.push((next, next_end));
                    visited[next] = true;
                    cur = next;
                    entry = next_end;
                }
                _ => break, // free end, junction, or cycle closed
            }
        }
        order.push(walk);
    }

    // Assemble merged pieces. Consume the inputs by index (each appears in
    // exactly one walk), reversing where the walk entered at the far end.
    let mut slots: Vec<Option<Piece>> = pieces.into_iter().map(Some).collect();
    let mut out: Vec<Piece> = Vec::with_capacity(order.len());
    for walk in order {
        if walk.len() == 1 {
            let (pi, _) = walk[0];
            out.push(slots[pi].take().expect("piece consumed once"));
            continue;
        }
        let mut members: Vec<usize> = Vec::new();
        let mut coords: Vec<geo::Coord<f64>> = Vec::new();
        let mut group = 0u32;
        for &(pi, seg_entry) in &walk {
            let mut p = slots[pi].take().expect("piece consumed once");
            members.append(&mut p.members);
            group = p.group;
            if seg_entry == 1 {
                p.coords.reverse();
            }
            for c in p.coords {
                if coords.last() == Some(&c) {
                    continue; // exact shared node: no duplicate vertex
                }
                coords.push(c);
            }
        }
        out.push(Piece {
            members,
            coords,
            group,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, MultiLineString};

    fn ls(coords: &[(f64, f64)]) -> Geometry<f64> {
        Geometry::LineString(LineString::from(coords.to_vec()))
    }

    fn input<'a>(index: usize, geom: &'a Geometry<f64>) -> CoalesceInput<'a> {
        CoalesceInput {
            index,
            geom,
            sort_key: None,
            group: 0,
        }
    }

    /// Small GSD so gates/thinning never interfere unless a test wants them.
    const TINY_GSD: f64 = 1e-6;

    fn cfg() -> AssignConfig {
        AssignConfig::default()
    }

    fn run<'a>(lines: &[CoalesceInput<'a>], gsd_m: f64) -> Vec<CoalescedLine> {
        coalesce_level_lines(lines, gsd_m, Crs::Epsg3857, &cfg(), DEFAULT_SNAP_GSD_FACTOR)
    }

    fn coords_of(g: &Geometry<f64>) -> Vec<(f64, f64)> {
        match g {
            Geometry::LineString(ls) => ls.0.iter().map(|c| (c.x, c.y)).collect(),
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    // --- chaining ------------------------------------------------------------

    #[test]
    fn two_touching_segments_merge() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let lines = [input(0, &a), input(1, &b)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 1, "touching segments must merge: {out:?}");
        assert_eq!(out[0].count, 2);
        assert_eq!(
            coords_of(&out[0].geom),
            vec![(0.0, 0.0), (100.0, 0.0), (200.0, 0.0)],
            "shared node vertex deduplicated"
        );
    }

    #[test]
    fn three_collinear_segments_merge_into_one() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let c = ls(&[(200.0, 0.0), (300.0, 0.0)]);
        let lines = [input(0, &a), input(1, &b), input(2, &c)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].count, 3);
        assert_eq!(coords_of(&out[0].geom).len(), 4);
    }

    #[test]
    fn reversed_orientation_still_merges() {
        // b runs INTO the shared node: (200,0) -> (100,0). The walk must
        // reverse it so the merged line is continuous.
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(200.0, 0.0), (100.0, 0.0)]);
        let lines = [input(0, &a), input(1, &b)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 1);
        let c = coords_of(&out[0].geom);
        assert_eq!(c.len(), 3, "no duplicate shared vertex: {c:?}");
        assert!(
            c == vec![(0.0, 0.0), (100.0, 0.0), (200.0, 0.0)]
                || c == vec![(200.0, 0.0), (100.0, 0.0), (0.0, 0.0)]
        );
    }

    #[test]
    fn t_junction_degree_three_does_not_merge_through() {
        // Three segments meeting at (100, 0): degree-3 node, no chaining.
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let c = ls(&[(100.0, 0.0), (100.0, 100.0)]);
        let lines = [input(0, &a), input(1, &b), input(2, &c)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 3, "junction must terminate chains: {out:?}");
        assert!(out.iter().all(|l| l.count == 1));
    }

    #[test]
    fn snap_tolerance_joins_near_endpoints() {
        // gsd = 1 m, snap 1.0 => endpoints quantized to 1 m cells. Endpoints
        // 0.3 m apart in the same cell join; endpoints 2 m apart do not.
        let a = ls(&[(0.0, 0.0), (100.2, 0.2)]);
        let b = ls(&[(100.4, 0.4), (200.0, 0.0)]); // same 1 m cell as a's end
        let c = ls(&[(202.0, 0.0), (300.0, 0.0)]); // 2 m gap: separate cell
        let lines = [input(0, &a), input(1, &b), input(2, &c)];
        let out = run(&lines, 1.0);
        assert_eq!(out.len(), 2, "near endpoints join, far do not: {out:?}");
        let merged = out.iter().find(|l| l.count == 2).expect("merged chain");
        // Snapped (non-identical) join keeps both endpoint vertices.
        assert_eq!(coords_of(&merged.geom).len(), 4);
    }

    #[test]
    fn exact_matching_when_snap_zero() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]); // exact match
        let c = ls(&[(200.0000001, 0.0), (300.0, 0.0)]); // near, not exact
        let lines = [input(0, &a), input(1, &b), input(2, &c)];
        let out = coalesce_level_lines(&lines, 1.0, Crs::Epsg3857, &cfg(), 0.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().map(|l| l.count).max(), Some(2));
    }

    #[test]
    fn class_mismatch_does_not_merge() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let mut ia = input(0, &a);
        let mut ib = input(1, &b);
        ia.group = 1;
        ib.group = 2;
        let out = run(&[ia, ib], TINY_GSD);
        assert_eq!(out.len(), 2, "different classes never chain");
    }

    #[test]
    fn junction_degree_counted_within_group_only() {
        // A minor road (group 9) also touches the shared node; within the
        // motorway group the node is still degree 2, so the motorway chains
        // straight through the cross-class junction.
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let minor = ls(&[(100.0, 0.0), (100.0, 50.0)]);
        let mut ia = input(0, &a);
        let mut ib = input(1, &b);
        let mut im = input(2, &minor);
        ia.group = 1;
        ib.group = 1;
        im.group = 9;
        let out = run(&[ia, ib, im], TINY_GSD);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().map(|l| l.count).max(), Some(2));
    }

    #[test]
    fn priority_attribute_inheritance() {
        // The member holding a sort key out-ranks the keyless one (Q1 null
        // loses); the merged feature must take ITS index as rep.
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (150.0, 0.0)]); // shorter, but has key
        let ia = input(7, &a);
        let mut ib = input(3, &b);
        ib.sort_key = Some(5.0);
        let out = run(&[ia, ib], TINY_GSD);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rep, 3, "sort-key holder donates attributes");
        assert_eq!(out[0].count, 2);
    }

    #[test]
    fn cycle_merges_into_closed_chain() {
        // Four segments forming a square ring, all degree-2 nodes.
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (100.0, 100.0)]);
        let c = ls(&[(100.0, 100.0), (0.0, 100.0)]);
        let d = ls(&[(0.0, 100.0), (0.0, 0.0)]);
        let lines = [input(0, &a), input(1, &b), input(2, &c), input(3, &d)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 1, "ring merges into one chain: {out:?}");
        assert_eq!(out[0].count, 4);
        let coords = coords_of(&out[0].geom);
        assert_eq!(coords.first(), coords.last(), "cycle closes");
    }

    #[test]
    fn self_loop_is_not_merged_with_itself() {
        // A closed ring segment (start == end) has both ends at one node:
        // degree 2 from the SAME segment — never joined.
        let ring = ls(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 0.0)]);
        let lines = [input(0, &ring)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].count, 1);
        assert_eq!(out[0].geom, ring);
    }

    #[test]
    fn multilinestring_passes_through_as_singleton() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::from(vec![(0.0, 0.0), (100.0, 0.0)]),
            LineString::from(vec![(300.0, 0.0), (400.0, 0.0)]),
        ]));
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]); // touches mls part 1's end
        let lines = [input(0, &mls), input(1, &b)];
        let out = run(&lines, TINY_GSD);
        assert_eq!(out.len(), 2, "multilines never chain");
        assert!(out.iter().all(|l| l.count == 1));
    }

    // --- gate + thinning on chains (THE payoff ordering) ----------------------

    #[test]
    fn sub_visibility_fragments_survive_as_one_chain() {
        // gsd = 10 m, line_visibility = 2 => gate = 20 m. Five collinear 8 m
        // segments each fail the gate alone (8 < 20) but their 40 m chain
        // passes. An isolated 8 m segment far away is dropped.
        let segs: Vec<Geometry<f64>> = (0..5)
            .map(|i| ls(&[(i as f64 * 8.0, 0.0), ((i + 1) as f64 * 8.0, 0.0)]))
            .collect();
        let lone = ls(&[(10_000.0, 0.0), (10_008.0, 0.0)]);
        let mut lines: Vec<CoalesceInput> =
            segs.iter().enumerate().map(|(i, g)| input(i, g)).collect();
        lines.push(input(5, &lone));
        let out = run(&lines, 10.0);
        assert_eq!(out.len(), 1, "chain survives, lone fragment drops: {out:?}");
        assert_eq!(out[0].count, 5);
        let coords = coords_of(&out[0].geom);
        assert_eq!(coords.first(), Some(&(0.0, 0.0)));
        assert_eq!(coords.last(), Some(&(40.0, 0.0)));
    }

    #[test]
    fn thinning_keeps_one_chain_per_cell() {
        // gsd = 1000 m, line_thinning = 1 => 1000 m cells. Two disjoint
        // parallel chains whose centers share a cell: one winner.
        let a = ls(&[(0.0, 0.0), (900.0, 0.0)]);
        let b = ls(&[(0.0, 10.0), (900.0, 10.0)]);
        let lines = [input(0, &a), input(1, &b)];
        // Both pass the 2000 m gate? diag = 900 < 2000 -> both would drop.
        // Use a lenient gate for this test.
        let cfg = AssignConfig {
            line_visibility: 0.5, // gate 500 m < 900 m diag
            ..AssignConfig::default()
        };
        let out = coalesce_level_lines(&lines, 1000.0, Crs::Epsg3857, &cfg, 1.0);
        assert_eq!(out.len(), 1, "one chain per thinning cell: {out:?}");
    }

    #[test]
    fn longer_chain_wins_thinning_cell() {
        // Same cell, no sort keys: the longer (bigger diagonal) chain wins.
        let long = ls(&[(0.0, 0.0), (900.0, 0.0)]);
        let short = ls(&[(0.0, 10.0), (400.0, 10.0)]);
        let lines = [input(0, &long), input(1, &short)];
        let cfg = AssignConfig {
            line_visibility: 0.1,
            ..AssignConfig::default()
        };
        let out = coalesce_level_lines(&lines, 1000.0, Crs::Epsg3857, &cfg, 1.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rep, 0, "longer chain out-ranks in its cell");
    }

    // --- determinism / misc ----------------------------------------------------

    #[test]
    fn output_is_input_order_independent() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let c = ls(&[(500.0, 0.0), (600.0, 0.0)]);
        let l1 = [input(0, &a), input(1, &b), input(2, &c)];
        let l2 = [input(2, &c), input(1, &b), input(0, &a)];
        let mut o1 = run(&l1, TINY_GSD);
        let mut o2 = run(&l2, TINY_GSD);
        o1.sort_by_key(|l| l.rep);
        o2.sort_by_key(|l| l.rep);
        assert_eq!(o1, o2, "results independent of input order");
    }

    #[test]
    fn empty_input_is_noop() {
        assert!(run(&[], 10.0).is_empty());
    }

    #[test]
    fn degenerate_single_point_line_is_gated_out() {
        let dot = Geometry::LineString(LineString::new(vec![Coord { x: 1.0, y: 1.0 }]));
        let lines = [input(0, &dot)];
        let out = run(&lines, 10.0);
        assert!(out.is_empty(), "zero-diagonal chain fails the gate");
    }
}
