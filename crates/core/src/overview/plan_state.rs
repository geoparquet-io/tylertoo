//! The **convert plan artifact** (`--save-plan` / `--plan`): the complete
//! pass-1 + assignment result, persisted so it never has to be recomputed.
//!
//! # Why this exists
//!
//! ## 1. Resume
//!
//! Pass 1 streams the whole input to build one byte per row (the winner
//! table) plus the assignment side tables; on a planet-scale input that is
//! the majority of a convert's wall time. Saving that result lets a re-run
//! with different *write-side* knobs (row-group sizing, profile, in-flight
//! depth, compression) skip pass 1 and the assignment entirely.
//!
//! ## 2. Sharding consistency — the load-bearing reason
//!
//! The assignment is **not** a per-feature function. It threads dataset-wide
//! fold state:
//!
//! - [`assign_levels_bounded`] walks levels coarse → fine carrying a running
//!   `kept_count` across levels;
//! - [`apply_density_budget`] water-fills a 128 × GSD super-cell budget over
//!   **all** candidates of a level (see `assign.rs`);
//! - the entry-zoom ladder dense-ranks the **global** distinct values of its
//!   column (`ladder.rs`);
//! - the Q1 ranking auto-detection picks its column from a **global**
//!   vocabulary scan.
//!
//! A sharded build therefore cannot recompute the assignment locally: each
//! shard would fold over its own subset and reach a different answer, and the
//! shards' pyramids would disagree. One global assignment must be computed
//! once and handed to every shard — that artifact is this file.
//!
//! # Format
//!
//! One Arrow IPC **file** holding a single record batch whose every column is
//! a `LargeList` carrying one whole side table as a single list value (`null`
//! when the section does not apply). Scalars, the fingerprint, and the
//! provenance blocks travel as JSON under the schema metadata key
//! [`PLAN_META_KEY`]; [`PLAN_FORMAT_VERSION`] versions both.
//!
//! Sections are lists rather than one batch per table because an IPC file
//! carries exactly one schema: a batch per table would force every column to
//! exist in every batch, and an all-null `Int64` column still costs 8 bytes
//! per row on disk.
//!
//! # Fingerprint
//!
//! Loading a plan whose fingerprint does not match the current run is a hard
//! error naming the offending field — never a silent stale run. The
//! fingerprint pins the tylertoo version, every thinning-relevant option, and
//! each input part's identity (path, byte length, mtime, selected row
//! groups). See [`Fingerprint`].
//!
//! # Known gap (follow-up)
//!
//! The entry-zoom ladder's *derived* rungs are not stored verbatim: they are
//! computed inside pass 1 from the global column scan. They are fully baked
//! into `min_levels` — which is what a shard consumes — and their inputs (the
//! spec, and the input parts the values come from) are both fingerprinted, so
//! a stale ladder cannot slip through. Capturing the resolved rungs for human
//! provenance needs a pass-1 signature change and is deliberately left out of
//! this change.
//!
//! [`assign_levels_bounded`]: super::assign::assign_levels_bounded
//! [`apply_density_budget`]: super::assign::apply_density_budget

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, Float64Builder, Int64Builder, LargeListBuilder, UInt32Builder, UInt64Builder,
    UInt8Builder,
};
use arrow_array::{
    Array, BinaryArray, Float64Array, Int64Array, LargeListArray, RecordBatch, UInt32Array,
    UInt64Array, UInt8Array,
};
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::FileWriter;
use arrow_schema::{DataType, Field, Schema};
use serde::{Deserialize, Serialize};

use crate::input_set::{ConvertSource, RowGroupSelection};

use super::assign::FeatureKind;
use super::cluster::{ClusterEntry, ClusterTables};
use super::convert::{ConvertError, ConvertOptions};
use super::level::RankingProvenance;

/// Schema-metadata key under which the plan's JSON scalars travel.
pub(super) const PLAN_META_KEY: &str = "tylertoo:convert_plan";

/// On-disk format version of the plan artifact. Bump on any incompatible
/// change to the section layout or the JSON block.
pub(super) const PLAN_FORMAT_VERSION: u32 = 1;

/// Identity of one input part, as of the run that produced the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct InputFingerprint {
    /// The part's display name (local path, or remote URL).
    pub path: String,
    /// File size in bytes. `None` for an input whose size cannot be stat'ed
    /// (a remote object).
    pub byte_len: Option<u64>,
    /// Modification time in nanoseconds since the Unix epoch, when available.
    pub mtime_nanos: Option<i128>,
    /// Row groups selected for this part by `--bbox` / `--filter` pruning.
    /// `None` = every row group.
    pub row_groups: Option<Vec<usize>>,
    /// Row groups the part has in total.
    pub row_groups_total: Option<usize>,
}

