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
//!    (closing sub-resolution digitization gaps). Both phases join nodes
//!    of **degree 2** unconditionally — junctions (degree >= 3) terminate
//!    chains by default, preserving network topology. Optionally
//!    ([`CoalesceParams::junction_angle_deg`] > 0, default OFF per
//!    maintainer render review), the incident pairs at a junction that
//!    continue each other within that deviation of straight merge
//!    best-pair first (stroke building).
//! 3. The merged feature takes the attributes of its **highest-priority
//!    member** (the same [`Priority`] order the cell-winner stage uses),
//!    and records the number of merged source segments
//!    (`coalesced_count`, 1 for unmerged).
//! 4. The **visibility gate** and **cell-winner thinning** then run on the
//!    merged chains (gate on the chain's bbox diagonal; one chain per
//!    `line_thinning × GSD` grid cell, best [`Priority`] wins).
//!
//! The canonical level is NEVER coalesced (spec §2.4 value fidelity), and
//! coalescing is INERT in partitioning mode (merged geometries violate
//! §2.3's feature-once / geometry-verbatim contract; spec §13.5).
//! Coalescing is ON by default in the conversion options (maintainer
//! render review 2026-07-03: defaults should look right), with an opt-out.
//!
//! # DIVERGENCE FROM TIPPECANOE
//!
//! Tippecanoe's `--coalesce` merges *consecutive* same-attribute features
//! into multi-geometries without topological chaining (tile.cpp, coalesce
//! path); its shared-node machinery protects junction vertices during
//! simplification rather than building strokes. Our endpoint chaining
//! through degree-2 nodes (plus angle-limited junction continuation)
//! matches planetiler's `mergeLineStrings` behavior (the standard
//! cartographic stroke-building operation), while keeping tippecanoe's
//! *philosophy*: never merge across attribute (class) boundaries,
//! deterministic output. We also merge with a snap tolerance of one GSD —
//! two endpoints
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

use std::borrow::Cow;
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

/// Default junction continuation threshold, in degrees. **`0` = OFF**
/// (strict degree-2 chaining): the maintainer's Portland junction-angle
/// sweep (`corpus/data/bench/q3/portland-roads-junction{00,30}.pmtiles`,
/// reviewed 2026-07-03) found strict degree-2 chaining renders better —
/// junction continuation over-merges. The knob remains available
/// (`--coalesce-junction-angle DEG`): at a junction (node degree >= 3)
/// the pair of incident lines that best continue each other merge when
/// their deviation from a straight continuation is at most the angle.
pub const DEFAULT_JUNCTION_ANGLE_DEG: f64 = 0.0;

/// Default per-level candidate-row ceiling above which coalescing is
/// skipped (bounded-memory guard; see `docs/OVERVIEW_TUNING.md`). Chaining
/// needs the level's line geometries in memory at once; levels larger than
/// this are near-canonical, where segments are individually visible and
/// coalescing matters least.
pub const DEFAULT_COALESCE_MAX_LEVEL_ROWS: usize = 2_000_000;

/// Nominal retained bytes per candidate line, used to read the
/// [`DEFAULT_COALESCE_MAX_LEVEL_ROWS`] row ceiling as a **memory** ceiling
/// (#449).
///
/// A row count does not bound memory: 2M two-point road segments model at
/// 168 MiB of retained geometry, 2M 500-vertex contour lines at ~15 GiB. The
/// guard's whole purpose is the bound, so it trips on whichever comes
/// first — `coalesce_max_level_rows` lines, or `coalesce_max_level_rows ×`
/// this many bytes of *modelled* line geometry ([`collected_line_bytes`]:
/// 56 B + 16 B per vertex). At the default ceiling that second limb is
/// 2M × 512 B = 1.024 GB (≈977 MiB, "≈1 GB") modelled — ≈1.2 GiB resident,
/// since the model omits allocator headers, `Vec` growth slack and the side
/// vectors.
///
/// **Where the byte limb binds.** 512 B is 56 + 16 × 28.5, so on a line set
/// averaging more than ~28 vertices per line it trips *before* the row limb:
/// in practice, once the input's lines total more than ~57–64 million
/// vertices while numbering under 2M (e.g. 2M lines × 29 vertices, 1M × 61,
/// 250k × 253). Road networks split at intersections sit well under it
/// (Overture transportation segments average ≈8 vertices, ≈190 B modelled),
/// but **unsplit** line data does not: OSM ways, rivers and streams,
/// admin/coastline boundaries, contours, GPS tracks. Such inputs lose
/// coalescing where the row-only guard used to allow it — the one intended
/// output change of #449. To restore it, raise `--coalesce-max-level-rows`
/// (it scales both limbs: 4,000,000 allows 4M lines / 2.048 GB modelled), at
/// the cost of that much more resident geometry plus the chain-stage peak
/// on top (`docs/OVERVIEW_TUNING.md`, "Line coalescing").
///
/// (The synthetic 37-vertex lines of #569's germany-segments benchmark
/// proxy — 648 B modelled — would trip the byte limb from ~1.58M lines up
/// to the 2M row ceiling; at the proxy's 6M lines the row limb trips first
/// either way. The proxy is not a typical road segment.)
pub(crate) const COALESCE_NOMINAL_BYTES_PER_LINE: u64 = 512;