/// Everything that must be identical between the run that saved a plan and
/// the run that loads it.
///
/// The `options` map is a `field -> canonical value` listing rather than a
/// single digest so a mismatch can name the offending field. It covers the
/// options that change the *assignment* — mode, the level plan, every
/// thinning knob, the rank plan, the ladder, the filters — and deliberately
/// omits the write-side knobs (profile, row-group sizing, in-flight depth,
/// compression), which is exactly what makes a plan reusable across them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Fingerprint {
    /// `CARGO_PKG_VERSION` of the tylertoo that wrote the plan.
    pub tylertoo_version: String,
    /// Thinning-relevant options, `field -> canonical value`.
    pub options: BTreeMap<String, String>,
    /// One entry per input part, in read order.
    pub inputs: Vec<InputFingerprint>,
}

impl Fingerprint {
    /// Capture the fingerprint of the run about to start.
    pub fn capture(
        source: &ConvertSource,
        selected_row_groups: Option<&RowGroupSelection>,
        options: &ConvertOptions,
    ) -> Self {
        let parts = source.parts();
        let selected = selected_row_groups.map(RowGroupSelection::parts);
        let inputs = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let path = part.display_name();
                let meta = std::fs::metadata(&path).ok();
                InputFingerprint {
                    byte_len: meta.as_ref().map(std::fs::Metadata::len),
                    mtime_nanos: meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i128),
                    row_groups: selected.and_then(|s| s.get(i).cloned()),
                    row_groups_total: None,
                    path,
                }
            })
            .collect();
        Fingerprint {
            tylertoo_version: env!("CARGO_PKG_VERSION").to_string(),
            options: options_digest(options),
            inputs,
        }
    }

    /// Verify `self` (loaded from a plan) against the current run's
    /// fingerprint. Any mismatch is a hard error naming the offending field.
    pub fn verify(&self, current: &Fingerprint) -> Result<(), ConvertError> {
        if self.tylertoo_version != current.tylertoo_version {
            return Err(mismatch(
                "tylertoo_version",
                &self.tylertoo_version,
                &current.tylertoo_version,
            ));
        }
        for (key, saved) in &self.options {
            match current.options.get(key) {
                Some(now) if now == saved => {}
                Some(now) => return Err(mismatch(&format!("option {key}"), saved, now)),
                None => return Err(mismatch(&format!("option {key}"), saved, "<absent>")),
            }
        }
        for key in current.options.keys() {
            if !self.options.contains_key(key) {
                return Err(mismatch(
                    &format!("option {key}"),
                    "<absent>",
                    &current.options[key],
                ));
            }
        }
        if self.inputs.len() != current.inputs.len() {
            return Err(mismatch(
                "input part count",
                &self.inputs.len().to_string(),
                &current.inputs.len().to_string(),
            ));
        }
        for (saved, now) in self.inputs.iter().zip(&current.inputs) {
            verify_input(saved, now)?;
        }
        Ok(())
    }
}

/// Per-part fingerprint comparison, field by field.
fn verify_input(saved: &InputFingerprint, now: &InputFingerprint) -> Result<(), ConvertError> {
    if saved.path != now.path {
        return Err(mismatch("input path", &saved.path, &now.path));
    }
    let what = |field: &str| format!("input {:?} {field}", saved.path);
    if saved.byte_len != now.byte_len {
        return Err(mismatch(
            &what("byte_len"),
            &opt_str(saved.byte_len),
            &opt_str(now.byte_len),
        ));
    }
    if saved.mtime_nanos != now.mtime_nanos {
        return Err(mismatch(
            &what("mtime"),
            &opt_str(saved.mtime_nanos),
            &opt_str(now.mtime_nanos),
        ));
    }
    if saved.row_groups != now.row_groups {
        return Err(mismatch(
            &what("selected row groups"),
            &format!("{:?}", saved.row_groups),
            &format!("{:?}", now.row_groups),
        ));
    }
    Ok(())
}

fn opt_str<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "<none>".to_string(), |v| v.to_string())
}

fn mismatch(field: &str, saved: &str, now: &str) -> ConvertError {
    ConvertError::InvalidConfig(format!(
        "--plan: saved plan does not match this run: {field} was {saved:?} when the plan was \
         saved but is {now:?} now. Re-run without --plan (add --save-plan to write a fresh one)."
    ))
}

/// The thinning-relevant options, canonicalized to strings.
///
/// Debug formatting is the canonical form: every sub-config derives `Debug`,
/// the rendering is stable for a given tylertoo version, and the version is
/// itself part of the fingerprint — so a formatting change across versions can
/// never be read as an options change.
fn options_digest(o: &ConvertOptions) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: String| {
        m.insert(k.to_string(), v);
    };
    put("mode", format!("{:?}", o.mode));
    put("levels", format!("{:?}", o.levels));
    put("gsd_base", format!("{:?}", o.gsd_base));
    put("assign", format!("{:?}", o.assign));
    put("density", format!("{:?}", o.density));
    put("entry_zoom", format!("{:?}", o.entry_zoom));
    put("sort_key", format!("{:?}", o.sort_key));
    put("class_ranking", format!("{:?}", o.class_ranking));
    put("no_auto_rank", format!("{:?}", o.no_auto_rank));
    put("representation", format!("{:?}", o.representation));
    put("cluster", format!("{:?}", o.cluster));
    put("accumulate", format!("{:?}", o.accumulate));
    put("coalesce_lines", format!("{:?}", o.coalesce_lines));
    put("coalesce_snap", format!("{:?}", o.coalesce_snap));
    put(
        "coalesce_max_level_rows",
        format!("{:?}", o.coalesce_max_level_rows),
    );
    put(
        "coalesce_junction_angle",
        format!("{:?}", o.coalesce_junction_angle),
    );
    put("bbox", format!("{:?}", o.bbox));
    put("filter", format!("{:?}", o.filter));
    put("properties", format!("{:?}", o.properties));
    // Simplification runs in pass 2, but the level plan's omission decision
    // (#211) and the coalesce chain counts both read it, so it belongs here.
    put("simplify", format!("{:?}", o.simplify));
    m
}

/// Dataset-wide tallies pass 2 and the convert report need, which otherwise
/// only exist while the pass-1 feature scratch is alive.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(super) struct PlanTotals {
    /// Total input rows streamed, INCLUDING skipped-geometry rows: the domain
    /// of every row-indexed table.
    pub n_rows: usize,
    /// Features that survived the scan (rows with a usable geometry).
    pub n_features: usize,
    /// Rows skipped for a null, empty, or non-finite geometry.
    pub skipped_rows: usize,
    /// Line features in the scan.
    pub n_lines: usize,
    /// Point features in the scan.
    pub n_points: usize,
    /// Polygon features in the scan.
    pub n_polygons: usize,
    /// Total Arrow byte size of the encoded geometry column across the scan
    /// (#305): sizes pass 2's RAM-vs-spill decision.
    pub geom_bytes: u64,
    /// Features whose bbox spans more than 180° of longitude (#188).
    pub antimeridian_suspect: usize,
    /// Features outside their CRS's coordinate range (#429).
    pub out_of_range: usize,
    /// Features valid for the CRS but outside the Web Mercator tiling domain.
    pub unprojectable: usize,
}

impl PlanTotals {
    /// The point/total ratio the profile heuristics read.
    pub fn point_ratio(&self) -> f64 {
        if self.n_features == 0 {
            0.0
        } else {
            self.n_points as f64 / self.n_features as f64
        }
    }
}

/// How the Q1 cell-winner ranking resolved, recorded for a reviewer of a
/// sharded build: tier, the chosen column, and (for the categorical tiers)
/// the vocabulary the ranks were drawn from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RankPlanProvenance {
    /// The ranking tier (`explicit-sort-key`, `class-ranking`,
    /// `auto-overture-roads`, `auto-confidence`, `size-fallback`).
    pub mode: String,
    /// The source column, when a tier has one.
    pub column: Option<String>,
    /// The categorical vocabulary, sorted; empty for the numeric tiers.
    pub vocabulary: Vec<String>,
}

impl RankPlanProvenance {
    /// Derive from the resolved [`RankingProvenance`] the writer records.
    pub fn from_provenance(p: &RankingProvenance) -> Self {
        RankPlanProvenance {
            mode: p.mode.clone(),
            column: p.column.clone(),
            vocabulary: p
                .ranks
                .as_ref()
                .map(|r| r.keys().cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// The entry-zoom ladder's *inputs* (#364). See the module's "Known gap".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct LadderProvenance {
    /// Column the rungs are drawn from.
    pub column: String,
    /// The rung-placement rule, canonicalized.
    pub kind: String,
}

/// The coalescing (Q3) line scratch, geometry encoded as WKB.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CoalescePlan {
    /// Source row index per collected line, ascending input order.
    pub rows: Vec<usize>,
    /// WKB of each line, parallel to `rows`.
    pub wkb: Vec<Vec<u8>>,
    /// Sort key per line, parallel to `rows`.
    pub sort_keys: Vec<Option<f64>>,
    /// Interned class group per line; `None` = no class ranking active.
    pub groups: Option<Vec<u32>>,
}

/// The persisted pass-1 + assignment result.
///
/// Everything pass 2, the writer, and the convert report consume from pass 1
/// lives here, so a load fully replaces both the scan and the assignment.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ConvertPlan {
    /// On-disk format version ([`PLAN_FORMAT_VERSION`]).
    pub version: u32,
    /// Identity of the run that produced this plan.
    pub fingerprint: Fingerprint,
    /// The resolved level plan: `(gsd, zoom)` per planned level.
    pub level_specs: Vec<(f64, Option<u8>)>,
    /// Per-level winner counts, cumulative in duplicating mode.
    pub counts: Vec<usize>,
    /// Index of the canonical (finest) planned level.
    pub finest: usize,
    /// Coarsest level per INPUT ROW; the `UNASSIGNED_LEVEL` sentinel for
    /// skipped rows.
    pub min_levels: Vec<u8>,
    /// Per-row geometry kinds (Q3), or `None` when line coalescing is off.
    pub kinds: Option<Vec<FeatureKind>>,
    /// Per planned level, the tiny-polygon accumulator's carrier rows (#384).
    pub carriers: Vec<Vec<usize>>,
    /// Cluster tables (Q4), or `None` when clustering is off.
    pub cluster_tables: Option<ClusterTables>,
    /// Line scratch for coalescing (Q3), or `None`.
    pub coalesce: Option<CoalescePlan>,
    /// The ranking provenance block the writer stamps into the footer.
    pub rank_provenance: RankingProvenance,
    /// The dataset-global rank-plan decision, for a reviewer.
    pub rank_plan: RankPlanProvenance,
    /// The entry-zoom ladder inputs, when one applies.
    pub ladder: Option<LadderProvenance>,
    /// Dataset-wide tallies.
    pub totals: PlanTotals,
}

/// The JSON block carried in the IPC schema metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanMeta {
    version: u32,
    fingerprint: Fingerprint,
    level_specs: Vec<(f64, Option<u8>)>,
    counts: Vec<usize>,
    finest: usize,
    num_levels: usize,
    /// Aggregates per cluster entry (the accumulate-spec count); `cluster_agg`
    /// is a flat array with this stride.
    cluster_agg_stride: usize,
    has_cluster: bool,
    has_coalesce: bool,
    rank_provenance: RankingProvenance,
    rank_plan: RankPlanProvenance,
    ladder: Option<LadderProvenance>,
    totals: PlanTotals,
}