/// Modelled bytes of one line's `Geometry<f64>` slot in the coalescing
/// scratch vector.
///
/// **Deliberately a fixed constant, not `size_of::<Geometry<f64>>()`**: the
/// guard's verdict decides whether the output is coalesced, so the model must
/// not move when a `geo` upgrade changes the enum's layout — the same input
/// and options must produce the same file across tylertoo versions. 56 is the
/// layout at the time of #449 (the `Polygon` variant, two `Vec`s + tag);
/// `geometry_slot_model_covers_the_real_slot` fails if a future layout grows
/// past it, which is the cue to revisit (not silently re-derive) the model.
pub(crate) const COALESCE_MODEL_SLOT_BYTES: u64 = 56;

/// Modelled bytes per coordinate (`Coord<f64>`: two `f64`s). Fixed for the
/// same reason as [`COALESCE_MODEL_SLOT_BYTES`].
pub(crate) const COALESCE_MODEL_COORD_BYTES: u64 = 16;

/// Retained footprint of one line geometry held in the pass-1 coalescing
/// scratch: its `Geometry<f64>` slot in the scratch vector plus the heap
/// coordinate run behind it —
/// [`COALESCE_MODEL_SLOT_BYTES`] + [`COALESCE_MODEL_COORD_BYTES`] × vertices.
///
/// Deliberately a *model*, not a measurement: it must be a pure function of
/// the geometry so the [`super::convert::coalesce_effective`] verdict is
/// identical on every platform, allocator, batch size and `geo` version (the
/// streaming and buffered engines are byte-identical by contract, and both
/// evaluate it). It undercounts the true resident cost — per-allocation
/// malloc headers, `Vec` growth slack, and the parallel
/// `rows`/`sort_keys`/`groups` side vectors (~100–120 B per line all told) —
/// which the row limb of the ceiling covers.
pub(crate) fn collected_line_bytes(g: &Geometry<f64>) -> u64 {
    use geo::coords_iter::CoordsIter;
    COALESCE_MODEL_SLOT_BYTES + (g.coords_count() as u64) * COALESCE_MODEL_COORD_BYTES
}

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

/// Per-level chaining parameters for [`coalesce_level_lines`].
#[derive(Debug, Clone, Copy)]
pub struct CoalesceParams {
    /// Endpoint snap tolerance in GSD multiples. `<= 0` means exact
    /// coordinate matching only.
    pub snap_gsd_factor: f64,
    /// Junction continuation threshold in degrees
    /// ([`DEFAULT_JUNCTION_ANGLE_DEG`]): at nodes of degree >= 3, incident
    /// pairs that continue each other within this deviation from straight
    /// are joined (best pair first, repeatedly — a 4-way crossing continues
    /// both through-streets). `<= 0` restores strict degree-2-only chaining.
    pub junction_angle_deg: f64,
    /// OPTIONAL per-level `(max_chains, gamma)` density budget (Q2 applied
    /// to CHAINS): when more chains than `max_chains` survive the gate +
    /// thinning, the lowest-priority ones are dropped with the same
    /// super-cell spatial fairness the point/polygon budget uses. Without
    /// this, coalescing would bypass the Q2 budget entirely and re-inflate
    /// the mid-zoom counts the budget was calibrated to cap.
    pub budget: Option<(usize, f64)>,
}

impl Default for CoalesceParams {
    fn default() -> Self {
        Self {
            snap_gsd_factor: DEFAULT_SNAP_GSD_FACTOR,
            junction_angle_deg: DEFAULT_JUNCTION_ANGLE_DEG,
            budget: None,
        }
    }
}