/// The single-batch schema every section list hangs off.
fn plan_schema(meta: String) -> Schema {
    // Every inner field is nullable: that is what the Arrow list builders
    // produce, and only `cluster_agg` / `coalesce_sort_key` actually carry
    // nulls (a missing aggregate / a missing sort key).
    let list = |name: &str, inner: DataType| {
        Field::new(
            name,
            DataType::LargeList(Arc::new(Field::new("item", inner, true))),
            true,
        )
    };
    let fields = vec![
        list("min_levels", DataType::UInt8),
        list("kinds", DataType::UInt8),
        list("carrier_offsets", DataType::UInt64),
        list("carrier_rows", DataType::UInt64),
        list("cluster_level", DataType::UInt32),
        list("cluster_row", DataType::UInt64),
        list("cluster_point_count", DataType::Int64),
        list("cluster_agg", DataType::Float64),
        list("coalesce_row", DataType::UInt64),
        list("coalesce_wkb", DataType::Binary),
        list("coalesce_sort_key", DataType::Float64),
        list("coalesce_group", DataType::UInt32),
    ];
    Schema::new(fields).with_metadata(HashMap::from([(PLAN_META_KEY.to_string(), meta)]))
}

/// Build a one-row `LargeList` column from an iterator of values, or a null
/// row when `present` is false.
macro_rules! list_col {
    ($builder:expr, $present:expr, $push:expr) => {{
        let mut b = LargeListBuilder::new($builder);
        if $present {
            #[allow(clippy::redundant_closure_call)]
            $push(&mut b);
            b.append(true);
        } else {
            b.append(false);
        }
        Arc::new(b.finish()) as arrow_array::ArrayRef
    }};
}