/// Coalesce one level's candidate lines: chain within compatibility groups
/// through endpoint nodes (degree-2 always; junction continuation per
/// [`CoalesceParams::junction_angle_deg`]), then apply the visibility gate,
/// cell-winner thinning, and the optional chain budget. Returns the
/// surviving chains ordered by ascending `rep` index.
///
/// - `gsd_m`: the level's GSD in meters (governs snap tolerance, gate, and
///   thinning grid). Non-positive GSD returns every input as an ungated
///   singleton (degenerate; callers pass canonical levels elsewhere).
/// - `config`: the assignment configuration; `line_visibility`,
///   `line_thinning`, and `sort_direction` are used.
pub fn coalesce_level_lines(
    lines: &[CoalesceInput<'_>],
    gsd_m: f64,
    crs: Crs,
    config: &AssignConfig,
    params: &CoalesceParams,
) -> Vec<CoalescedLine> {
    if lines.is_empty() {
        return Vec::new();
    }
    let budget = params.budget;
    let gsd_units = crs.meters_to_units(gsd_m);
    let snap_tol = if gsd_units > 0.0 {
        params.snap_gsd_factor * gsd_units
    } else {
        0.0
    };

    // --- 1+2. Build chains (grouped endpoint joins + junction continuation).
    let chains = build_chains(lines, snap_tol, params.junction_angle_deg);

    // --- Priorities of the members (the cell-winner order, Q1). -------------
    // Position-indexed, matching `RawChain::members`: two `HashMap`s with one
    // entry per input line cost ~100 MiB at 1.5M lines, per level, in
    // parallel across levels (#570). Sort keys are read straight off `lines`.
    let prio: Vec<Priority> = lines
        .iter()
        .map(|l| {
            let feat = AssignFeature {
                index: l.index,
                bbox: geom_bbox(l.geom),
                kind: FeatureKind::Line,
                sort_key: l.sort_key,
                entry_level: None,
            };
            Priority::new(&feat, config.sort_direction)
        })
        .collect();

    // --- 3. Merge geometry + inherit the best member's attributes. ----------
    // `rep_pos` (the winning member's position in `lines`) rides along so the
    // gate can read its sort key without a second lookup table.
    let mut merged: Vec<(CoalescedLine, usize)> = Vec::with_capacity(chains.len());
    for chain in chains {
        let rep_pos = chain
            .members
            .iter()
            .copied()
            .reduce(|best, m| if prio[m].beats(&prio[best]) { m } else { best })
            .expect("chain has at least one member");
        merged.push((
            CoalescedLine {
                rep: lines[rep_pos].index,
                count: chain.members.len() as i32,
                geom: chain.geom,
            },
            rep_pos,
        ));
    }

    // --- 4a. Visibility gate on the CHAIN's bbox diagonal. -------------------
    let gate = config.line_visibility * gsd_units;
    let gate_sq = gate * gate;
    let mut gated: Vec<(CoalescedLine, AssignFeature)> = Vec::with_capacity(merged.len());
    for (line, rep_pos) in merged {
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
            sort_key: lines[rep_pos].sort_key,
            entry_level: None,
        };
        gated.push((line, feat));
    }

    // --- 4b. Cell-winner thinning among the surviving chains. ----------------
    let cell_size = gsd_units * config.line_thinning;
    let mut survivors: Vec<(CoalescedLine, AssignFeature)> =
        if cell_size > 0.0 && !cell_size.is_nan() {
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
            // Keep winners in ascending position order (stable, deterministic;
            // O(n) — a per-winner `Vec::remove` here is quadratic on real data).
            let mut keep = vec![false; gated.len()];
            for pos in grid.into_values() {
                keep[pos] = true;
            }
            gated
                .into_iter()
                .zip(&keep)
                .filter(|(_, &k)| k)
                .map(|(pair, _)| pair)
                .collect()
        } else {
            gated
        };

    // --- 4c. Per-level density budget on chains (Q2 analog). -----------------
    if let Some((max_chains, gamma)) = budget {
        if survivors.len() > max_chains {
            let feats: Vec<AssignFeature> = survivors
                .iter()
                .enumerate()
                .map(|(pos, (_, f))| AssignFeature { index: pos, ..*f })
                .collect();
            // No priority table: `select_budget_survivors` derives each
            // candidate's from `feats[pos]` (#565). `feats` is built with
            // `index: pos` precisely so the derived priority — hash and index
            // tie-break included — is the one the removed table held.
            let cands: Vec<usize> = (0..survivors.len()).collect();
            let mut chosen = super::assign::select_budget_survivors(
                &cands,
                max_chains,
                &feats,
                config.sort_direction,
                gsd_m,
                crs,
                gamma,
            );
            chosen.sort_unstable();
            let mut keep = vec![false; survivors.len()];
            for pos in chosen {
                keep[pos] = true;
            }
            survivors = survivors
                .into_iter()
                .zip(&keep)
                .filter(|(_, &k)| k)
                .map(|(pair, _)| pair)
                .collect();
        }
    }

    let mut survivors: Vec<CoalescedLine> = survivors.into_iter().map(|(l, _)| l).collect();
    survivors.sort_by_key(|c| c.rep);
    survivors
}