impl ConvertPlan {
    /// Write the plan to `path` as an Arrow IPC file.
    pub fn save(&self, path: &Path) -> Result<(), ConvertError> {
        let stride = self
            .cluster_tables
            .as_ref()
            .and_then(|t| t.iter().flat_map(|m| m.values()).next())
            .map_or(0, |e| e.aggregates.len());
        let meta = PlanMeta {
            version: self.version,
            fingerprint: self.fingerprint.clone(),
            level_specs: self.level_specs.clone(),
            counts: self.counts.clone(),
            finest: self.finest,
            num_levels: self.level_specs.len(),
            cluster_agg_stride: stride,
            has_cluster: self.cluster_tables.is_some(),
            has_coalesce: self.coalesce.is_some(),
            rank_provenance: self.rank_provenance.clone(),
            rank_plan: self.rank_plan.clone(),
            ladder: self.ladder.clone(),
            totals: self.totals,
        };
        let meta_json = serde_json::to_string(&meta)
            .map_err(|e| ConvertError::InvalidConfig(format!("--save-plan: {e}")))?;
        let schema = Arc::new(plan_schema(meta_json));

        // Cluster tables flattened and sorted by (level, row) so the artifact
        // is deterministic: the source is a HashMap per level.
        let mut cluster: Vec<(u32, u64, &ClusterEntry)> = Vec::new();
        if let Some(tables) = &self.cluster_tables {
            for (level, table) in tables.iter().enumerate() {
                for (row, entry) in table {
                    cluster.push((level as u32, *row as u64, entry));
                }
            }
            cluster.sort_by_key(|&(l, r, _)| (l, r));
        }

        let columns: Vec<arrow_array::ArrayRef> = vec![
            list_col!(UInt8Builder::new(), true, |b: &mut LargeListBuilder<
                UInt8Builder,
            >| {
                b.values().append_slice(&self.min_levels);
            }),
            list_col!(
                UInt8Builder::new(),
                self.kinds.is_some(),
                |b: &mut LargeListBuilder<UInt8Builder>| {
                    for k in self.kinds.iter().flatten() {
                        b.values().append_value(kind_code(*k));
                    }
                }
            ),
            list_col!(UInt64Builder::new(), true, |b: &mut LargeListBuilder<
                UInt64Builder,
            >| {
                let mut off = 0u64;
                b.values().append_value(off);
                for level in &self.carriers {
                    off += level.len() as u64;
                    b.values().append_value(off);
                }
            }),
            list_col!(UInt64Builder::new(), true, |b: &mut LargeListBuilder<
                UInt64Builder,
            >| {
                for row in self.carriers.iter().flatten() {
                    b.values().append_value(*row as u64);
                }
            }),
            list_col!(
                UInt32Builder::new(),
                self.cluster_tables.is_some(),
                |b: &mut LargeListBuilder<UInt32Builder>| {
                    for &(l, _, _) in &cluster {
                        b.values().append_value(l);
                    }
                }
            ),
            list_col!(
                UInt64Builder::new(),
                self.cluster_tables.is_some(),
                |b: &mut LargeListBuilder<UInt64Builder>| {
                    for &(_, r, _) in &cluster {
                        b.values().append_value(r);
                    }
                }
            ),
            list_col!(
                Int64Builder::new(),
                self.cluster_tables.is_some(),
                |b: &mut LargeListBuilder<Int64Builder>| {
                    for &(_, _, e) in &cluster {
                        b.values().append_value(e.point_count);
                    }
                }
            ),
            list_col!(
                Float64Builder::new(),
                self.cluster_tables.is_some(),
                |b: &mut LargeListBuilder<Float64Builder>| {
                    for &(_, _, e) in &cluster {
                        for v in &e.aggregates {
                            b.values().append_option(*v);
                        }
                    }
                }
            ),
            list_col!(
                UInt64Builder::new(),
                self.coalesce.is_some(),
                |b: &mut LargeListBuilder<UInt64Builder>| {
                    for row in self.coalesce.iter().flat_map(|c| &c.rows) {
                        b.values().append_value(*row as u64);
                    }
                }
            ),
            list_col!(
                BinaryBuilder::new(),
                self.coalesce.is_some(),
                |b: &mut LargeListBuilder<BinaryBuilder>| {
                    for w in self.coalesce.iter().flat_map(|c| &c.wkb) {
                        b.values().append_value(w);
                    }
                }
            ),
            list_col!(
                Float64Builder::new(),
                self.coalesce.is_some(),
                |b: &mut LargeListBuilder<Float64Builder>| {
                    for k in self.coalesce.iter().flat_map(|c| &c.sort_keys) {
                        b.values().append_option(*k);
                    }
                }
            ),
            list_col!(
                UInt32Builder::new(),
                self.coalesce.as_ref().is_some_and(|c| c.groups.is_some()),
                |b: &mut LargeListBuilder<UInt32Builder>| {
                    for g in self
                        .coalesce
                        .iter()
                        .filter_map(|c| c.groups.as_ref())
                        .flatten()
                    {
                        b.values().append_value(*g);
                    }
                }
            ),
        ];

        let batch = RecordBatch::try_new(schema.clone(), columns)?;
        let file = File::create(path)?;
        let mut w = FileWriter::try_new(BufWriter::new(file), &schema)?;
        w.write(&batch)?;
        w.finish()?;
        Ok(())
    }

    /// Read a plan back from `path`.
    pub fn load(path: &Path) -> Result<ConvertPlan, ConvertError> {
        let file = File::open(path)?;
        let mut reader = FileReader::try_new(BufReader::new(file), None)?;
        let schema = reader.schema();
        let meta_json = schema.metadata().get(PLAN_META_KEY).ok_or_else(|| {
            ConvertError::InvalidConfig(format!(
                "--plan: {} is not a tylertoo convert plan (no {PLAN_META_KEY} metadata)",
                path.display()
            ))
        })?;
        let meta: PlanMeta = serde_json::from_str(meta_json)
            .map_err(|e| ConvertError::InvalidConfig(format!("--plan: unreadable plan: {e}")))?;
        if meta.version != PLAN_FORMAT_VERSION {
            return Err(ConvertError::InvalidConfig(format!(
                "--plan: {} was written by plan format v{} but this tylertoo reads v{PLAN_FORMAT_VERSION}",
                path.display(),
                meta.version,
            )));
        }
        let batch = reader
            .next()
            .transpose()?
            .ok_or_else(|| ConvertError::InvalidConfig("--plan: empty plan file".to_string()))?;

        let min_levels: Vec<u8> = u8_section(&batch, "min_levels")?.unwrap_or_default();
        let kinds = u8_section(&batch, "kinds")?
            .map(|codes| codes.into_iter().map(kind_from_code).collect::<Vec<_>>());

        let carrier_offsets = u64_section(&batch, "carrier_offsets")?.unwrap_or_default();
        let carrier_rows = u64_section(&batch, "carrier_rows")?.unwrap_or_default();
        let carriers = rebuild_carriers(&carrier_offsets, &carrier_rows, meta.num_levels)?;

        let cluster_tables = if meta.has_cluster {
            Some(rebuild_cluster_tables(&batch, &meta)?)
        } else {
            None
        };

        let coalesce = if meta.has_coalesce {
            Some(rebuild_coalesce(&batch)?)
        } else {
            None
        };

        Ok(ConvertPlan {
            version: meta.version,
            fingerprint: meta.fingerprint,
            level_specs: meta.level_specs,
            counts: meta.counts,
            finest: meta.finest,
            min_levels,
            kinds,
            carriers,
            cluster_tables,
            coalesce,
            rank_provenance: meta.rank_provenance,
            rank_plan: meta.rank_plan,
            ladder: meta.ladder,
            totals: meta.totals,
        })
    }
}

fn kind_code(k: FeatureKind) -> u8 {
    match k {
        FeatureKind::Point => 0,
        FeatureKind::Line => 1,
        FeatureKind::Polygon => 2,
    }
}

fn kind_from_code(c: u8) -> FeatureKind {
    match c {
        1 => FeatureKind::Line,
        2 => FeatureKind::Polygon,
        _ => FeatureKind::Point,
    }
}

/// The single list value of `name`, or `None` when the section is absent.
fn section(batch: &RecordBatch, name: &str) -> Result<Option<arrow_array::ArrayRef>, ConvertError> {
    let idx = batch.schema().index_of(name).map_err(|_| {
        ConvertError::InvalidConfig(format!("--plan: plan file is missing section {name:?}"))
    })?;
    let col = batch.column(idx);
    let list = col
        .as_any()
        .downcast_ref::<LargeListArray>()
        .ok_or_else(|| {
            ConvertError::InvalidConfig(format!("--plan: section {name:?} has the wrong layout"))
        })?;
    if list.is_null(0) {
        return Ok(None);
    }
    Ok(Some(list.value(0)))
}

macro_rules! typed_section {
    ($fn_name:ident, $arr:ty, $native:ty) => {
        fn $fn_name(batch: &RecordBatch, name: &str) -> Result<Option<Vec<$native>>, ConvertError> {
            let Some(values) = section(batch, name)? else {
                return Ok(None);
            };
            let a = values.as_any().downcast_ref::<$arr>().ok_or_else(|| {
                ConvertError::InvalidConfig(format!(
                    "--plan: section {name:?} has the wrong value type"
                ))
            })?;
            Ok(Some(a.values().to_vec()))
        }
    };
}

typed_section!(u8_section, UInt8Array, u8);
typed_section!(u64_section, UInt64Array, u64);
typed_section!(u32_section, UInt32Array, u32);
typed_section!(i64_section, Int64Array, i64);

/// A nullable `Float64` section, preserving nulls.
fn f64_opt_section(
    batch: &RecordBatch,
    name: &str,
) -> Result<Option<Vec<Option<f64>>>, ConvertError> {
    let Some(values) = section(batch, name)? else {
        return Ok(None);
    };
    let a = values
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| {
            ConvertError::InvalidConfig(format!(
                "--plan: section {name:?} has the wrong value type"
            ))
        })?;
    Ok(Some(a.iter().collect()))
}

fn rebuild_carriers(
    offsets: &[u64],
    rows: &[u64],
    num_levels: usize,
) -> Result<Vec<Vec<usize>>, ConvertError> {
    if offsets.len() != num_levels + 1 {
        return Err(ConvertError::InvalidConfig(format!(
            "--plan: carrier offsets have {} entries but the plan has {num_levels} level(s)",
            offsets.len(),
        )));
    }
    let mut out = Vec::with_capacity(num_levels);
    for w in offsets.windows(2) {
        let (a, b) = (w[0] as usize, w[1] as usize);
        if a > b || b > rows.len() {
            return Err(ConvertError::InvalidConfig(
                "--plan: carrier offsets are out of range".to_string(),
            ));
        }
        out.push(rows[a..b].iter().map(|&r| r as usize).collect());
    }
    Ok(out)
}

fn rebuild_cluster_tables(
    batch: &RecordBatch,
    meta: &PlanMeta,
) -> Result<ClusterTables, ConvertError> {
    let levels = u32_section(batch, "cluster_level")?.unwrap_or_default();
    let rows = u64_section(batch, "cluster_row")?.unwrap_or_default();
    let counts = i64_section(batch, "cluster_point_count")?.unwrap_or_default();
    let aggs = f64_opt_section(batch, "cluster_agg")?.unwrap_or_default();
    let stride = meta.cluster_agg_stride;
    if levels.len() != rows.len() || levels.len() != counts.len() {
        return Err(ConvertError::InvalidConfig(
            "--plan: cluster sections have mismatched lengths".to_string(),
        ));
    }
    if aggs.len() != levels.len() * stride {
        return Err(ConvertError::InvalidConfig(
            "--plan: cluster aggregate section has the wrong length".to_string(),
        ));
    }
    let mut tables: ClusterTables = vec![HashMap::new(); meta.num_levels];
    for (i, ((&level, &row), &point_count)) in levels
        .iter()
        .zip(rows.iter())
        .zip(counts.iter())
        .enumerate()
    {
        let level = level as usize;
        let table = tables.get_mut(level).ok_or_else(|| {
            ConvertError::InvalidConfig(format!("--plan: cluster entry names level {level}"))
        })?;
        table.insert(
            row as usize,
            ClusterEntry {
                point_count,
                aggregates: aggs[i * stride..(i + 1) * stride].to_vec(),
            },
        );
    }
    Ok(tables)
}