/// Unit direction of `p` at its `end`, pointing INTO the line away from
/// the endpoint (toward the first distinct vertex). `None` when every
/// vertex coincides (degenerate).
fn piece_direction(p: &Piece, end: End) -> Option<(f64, f64)> {
    let c = &p.coords;
    let (anchor, inward) = if end == 0 {
        (c[0], c.iter().skip(1).find(|&&q| q != c[0]).copied())
    } else {
        let last = c[c.len() - 1];
        (last, c.iter().rev().skip(1).find(|&&q| q != last).copied())
    };
    let q = inward?;
    let (dx, dy) = (q.x - anchor.x, q.y - anchor.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len > 0.0 {
        Some((dx / len, dy / len))
    } else {
        None
    }
}

/// Angular deviation (degrees) of continuing from a line with inward
/// direction `a` onto a line with inward direction `b` at a shared node:
/// `0°` = perfectly straight (the directions are opposite), `90°` = a
/// right-angle turn.
fn continuation_deviation_deg(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dot = (a.0 * b.0 + a.1 * b.1).clamp(-1.0, 1.0);
    180.0 - dot.acos().to_degrees()
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
///
/// Members are POSITIONS in the caller's `lines` slice, not
/// [`CoalesceInput::index`] values: the caller's priority and sort-key
/// tables are then plain position-indexed `Vec`s instead of two
/// `HashMap<usize, _>`s with one entry per input line (#570).
struct RawChain {
    members: Vec<usize>,
    geom: Geometry<f64>,
}

/// A joinable polyline piece during chaining: one or more already-merged
/// source segments with an oriented coordinate run.
///
/// The run is a [`Cow`]: an unmerged piece BORROWS its source line's
/// coordinates, so phase 1 no longer copies the whole input coordinate set
/// before a single join happens (#570). Assembling a multi-piece walk
/// allocates the merged run once, exactly as before.
struct Piece<'a> {
    /// Positions in the caller's `lines` slice, merged so far.
    members: Vec<usize>,
    coords: Cow<'a, [geo::Coord<f64>]>,
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
/// 2. **Snap pass**: re-run the same join over the *resulting chains'*
///    free endpoints, quantized to `snap_tol` grid cells — closing small
///    digitization gaps at the level's resolution. Skipped when
///    `snap_tol <= 0` (exact matching only).
///
/// Both phases join degree-2 nodes unconditionally and, when
/// `junction_angle_deg > 0`, continue through junctions (degree >= 3) by
/// repeatedly pairing the incident lines that best continue each other
/// within that angular deviation (stroke building).
fn build_chains<'a>(
    lines: &[CoalesceInput<'a>],
    snap_tol: f64,
    junction_angle_deg: f64,
) -> Vec<RawChain> {
    // Chainable = plain LineString with >= 2 coordinates. Everything else
    // (MultiLineString, Line, degenerate) is an unmerged singleton.
    let mut pieces: Vec<Piece<'a>> = Vec::new();
    let mut singles: Vec<usize> = Vec::new(); // positions in `lines`
    for (pos, l) in lines.iter().enumerate() {
        match l.geom {
            Geometry::LineString(ls) if ls.0.len() >= 2 => pieces.push(Piece {
                members: vec![pos],
                coords: Cow::Borrowed(&ls.0[..]),
                group: l.group,
            }),
            _ => singles.push(pos),
        }
    }

    // Phase 1: exact endpoint matching; phase 2: snapped matching.
    let pieces = join_pieces(pieces, exact_key, junction_angle_deg);
    let pieces = if snap_tol > 0.0 {
        join_pieces(pieces, |g, c| snap_key(g, c, snap_tol), junction_angle_deg)
    } else {
        pieces
    };

    let mut chains: Vec<RawChain> = pieces
        .into_iter()
        .map(|p| RawChain {
            geom: Geometry::LineString(LineString::new(p.coords.into_owned())),
            members: p.members,
        })
        .collect();

    // Non-chainable singletons pass through unmerged (original geometry).
    for pos in singles {
        chains.push(RawChain {
            members: vec![pos],
            geom: lines[pos].geom.clone(),
        });
    }
    chains
}

/// One round of endpoint joining over `pieces`, with node keys produced by
/// `key(group, endpoint)`.
///
/// - Nodes with exactly two incident endpoints from two DISTINCT pieces
///   join unconditionally (a degree-2 node is an unambiguous continuation).
/// - Junctions (degree >= 3): when `junction_angle_deg > 0`, incident pairs
///   from distinct pieces whose directions continue each other within that
///   deviation from straight are joined best-pair-first (so a 4-way
///   crossing continues BOTH through-streets); remaining incidents
///   terminate. `<= 0` restores strict degree-2-only behavior.
/// - Self-loops never join.
///
/// Deterministic: components are walked in ascending piece index, starting
/// from a free end (or the lowest-index piece of a cycle); junction pairs
/// are ordered by (deviation, incident order).
fn join_pieces<'a>(
    pieces: Vec<Piece<'a>>,
    key: impl Fn(u32, geo::Coord<f64>) -> NodeKey,
    junction_angle_deg: f64,
) -> Vec<Piece<'a>> {
    // Endpoint incidences, grouped into per-node runs by sorting rather than
    // by a `HashMap<NodeKey, Vec<_>>` — which cost one heap `Vec` per node,
    // up to 2N of them (#570).
    //
    // Identical grouping AND identical per-node incident order: the old map
    // was filled in ascending piece index, pushing end 0 before end 1, so
    // every node's list was ordered `(piece asc, end asc)`. Packing the
    // incident as `piece << 1 | end` makes the sort tiebreaker exactly that
    // order, and each `(piece, end)` belongs to exactly one node, so the
    // runs partition the same way. Node processing only reads `pieces` and
    // writes this node's own `joins` slots, so the order the runs are
    // visited in cannot affect the result either (as before, when it was
    // hash order).
    debug_assert!(pieces.len() <= usize::MAX >> 1);
    let mut incidents: Vec<(NodeKey, usize)> = Vec::with_capacity(pieces.len() * 2);
    for (pi, p) in pieces.iter().enumerate() {
        let first = p.coords[0];
        let last = p.coords[p.coords.len() - 1];
        incidents.push((key(p.group, first), pi << 1));
        incidents.push((key(p.group, last), (pi << 1) | 1));
    }
    incidents.sort_unstable();

    // Joins: per piece, the (other piece, other end) connected at each end.
    let mut joins: Vec<[Option<(usize, End)>; 2]> = vec![[None, None]; pieces.len()];
    let mut run_start = 0usize;
    while run_start < incidents.len() {
        let mut run_end = run_start + 1;
        while run_end < incidents.len() && incidents[run_end].0 == incidents[run_start].0 {
            run_end += 1;
        }
        let run = &incidents[run_start..run_end];
        run_start = run_end;

        // `(piece, end)` of the run's k-th incident, in the old map's order.
        let inc = |k: usize| -> (usize, End) { (run[k].1 >> 1, (run[k].1 & 1) as End) };
        let d = run.len();
        if d == 2 && inc(0).0 != inc(1).0 {
            let (a, a_end) = inc(0);
            let (b, b_end) = inc(1);
            joins[a][a_end as usize] = Some((b, b_end));
            joins[b][b_end as usize] = Some((a, a_end));
        } else if d >= 3 && junction_angle_deg > 0.0 {
            // Junction continuation (stroke building): pair up incidents
            // that continue each other within the angular threshold,
            // straightest pair first. Each node's pairing is independent of
            // every other node's (an end belongs to exactly one node), so
            // the order the nodes are visited in cannot affect the result.
            let dirs: Vec<Option<(f64, f64)>> = (0..d)
                .map(|k| {
                    let (pi, e) = inc(k);
                    piece_direction(&pieces[pi], e)
                })
                .collect();
            let mut cands: Vec<(f64, usize, usize)> = Vec::new();
            for i in 0..d {
                for j in (i + 1)..d {
                    if inc(i).0 == inc(j).0 {
                        continue; // self-loops never join
                    }
                    if let (Some(a), Some(b)) = (dirs[i], dirs[j]) {
                        let dev = continuation_deviation_deg(a, b);
                        if dev <= junction_angle_deg {
                            cands.push((dev, i, j));
                        }
                    }
                }
            }
            cands.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let mut used = vec![false; d];
            for (_, i, j) in cands {
                if used[i] || used[j] {
                    continue;
                }
                used[i] = true;
                used[j] = true;
                let (a, a_end) = inc(i);
                let (b, b_end) = inc(j);
                joins[a][a_end as usize] = Some((b, b_end));
                joins[b][b_end as usize] = Some((a, a_end));
            }
        }
    }
    drop(incidents);

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
    let mut slots: Vec<Option<Piece<'a>>> = pieces.into_iter().map(Some).collect();
    let mut out: Vec<Piece<'a>> = Vec::with_capacity(order.len());
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
            // Reversing in place and then appending forward is the same
            // sequence as appending in reverse — done this way so a
            // borrowed run never has to be cloned just to be flipped.
            append_run(&mut coords, &p.coords, seg_entry == 1);
        }
        out.push(Piece {
            members,
            coords: Cow::Owned(coords),
            group,
        });
    }
    out
}

/// Append `src` (reversed when `flip`) to `out`, dropping a vertex that
/// exactly repeats `out`'s last one (the shared node between two joined
/// pieces).
fn append_run(out: &mut Vec<geo::Coord<f64>>, src: &[geo::Coord<f64>], flip: bool) {
    let mut push = |c: geo::Coord<f64>| {
        if out.last() != Some(&c) {
            out.push(c);
        }
    };
    if flip {
        for &c in src.iter().rev() {
            push(c);
        }
    } else {
        for &c in src {
            push(c);
        }
    }
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

    fn params(snap: f64, junction: f64, budget: Option<(usize, f64)>) -> CoalesceParams {
        CoalesceParams {
            snap_gsd_factor: snap,
            junction_angle_deg: junction,
            budget,
        }
    }

    fn run<'a>(lines: &[CoalesceInput<'a>], gsd_m: f64) -> Vec<CoalescedLine> {
        coalesce_level_lines(
            lines,
            gsd_m,
            Crs::Epsg3857,
            &cfg(),
            &CoalesceParams {
                junction_angle_deg: 0.0, // strict degree-2 (junction tests are explicit)
                ..CoalesceParams::default()
            },
        )
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

    // --- junction continuation (stroke building) ------------------------------

    /// T-junction with the junction knob on: the collinear pair chains
    /// straight through; the perpendicular branch stays separate.
    #[test]
    fn junction_continuation_merges_straight_pair_at_t() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]);
        let c = ls(&[(100.0, 0.0), (100.0, 100.0)]);
        let lines = [input(0, &a), input(1, &b), input(2, &c)];
        let out = coalesce_level_lines(
            &lines,
            TINY_GSD,
            Crs::Epsg3857,
            &cfg(),
            &params(1.0, 30.0, None),
        );
        assert_eq!(
            out.len(),
            2,
            "straight pair merges, branch survives: {out:?}"
        );
        let merged = out.iter().find(|l| l.count == 2).expect("merged pair");
        assert_eq!(coords_of(&merged.geom).len(), 3);
        assert_eq!(out.iter().find(|l| l.count == 1).map(|l| l.rep), Some(2));
    }

    /// Four-way crossing: BOTH through-streets continue (two pairs merge).
    /// The crossing is asymmetric so the two merged chains land in
    /// different thinning cells (equal bbox centers would thin to one).
    #[test]
    fn junction_continuation_pairs_both_streets_at_crossing() {
        let e = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let w = ls(&[(100.0, 0.0), (300.0, 0.0)]);
        let n = ls(&[(100.0, 0.0), (100.0, 100.0)]);
        let s = ls(&[(100.0, -300.0), (100.0, 0.0)]);
        let lines = [input(0, &e), input(1, &w), input(2, &n), input(3, &s)];
        let out = coalesce_level_lines(
            &lines,
            TINY_GSD,
            Crs::Epsg3857,
            &cfg(),
            &params(1.0, 30.0, None),
        );
        assert_eq!(out.len(), 2, "both through-streets continue: {out:?}");
        assert!(out.iter().all(|l| l.count == 2));
    }

    /// A bend sharper than the threshold does not merge through a junction.
    #[test]
    fn junction_continuation_respects_angle_threshold() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let bent = ls(&[(100.0, 0.0), (170.0, 70.0)]); // 45° off straight
        let branch = ls(&[(100.0, 0.0), (100.0, 100.0)]); // 90° off
        let lines = [input(0, &a), input(1, &bent), input(2, &branch)];
        // 30° threshold: nothing continues (45° and 90° both exceed it).
        let strict = coalesce_level_lines(
            &lines,
            TINY_GSD,
            Crs::Epsg3857,
            &cfg(),
            &params(1.0, 30.0, None),
        );
        assert_eq!(strict.len(), 3, "45° bend exceeds 30°: {strict:?}");
        // 60° threshold: the 45° pair merges; the 90° branch still doesn't.
        let loose = coalesce_level_lines(
            &lines,
            TINY_GSD,
            Crs::Epsg3857,
            &cfg(),
            &params(1.0, 60.0, None),
        );
        assert_eq!(loose.len(), 2, "45° bend within 60°: {loose:?}");
        assert_eq!(loose.iter().map(|l| l.count).max(), Some(2));
    }

    /// Junction continuation still never crosses class groups.
    #[test]
    fn junction_continuation_respects_groups() {
        let a = ls(&[(0.0, 0.0), (100.0, 0.0)]);
        let b = ls(&[(100.0, 0.0), (200.0, 0.0)]); // straight, WRONG class
        let c = ls(&[(100.0, 0.0), (100.0, 100.0)]); // same class, 90° off
        let mut ia = input(0, &a);
        let mut ib = input(1, &b);
        let mut ic = input(2, &c);
        ia.group = 1;
        ib.group = 2;
        ic.group = 1;
        let out = coalesce_level_lines(
            &[ia, ib, ic],
            TINY_GSD,
            Crs::Epsg3857,
            &cfg(),
            &params(1.0, 30.0, None),
        );
        // Within group 1 the node is degree 2 (a + c) — but they meet at
        // 90°... degree-2 joins are unconditional, so a + c chain; b stays.
        assert_eq!(out.len(), 2, "no cross-group merge: {out:?}");
        let merged = out.iter().find(|l| l.count == 2).expect("a+c chain");
        assert!(merged.rep == 0 || merged.rep == 2);
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
        let out = coalesce_level_lines(&lines, 1.0, Crs::Epsg3857, &cfg(), &params(0.0, 0.0, None));
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
        let out =
            coalesce_level_lines(&lines, 1000.0, Crs::Epsg3857, &cfg, &params(1.0, 0.0, None));
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
        let out =
            coalesce_level_lines(&lines, 1000.0, Crs::Epsg3857, &cfg, &params(1.0, 0.0, None));
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
    fn chain_budget_caps_survivors_by_priority() {
        // Three disjoint chains in separate thinning cells; a budget of 2
        // keeps the two highest-priority (longest) ones.
        let long = ls(&[(0.0, 0.0), (5000.0, 0.0)]);
        let mid = ls(&[(0.0, 20_000.0), (3000.0, 20_000.0)]);
        let short = ls(&[(0.0, 40_000.0), (1000.0, 40_000.0)]);
        let lines = [input(0, &long), input(1, &mid), input(2, &short)];
        let cfg = AssignConfig {
            line_visibility: 0.1,
            ..AssignConfig::default()
        };
        let all =
            coalesce_level_lines(&lines, 1000.0, Crs::Epsg3857, &cfg, &params(1.0, 0.0, None));
        assert_eq!(all.len(), 3, "no budget keeps all: {all:?}");
        let cut = coalesce_level_lines(
            &lines,
            1000.0,
            Crs::Epsg3857,
            &cfg,
            &params(1.0, 0.0, Some((2, 1.0))),
        );
        assert_eq!(cut.len(), 2, "budget of 2 binds: {cut:?}");
        let reps: Vec<usize> = cut.iter().map(|c| c.rep).collect();
        assert_eq!(reps, vec![0, 1], "lowest-priority (shortest) chain drops");
    }

    #[test]
    fn empty_input_is_noop() {
        assert!(run(&[], 10.0).is_empty());
    }

    // --- #449 memory accounting --------------------------------------------

    #[test]
    fn collected_line_bytes_counts_slot_plus_coordinates() {
        let slot = 56;
        let two = ls(&[(0.0, 0.0), (1.0, 0.0)]);
        assert_eq!(collected_line_bytes(&two), slot + 2 * 16);
        let five = ls(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0), (4.0, 0.0)]);
        assert_eq!(collected_line_bytes(&five), slot + 5 * 16);
        // MultiLineStrings are collected too (they pass through as
        // singletons), and every part's coordinates are retained.
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::from(vec![(0.0, 0.0), (1.0, 0.0)]),
            LineString::from(vec![(3.0, 0.0), (4.0, 0.0), (5.0, 0.0)]),
        ]));
        assert_eq!(collected_line_bytes(&mls), slot + 5 * 16);
    }

    /// The byte model is pinned to fixed constants (so the coalescing verdict
    /// cannot drift across `geo` versions), but it must still cover the real
    /// layout: if `Geometry<f64>` or `Coord<f64>` ever grows past the model,
    /// revisit `COALESCE_MODEL_*_BYTES` deliberately — changing them changes
    /// which inputs get coalesced.
    #[test]
    fn geometry_slot_model_covers_the_real_slot() {
        assert!(
            std::mem::size_of::<Geometry<f64>>() as u64 <= COALESCE_MODEL_SLOT_BYTES,
            "Geometry<f64> is {} B, over the fixed {COALESCE_MODEL_SLOT_BYTES} B model",
            std::mem::size_of::<Geometry<f64>>()
        );
        assert_eq!(
            std::mem::size_of::<geo::Coord<f64>>() as u64,
            COALESCE_MODEL_COORD_BYTES
        );
        // 512 B/line is the byte limb's per-line budget: 56 + 16 × 28.5 —
        // the "~28 vertices/line" threshold the docs quote.
        let per_line = |v: u64| COALESCE_MODEL_SLOT_BYTES + v * COALESCE_MODEL_COORD_BYTES;
        assert!(per_line(28) < COALESCE_NOMINAL_BYTES_PER_LINE);
        assert!(per_line(29) > COALESCE_NOMINAL_BYTES_PER_LINE);
    }

    /// The model must be monotone in coordinate count — the property the
    /// streaming ceiling relies on to stop collecting at the same line on
    /// every run.
    #[test]
    fn collected_line_bytes_is_monotone_in_vertices() {
        let mut prev = 0;
        for n in 2..12 {
            let coords: Vec<(f64, f64)> = (0..n).map(|i| (i as f64, 0.0)).collect();
            let bytes = collected_line_bytes(&ls(&coords));
            assert!(bytes > prev, "{n} vertices: {bytes} !> {prev}");
            prev = bytes;
        }
    }

    #[test]
    fn degenerate_single_point_line_is_gated_out() {
        let dot = Geometry::LineString(LineString::new(vec![Coord { x: 1.0, y: 1.0 }]));
        let lines = [input(0, &dot)];
        let out = run(&lines, 10.0);
        assert!(out.is_empty(), "zero-diagonal chain fails the gate");
    }
}