fn rebuild_coalesce(batch: &RecordBatch) -> Result<CoalescePlan, ConvertError> {
    let rows: Vec<usize> = u64_section(batch, "coalesce_row")?
        .unwrap_or_default()
        .into_iter()
        .map(|r| r as usize)
        .collect();
    let sort_keys = f64_opt_section(batch, "coalesce_sort_key")?.unwrap_or_default();
    let groups = u32_section(batch, "coalesce_group")?;
    let wkb = match section(batch, "coalesce_wkb")? {
        None => Vec::new(),
        Some(values) => {
            let a = values
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| {
                    ConvertError::InvalidConfig(
                        "--plan: section \"coalesce_wkb\" has the wrong value type".to_string(),
                    )
                })?;
            (0..a.len()).map(|i| a.value(i).to_vec()).collect()
        }
    };
    if wkb.len() != rows.len() || sort_keys.len() != rows.len() {
        return Err(ConvertError::InvalidConfig(
            "--plan: coalesce sections have mismatched lengths".to_string(),
        ));
    }
    Ok(CoalescePlan {
        rows,
        wkb,
        sort_keys,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_fingerprint() -> Fingerprint {
        Fingerprint {
            tylertoo_version: env!("CARGO_PKG_VERSION").to_string(),
            options: BTreeMap::from([("mode".to_string(), "Duplicating".to_string())]),
            inputs: vec![InputFingerprint {
                path: "/tmp/in.parquet".to_string(),
                byte_len: Some(1234),
                mtime_nanos: Some(99),
                row_groups: Some(vec![0, 2]),
                row_groups_total: Some(3),
            }],
        }
    }

    fn tiny_plan() -> ConvertPlan {
        ConvertPlan {
            version: PLAN_FORMAT_VERSION,
            fingerprint: tiny_fingerprint(),
            level_specs: vec![(1000.0, Some(5)), (500.0, Some(6)), (250.0, Some(7))],
            counts: vec![2, 5, 9],
            finest: 2,
            min_levels: vec![0, 1, 2, 255, 1, 0, 2, 2, 1],
            kinds: Some(vec![
                FeatureKind::Point,
                FeatureKind::Line,
                FeatureKind::Polygon,
                FeatureKind::Point,
                FeatureKind::Line,
                FeatureKind::Line,
                FeatureKind::Polygon,
                FeatureKind::Polygon,
                FeatureKind::Line,
            ]),
            carriers: vec![vec![3, 7], vec![], vec![1]],
            cluster_tables: Some(vec![
                HashMap::from([
                    (
                        5,
                        ClusterEntry {
                            point_count: 3,
                            aggregates: vec![Some(1.5), None],
                        },
                    ),
                    (
                        2,
                        ClusterEntry {
                            point_count: 7,
                            aggregates: vec![None, Some(-2.25)],
                        },
                    ),
                ]),
                HashMap::new(),
                HashMap::new(),
            ]),
            coalesce: Some(CoalescePlan {
                rows: vec![1, 4, 8],
                wkb: vec![vec![1, 2, 3], vec![], vec![9]],
                sort_keys: vec![Some(0.5), None, Some(3.0)],
                groups: Some(vec![0, 1, 0]),
            }),
            rank_provenance: RankingProvenance {
                mode: "class-ranking".to_string(),
                column: Some("class".to_string()),
                ranks: Some(BTreeMap::from([
                    ("motorway".to_string(), 5.0),
                    ("primary".to_string(), 4.0),
                ])),
                unknown_rank: Some(0.5),
            },
            rank_plan: RankPlanProvenance {
                mode: "class-ranking".to_string(),
                column: Some("class".to_string()),
                vocabulary: vec!["motorway".to_string(), "primary".to_string()],
            },
            ladder: Some(LadderProvenance {
                column: "pop".to_string(),
                kind: "DenseRank { step: 1 }".to_string(),
            }),
            totals: PlanTotals {
                n_rows: 9,
                n_features: 8,
                skipped_rows: 1,
                n_lines: 4,
                n_points: 2,
                n_polygons: 2,
                geom_bytes: 4096,
                antimeridian_suspect: 1,
                out_of_range: 0,
                unprojectable: 2,
            },
        }
    }

    /// The oracle for the artifact itself: every field survives a save/load
    /// round trip unchanged.
    #[test]
    fn plan_roundtrip_preserves_winner_tables() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = tiny_plan();
        plan.save(f.path()).unwrap();
        let back = ConvertPlan::load(f.path()).unwrap();
        assert_eq!(plan, back);
    }

    /// The empty/absent sections must round trip too (no clustering, no
    /// coalescing, no kinds, no carriers).
    #[test]
    fn plan_roundtrip_with_absent_sections() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = ConvertPlan {
            kinds: None,
            cluster_tables: None,
            coalesce: None,
            carriers: vec![vec![], vec![], vec![]],
            ladder: None,
            ..tiny_plan()
        };
        plan.save(f.path()).unwrap();
        assert_eq!(plan, ConvertPlan::load(f.path()).unwrap());
    }

    /// A coalesce scratch with no class groups keeps `groups: None`.
    #[test]
    fn plan_roundtrip_coalesce_without_groups() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = ConvertPlan {
            coalesce: Some(CoalescePlan {
                rows: vec![0, 1],
                wkb: vec![vec![7], vec![8, 9]],
                sort_keys: vec![None, None],
                groups: None,
            }),
            ..tiny_plan()
        };
        plan.save(f.path()).unwrap();
        assert_eq!(plan, ConvertPlan::load(f.path()).unwrap());
    }

    #[test]
    fn plan_fingerprint_rejects_changed_input() {
        let saved = tiny_fingerprint();

        let mut bigger = tiny_fingerprint();
        bigger.inputs[0].byte_len = Some(9999);
        let err = saved.verify(&bigger).unwrap_err().to_string();
        assert!(err.contains("byte_len"), "names the field: {err}");
        assert!(err.contains("1234") && err.contains("9999"), "{err}");

        let mut touched = tiny_fingerprint();
        touched.inputs[0].mtime_nanos = Some(100);
        let err = touched_err(&saved, &touched);
        assert!(err.contains("mtime"), "names the field: {err}");

        let mut moved = tiny_fingerprint();
        moved.inputs[0].path = "/tmp/other.parquet".to_string();
        let err = touched_err(&saved, &moved);
        assert!(err.contains("input path"), "names the field: {err}");

        let mut repruned = tiny_fingerprint();
        repruned.inputs[0].row_groups = Some(vec![0, 1, 2]);
        let err = touched_err(&saved, &repruned);
        assert!(err.contains("selected row groups"), "names it: {err}");

        // Identical fingerprints verify.
        saved.verify(&tiny_fingerprint()).unwrap();
    }

    fn touched_err(saved: &Fingerprint, current: &Fingerprint) -> String {
        saved.verify(current).unwrap_err().to_string()
    }

    #[test]
    fn plan_fingerprint_rejects_changed_options() {
        let saved = tiny_fingerprint();
        let mut changed = tiny_fingerprint();
        changed
            .options
            .insert("mode".to_string(), "Partitioning".to_string());
        let err = touched_err(&saved, &changed);
        assert!(err.contains("option mode"), "names the field: {err}");
        assert!(
            err.contains("Duplicating") && err.contains("Partitioning"),
            "{err}"
        );

        // A knob that did not exist when the plan was saved is also caught.
        let mut extra = tiny_fingerprint();
        extra
            .options
            .insert("polygon_thinning".to_string(), "0.5".to_string());
        let err = touched_err(&saved, &extra);
        assert!(err.contains("option polygon_thinning"), "{err}");
    }

    #[test]
    fn plan_fingerprint_rejects_version_change() {
        let saved = tiny_fingerprint();
        let mut other = tiny_fingerprint();
        other.tylertoo_version = "0.0.1-not-this".to_string();
        let err = touched_err(&saved, &other);
        assert!(err.contains("tylertoo_version"), "{err}");
    }

    /// A different plan-format version is refused rather than misread.
    #[test]
    fn plan_rejects_foreign_format_version() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = ConvertPlan {
            version: PLAN_FORMAT_VERSION + 1,
            ..tiny_plan()
        };
        plan.save(f.path()).unwrap();
        let err = ConvertPlan::load(f.path()).unwrap_err().to_string();
        assert!(err.contains("plan format"), "{err}");
    }

    /// Options the plan is explicitly reusable across must NOT be
    /// fingerprinted: a saved plan is meant to be replayed with different
    /// write-side knobs.
    #[test]
    fn options_digest_omits_write_side_knobs() {
        use super::super::level::MemoryProfile;

        let base = ConvertOptions::default();
        let tuned = ConvertOptions {
            profile: MemoryProfile::Bounded,
            max_row_group_size: 123,
            read_batch_size: 999,
            in_flight_batches: 3,
            full_column_stats: true,
            cogp_compat_key: true,
            spill_dir: Some(std::path::PathBuf::from("/tmp/elsewhere")),
            ..base.clone()
        };
        assert_eq!(options_digest(&base), options_digest(&tuned));

        // ...while a thinning knob does change it, and the changed key is
        // exactly the one named.
        let thinned = ConvertOptions {
            gsd_base: base.gsd_base * 2.0,
            ..base.clone()
        };
        let a = options_digest(&base);
        let b = options_digest(&thinned);
        let differing: Vec<&String> = a.keys().filter(|k| a[*k] != b[*k]).collect();
        assert_eq!(differing, vec!["gsd_base"]);
    }
}