/// Golden differential for the chain builder (#570 / #579 review).
///
/// #570 rewrote the chain builder's internals (borrowed coordinate runs, a
/// sorted incidence list instead of a node `HashMap`, position-indexed
/// priorities) with the claim that output is unchanged. This pins that claim:
/// 1500 seeded hostile cases (shared endpoints of degree 2 to 4+, duplicate
/// and reversed-duplicate lines, zero-length and one-vertex lines, closed
/// self-touching rings, repeated vertices, MultiLineStrings, antimeridian
/// coordinates, NaN/inf/-0.0, mixed groups and sort keys, shuffled indices,
/// every junction/snap/budget/gate setting) digest to the value the
/// pre-#570 builder produced. The same generator was also run for 40,000
/// cases (up to 20,000 lines each) against the pre-#570 code directly, with
/// identical results. A change to this digest is an output change.
#[cfg(test)]
mod golden_digest {
    use super::*;
    use geo::{Coord, MultiLineString};

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn f(&mut self) -> f64 {
            (self.next() as f64) / ((1u64 << 31) as f64)
        }
    }

    fn pt(r: &mut Lcg, grid: u64, scale: f64, mode: u64) -> Coord<f64> {
        let base = Coord {
            x: (r.below(grid) as f64) * scale,
            y: (r.below(grid) as f64) * scale,
        };
        match mode {
            // antimeridian: x near +180 or -180
            1 => Coord {
                x: if r.below(2) == 0 {
                    179.9 + base.x * 1e-3
                } else {
                    -179.9 - base.x * 1e-3
                },
                y: base.y * 1e-3,
            },
            // jittered: exercises the snap phase
            2 => Coord {
                x: base.x + (r.f() - 0.5) * scale * 0.3,
                y: base.y + (r.f() - 0.5) * scale * 0.3,
            },
            // non-finite sprinkled in
            3 if r.below(30) == 0 => Coord {
                x: [f64::NAN, f64::INFINITY, -0.0][r.below(3) as usize],
                y: base.y,
            },
            _ => base,
        }
    }

    fn gen_geoms(r: &mut Lcg, n: usize, scale: f64, mode: u64) -> Vec<Geometry<f64>> {
        let grid = 2 + r.below(12);
        let mut out: Vec<Geometry<f64>> = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.below(20);
            let g = match k {
                0 => {
                    let p = pt(r, grid, scale, mode);
                    Geometry::LineString(LineString::new(vec![p, p]))
                }
                1 if !out.is_empty() => out[r.below(out.len() as u64) as usize].clone(),
                2 if !out.is_empty() => match &out[r.below(out.len() as u64) as usize] {
                    Geometry::LineString(ls) => {
                        let mut c = ls.0.clone();
                        c.reverse();
                        Geometry::LineString(LineString::new(c))
                    }
                    g => g.clone(),
                },
                3 => {
                    let a = pt(r, grid, scale, mode);
                    let b = pt(r, grid, scale, mode);
                    let c = pt(r, grid, scale, mode);
                    Geometry::LineString(LineString::new(vec![a, b, c, a]))
                }
                4 => Geometry::MultiLineString(MultiLineString::new(vec![LineString::new(vec![
                    pt(r, grid, scale, mode),
                    pt(r, grid, scale, mode),
                ])])),
                5 => Geometry::LineString(LineString::new(vec![pt(r, grid, scale, mode)])),
                6 => {
                    let a = pt(r, grid, scale, mode);
                    let b = pt(r, grid, scale, mode);
                    let c = pt(r, grid, scale, mode);
                    Geometry::LineString(LineString::new(vec![a, a, b, b, c, c]))
                }
                7 => Geometry::LineString(LineString::new(vec![])),
                _ => {
                    let nv = 2 + r.below(4) as usize;
                    let mut v = vec![pt(r, grid, scale, mode)];
                    for _ in 1..nv - 1 {
                        v.push(Coord {
                            x: v[0].x + r.f() * scale,
                            y: v[0].y + r.f() * scale,
                        });
                    }
                    v.push(pt(r, grid, scale, mode));
                    Geometry::LineString(LineString::new(v))
                }
            };
            out.push(g);
        }
        out
    }

    /// One seeded case: returns the coalesced output and how many chains
    /// merged two or more lines.
    fn run_case(seed: u64) -> (Vec<CoalescedLine>, usize) {
        let mut r = Lcg(seed ^ 0x9E37_79B9_7F4A_7C15);
        let big = seed.is_multiple_of(97);
        let n = 1 + r.below(if big { 2000 } else { 150 }) as usize;
        let mode = r.below(4);
        let scale = [1.0, 10.0, 1000.0, 0.001][r.below(4) as usize];
        let geoms = gen_geoms(&mut r, n, scale, mode);
        let ngroups = 1 + r.below(3) as u32;
        let mut idx: Vec<usize> = (0..n).map(|i| i * 3 + 7).collect();
        if r.below(2) == 0 {
            for i in (1..n).rev() {
                let j = r.below(i as u64 + 1) as usize;
                idx.swap(i, j);
            }
        }
        let sks: Vec<Option<f64>> = (0..n)
            .map(|_| match r.below(4) {
                0 => None,
                1 => Some(1.0),
                _ => Some(r.below(5) as f64),
            })
            .collect();
        let groups: Vec<u32> = (0..n).map(|_| r.below(ngroups as u64) as u32).collect();
        let lines: Vec<CoalesceInput<'_>> = (0..n)
            .map(|i| CoalesceInput {
                index: idx[i],
                geom: &geoms[i],
                sort_key: sks[i],
                group: groups[i],
            })
            .collect();
        let snap = [0.0, 0.5, 1.0, 3.0][r.below(4) as usize];
        let junction = [0.0, 0.0, 20.0, 45.0, 90.0, 180.0][r.below(6) as usize];
        let budget = if r.below(3) == 0 {
            Some((1 + r.below(20) as usize, 0.5 + r.f()))
        } else {
            None
        };
        let gsd = [0.0, 1e-6, 0.5, 5.0, 100.0, 5000.0][r.below(6) as usize];
        let crs = if mode == 1 || r.below(2) == 0 {
            Crs::Epsg4326
        } else {
            Crs::Epsg3857
        };
        let mut cfg = AssignConfig::default();
        if r.below(3) == 0 {
            cfg.line_visibility = 0.0;
        }
        if r.below(3) == 0 {
            cfg.line_thinning = 0.0;
        }
        let params = CoalesceParams {
            snap_gsd_factor: snap,
            junction_angle_deg: junction,
            budget,
        };
        let out = coalesce_level_lines(&lines, gsd, crs, &cfg, &params);
        let merged = out.iter().filter(|c| c.count > 1).count();
        (out, merged)
    }

    /// FNV-1a over every rep, count and coordinate bit pattern.
    fn digest(out: &[CoalescedLine]) -> u64 {
        use geo::coords_iter::CoordsIter;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        eat(out.len() as u64);
        for c in out {
            eat(c.rep as u64);
            eat(c.count as u64);
            eat(c.geom.coords_count() as u64);
            for p in c.geom.coords_iter() {
                eat(p.x.to_bits());
                eat(p.y.to_bits());
            }
        }
        h
    }

    #[test]
    fn hostile_cases_match_the_pre_570_chain_builder() {
        let mut h: u64 = 0;
        let mut merged = 0;
        for seed in 0..1500 {
            let (out, m) = run_case(seed);
            merged += m;
            h = h.rotate_left(7) ^ digest(&out);
        }
        assert_eq!(merged, 8349, "the generator must keep exercising joins");
        assert_eq!(
            h, 0x98cb_5976_7524_9af4,
            "chain builder output changed against the pre-#570 golden digest"
        );
    }
}
