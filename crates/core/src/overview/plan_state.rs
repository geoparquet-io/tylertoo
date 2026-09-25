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
//! An 8-byte magic ([`PLAN_MAGIC`]) and an xxh3-64 checksum of everything
//! after it, then one Arrow IPC **file** holding a single record batch whose
//! every column is
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
//! # Reading a plan is reading hostile input
//!
//! A plan travels between machines (the sharding motivation) and is named by
//! a user-supplied `--plan PATH`, so it is held to the bar #417/#489/#430
//! set for PMTiles reading: **hostile input produces an error, never a
//! panic**. The checksum is verified before a single byte reaches an Arrow
//! decoder, and every structural check past it reports through [`plan_err`],
//! naming the flag and the path. See the `plan_rejects_*` tests (#512).
//!
//! # Fingerprint
//!
//! Loading a plan whose fingerprint does not match the current run is a hard
//! error naming the offending field — never a silent stale run. The
//! fingerprint pins the tylertoo version, every thinning-relevant option, and
//! each input part's identity: path/URL, byte size, footer row count, footer
//! row-group count and selected row groups for every part, plus mtime for a
//! local file. See [`Fingerprint`].
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
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
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
use super::ladder::EntryZoomSpec;
use super::level::RankingProvenance;
use super::stream::{CoalesceScratch, WinnerTables};

/// Schema-metadata key under which the plan's JSON scalars travel.
pub(super) const PLAN_META_KEY: &str = "tylertoo:convert_plan";

/// On-disk format version of the plan artifact. Bump on any incompatible
/// change to the section layout or the JSON block.
///
/// v2 wrapped the Arrow IPC payload in the [`PLAN_MAGIC`] + checksum header
/// (#512) and added the per-part row count to the fingerprint (#511).
pub(super) const PLAN_FORMAT_VERSION: u32 = 2;

/// The first bytes of every plan artifact, ahead of the Arrow IPC payload.
///
/// A plan travels between machines (the sharding motivation) and is named by
/// a user-supplied `--plan PATH`, so the reader must be able to say "this is
/// not a plan" and "this plan is damaged" *before* handing a single byte to
/// an Arrow decoder — a byte-flip sweep over a real 10.5 KB artifact panicked
/// inside arrow's buffer/IPC decoders at 11 of 203 positions (#512).
pub(super) const PLAN_MAGIC: &[u8; 8] = b"TTPLAN\x00\x02";

/// The format-independent part of [`PLAN_MAGIC`]: everything but the trailing
/// version byte.
///
/// Readers match on THIS and then compare the version byte separately, so a
/// plan written by a future tylertoo is reported as "format v3" rather than
/// as "bad magic bytes" — the latter sends whoever reads the message looking
/// for a corrupt file instead of a version skew.
pub(super) const PLAN_MAGIC_PREFIX: &[u8; 7] = b"TTPLAN\x00";

/// [`PLAN_FORMAT_VERSION`] as it appears in the magic's last byte.
pub(super) const PLAN_FORMAT_VERSION_BYTE: u8 = PLAN_FORMAT_VERSION as u8;

const _: () = assert!(
    PLAN_MAGIC[PLAN_MAGIC_PREFIX.len()] == PLAN_FORMAT_VERSION_BYTE,
    "PLAN_MAGIC's version byte must track PLAN_FORMAT_VERSION"
);

/// Bytes ahead of the IPC payload: [`PLAN_MAGIC`] (8) + the payload's
/// xxh3-64 checksum, little-endian (8).
const PLAN_HEADER_LEN: usize = 16;

/// Read `r` to the end, returning the xxh3-64 of what it yielded.
///
/// Streamed in fixed-size chunks rather than slurped: the artifact is
/// O(input rows), and a planet-scale plan must not be materialized twice
/// just to be checksummed.
fn hash_payload<R: Read>(mut r: R) -> std::io::Result<u64> {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.digest());
        }
        hasher.update(&buf[..n]);
    }
}

/// A `Read + Seek` view of `inner` with its first `offset` bytes hidden.
///
/// Arrow's `FileWriter` records each block's position relative to the start
/// of the stream it wrote to, and `FileReader` seeks to those positions
/// absolutely — so the IPC payload has to look like it begins at byte 0.
/// This shim is what lets the plan carry its magic + checksum header in
/// front of an otherwise ordinary Arrow IPC file.
struct Offset<R> {
    inner: R,
    offset: u64,
}

impl<R: Read> Read for Offset<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for Offset<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let abs = match pos {
            SeekFrom::Start(n) => {
                let target = self.offset.checked_add(n).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek past u64::MAX")
                })?;
                self.inner.seek(SeekFrom::Start(target))?
            }
            other => self.inner.seek(other)?,
        };
        Ok(abs.saturating_sub(self.offset))
    }
}

/// The plan's IPC payload as arrow wants to see it: byte 0 = the first IPC
/// byte, with the header hidden.
fn ipc_view(file: File) -> Offset<BufReader<File>> {
    Offset {
        inner: BufReader::new(file),
        offset: PLAN_HEADER_LEN as u64,
    }
}

/// Every plan-reader failure, named and carrying the `--plan` path.
///
/// The bar #417/#489/#430 set for PMTiles reading — "hostile input errors,
/// never panics" — applies here too: a sharded build that ships one damaged
/// plan to a worker must abort with a message naming the flag and the file,
/// not an arrow-internal index panic.
fn plan_err(path: &Path, what: &str) -> ConvertError {
    ConvertError::InvalidConfig(format!("--plan {}: {what}", path.display()))
}

/// The `--save-plan` counterpart of [`plan_err`].
fn save_plan_err(path: &Path, what: &str) -> ConvertError {
    ConvertError::InvalidConfig(format!("--save-plan {}: {what}", path.display()))
}

/// Identity of one input part, as of the run that produced the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct InputFingerprint {
    /// The part's display name (local path, or remote URL).
    pub path: String,
    /// Size in bytes: the local file's length, or a remote object's
    /// Content-Length as the input layer already holds it (#511). `None`
    /// only when neither is obtainable.
    pub byte_len: Option<u64>,
    /// Modification time in nanoseconds since the Unix epoch. Local files
    /// only — an object store's `LastModified` is not plumbed through the
    /// input layer, so this stays `None` for remote parts.
    pub mtime_nanos: Option<i128>,
    /// Row groups selected for this part by `--bbox` / `--filter` pruning.
    /// `None` = every row group.
    pub row_groups: Option<Vec<usize>>,
    /// Row groups the part has in total, from its parquet footer.
    #[serde(default)]
    pub row_groups_total: Option<usize>,
    /// Rows the part has in total, from its parquet footer (#511).
    ///
    /// **This is the load-bearing content binding for a remote part.** The
    /// winner table `min_levels` is addressed by row *position*, so an input
    /// swapped under a saved plan either silently corrupts the pyramid (fewer
    /// rows) or indexes out of bounds (more rows). `fs::metadata` cannot see a
    /// remote object at all; the footer's row count can, for free.
    #[serde(default)]
    pub num_rows: Option<i64>,
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
    ///
    /// `flag` is the plan flag that asked for it (`"save-plan"` or
    /// `"plan"`), used only in the remote-input honesty warning.
    pub fn capture(
        source: &ConvertSource,
        selected_row_groups: Option<&RowGroupSelection>,
        options: &ConvertOptions,
        flag: &str,
    ) -> Result<Self, ConvertError> {
        let parts = source.parts();
        let selected = selected_row_groups.map(RowGroupSelection::parts);
        // Footer facts (rows, row groups) per part: free, and — unlike
        // `fs::metadata` — available for remote objects too (#511).
        let footer = source.part_row_counts()?;
        let inputs = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let path = part.display_name();
                let meta = std::fs::metadata(&path).ok();
                InputFingerprint {
                    // A remote object has no local metadata; its
                    // Content-Length is already in hand from the HEAD /
                    // prefix listing that opened it.
                    byte_len: meta
                        .as_ref()
                        .map(std::fs::Metadata::len)
                        .or_else(|| part.fetch_stats().map(|s| s.object_size)),
                    mtime_nanos: meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i128),
                    row_groups: selected.and_then(|s| s.get(i).cloned()),
                    row_groups_total: footer.get(i).map(|&(_, groups)| groups),
                    num_rows: footer.get(i).map(|&(rows, _)| rows),
                    path,
                }
            })
            .collect();
        warn_remote_binding(source, flag);
        Ok(Fingerprint {
            tylertoo_version: env!("CARGO_PKG_VERSION").to_string(),
            options: options_digest(options),
            inputs,
        })
    }

    /// Verify `self` (loaded from a plan) against the current run's
    /// fingerprint. Any mismatch is a hard error naming the offending field.
    #[cfg(test)]
    pub fn verify(&self, current: &Fingerprint) -> Result<(), ConvertError> {
        self.verify_with(current, SelectionRule::Identical)
    }

    /// [`Fingerprint::verify`], with an explicit rule for the one term a
    /// shard is allowed to narrow.
    pub fn verify_with(
        &self,
        current: &Fingerprint,
        selection: SelectionRule,
    ) -> Result<(), ConvertError> {
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
            verify_input(saved, now, selection)?;
        }
        Ok(())
    }
}

/// How strictly the fingerprint's per-part row-group selection is compared.
///
/// Every other term stays an equality: a shard tiles the *same* input with the
/// *same* thinning options as the plan, and only narrows which row groups it
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionRule {
    /// The ordinary rule: the run must read exactly the row groups the plan
    /// was saved over. Anything else means the plan's row-indexed winner table
    /// no longer lines up with the stream, which is the corruption #511/#512
    /// exist to catch.
    Identical,
    /// The shard rule (#498): the run may read a **subset** of the plan's row
    /// groups, and nothing outside them.
    ///
    /// This is the only relaxation a `--shard` run gets, and it is safe for
    /// exactly one reason: the plan records the selection it was saved over,
    /// so a shard can compute where each of its groups sits in the plan's row
    /// stream and re-address the winner table onto its own
    /// ([`rebase_plan_for_shard`]). A *superset* — a row the plan never saw —
    /// has no winner byte at all and is refused here.
    ///
    /// Narrowing is also what makes a shard cheaper than the whole build: the
    /// subset is the row groups whose bbox reaches the shard's tile range, and
    /// a row group outside it cannot contribute to any tile the shard owns.
    SubsetAllowed,
}

/// Per-part fingerprint comparison, field by field.
fn verify_input(
    saved: &InputFingerprint,
    now: &InputFingerprint,
    selection: SelectionRule,
) -> Result<(), ConvertError> {
    if saved.path != now.path {
        return Err(mismatch("input path", &saved.path, &now.path));
    }
    let what = |field: &str| format!("input {:?} {field}", saved.path);
    // #511: the content binding that also holds for a remote part, and the
    // most informative thing to say first. The winner table is addressed by
    // row position, so a changed row count is the difference between a
    // correct replay and either a silently corrupted pyramid (fewer rows) or
    // an out-of-bounds index (more rows).
    if saved.num_rows != now.num_rows {
        return Err(mismatch(
            &what("row count"),
            &opt_str(saved.num_rows),
            &opt_str(now.num_rows),
        ));
    }
    if saved.row_groups_total != now.row_groups_total {
        return Err(mismatch(
            &what("row group count"),
            &opt_str(saved.row_groups_total),
            &opt_str(now.row_groups_total),
        ));
    }
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
    match selection {
        SelectionRule::Identical => {
            if saved.row_groups != now.row_groups {
                return Err(mismatch(
                    &what("selected row groups"),
                    &format!("{:?}", saved.row_groups),
                    &format!("{:?}", now.row_groups),
                ));
            }
        }
        SelectionRule::SubsetAllowed => {
            if let Some(extra) = selection_excess(
                saved.row_groups.as_deref(),
                now.row_groups.as_deref(),
                saved.row_groups_total,
            ) {
                return Err(ConvertError::InvalidConfig(format!(
                    "--plan: this shard would read row group {extra} of input {:?}, which the \
                     plan was not saved over (the plan covers {}). A shard may only narrow the \
                     plan's row-group selection, never widen it — the winner table has no row \
                     for a group pass 1 never saw. Re-save the plan over the full input.",
                    saved.path,
                    match &saved.row_groups {
                        Some(list) => format!("{} group(s): {list:?}", list.len()),
                        None => "every row group".to_string(),
                    }
                )));
            }
        }
    }
    Ok(())
}

/// One contiguous run of the plan's row stream that a shard also reads.
///
/// A row group is contiguous in the plan's row stream by construction (the
/// reader streams parts in order and, within a part, the selected groups in
/// ascending index order), so the shard's domain is a handful of runs rather
/// than a per-row bitmap — however many rows the dataset has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeptRun {
    /// Where the run starts in the **plan's** row stream.
    plan_base: usize,
    /// Where the same run starts in the **shard's** row stream.
    shard_base: usize,
    /// Rows in the run.
    len: usize,
}

/// Re-address a global convert plan onto the subset of row groups a shard
/// reads (#498) — the piece that makes a sharded build correct rather than
/// merely parallel.
///
/// # The problem
///
/// Every row-indexed section of a plan is addressed by row *position within
/// the row stream the plan was saved over*, not by absolute file row number:
/// pass 2 tags each batch with a running `row_offset` and looks up
/// `min_levels[row_offset + i]`. A shard that prunes row groups streams
/// *fewer* rows, so its own `row_offset` counts a different sequence — row 0
/// of the shard is whatever its first selected group starts at, which in the
/// plan's stream might be row 4 million. Handing the plan over unchanged
/// would silently read the wrong winner byte for nearly every feature: not a
/// crash, a quietly wrong pyramid.
///
/// # The fix
///
/// The plan records the row-group selection it was saved over, and row-group
/// row counts are footer facts. That is enough to compute where each of the
/// shard's groups sits in the plan's stream and to rewrite the plan's
/// sections into the shard's addressing before pass 2 ever runs. Afterwards
/// the shard is, to everything downstream, an ordinary `--plan` run whose
/// selection happens to be narrower — including for the row-domain check,
/// which is re-run against the compacted tables.
///
/// The values are never recomputed, only moved: which level a row enters at
/// stays the dataset-global decision the coarse job made. Rows the shard does
/// not read are dropped, which is sound because a row group is pruned only
/// when its bbox does not reach the shard's tile range, and a feature inside
/// that bbox cannot land in a tile the shard owns.
///
/// Returns the compacted row count.
pub(super) fn rebase_plan_for_shard(
    plan: &mut ConvertPlan,
    source: &ConvertSource,
    shard_selection: Option<&RowGroupSelection>,
    path: &Path,
) -> Result<usize, ConvertError> {
    let rg_rows = source.part_row_group_row_counts()?;
    let named = || path.display().to_string();

    // Walk the PLAN's stream in read order, marking the runs the shard keeps.
    let mut runs: Vec<KeptRun> = Vec::new();
    let mut plan_cursor = 0usize;
    let mut shard_cursor = 0usize;
    for (part_idx, part_rows) in rg_rows.iter().enumerate() {
        let plan_groups: Vec<usize> = match plan.fingerprint.inputs.get(part_idx) {
            Some(fp) => match &fp.row_groups {
                Some(list) => list.clone(),
                None => (0..part_rows.len()).collect(),
            },
            None => {
                return Err(ConvertError::InvalidConfig(format!(
                    "--plan: {} names {} input part(s) but this run has at least {}",
                    named(),
                    plan.fingerprint.inputs.len(),
                    part_idx + 1,
                )))
            }
        };
        let shard_groups: Option<&[usize]> = shard_selection
            .map(RowGroupSelection::parts)
            .and_then(|p| p.get(part_idx))
            .map(Vec::as_slice);
        for g in plan_groups {
            let rows = part_rows.get(g).copied().unwrap_or(0).max(0) as usize;
            let kept = shard_groups.is_none_or(|s| s.contains(&g));
            if kept && rows > 0 {
                // Extend the previous run when the groups are adjacent in
                // BOTH streams, so a shard that keeps everything collapses to
                // a single run and costs one memcpy.
                match runs.last_mut() {
                    Some(last) if last.plan_base + last.len == plan_cursor => last.len += rows,
                    _ => runs.push(KeptRun {
                        plan_base: plan_cursor,
                        shard_base: shard_cursor,
                        len: rows,
                    }),
                }
                shard_cursor += rows;
            }
            plan_cursor += rows;
        }
    }

    if plan_cursor != plan.min_levels.len() {
        return Err(ConvertError::InvalidConfig(format!(
            "--plan: {} holds {} winner-table row(s) but the row group(s) it was saved over \
             hold {plan_cursor} row(s). Re-run without --plan (add --save-plan to write a \
             fresh one).",
            named(),
            plan.min_levels.len(),
        )));
    }

    // Nothing was pruned: the shard reads exactly the plan's stream, so the
    // addressing already matches and there is nothing to move.
    if shard_cursor == plan_cursor {
        return Ok(plan_cursor);
    }

    let take = |src: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(shard_cursor);
        for r in &runs {
            out.extend_from_slice(&src[r.plan_base..r.plan_base + r.len]);
        }
        out
    };
    plan.min_levels = take(&plan.min_levels);
    if let Some(kinds) = &plan.kinds {
        let mut out = Vec::with_capacity(shard_cursor);
        for r in &runs {
            out.extend_from_slice(&kinds[r.plan_base..r.plan_base + r.len]);
        }
        plan.kinds = Some(out);
    }

    // Side tables are keyed by the same row index, so they move with it.
    // A key outside the shard's runs belongs to a row the shard does not
    // read and is dropped.
    for level in &mut plan.carriers {
        let mut out: Vec<usize> = level
            .iter()
            .filter_map(|&row| shard_row(&runs, row))
            .collect();
        out.sort_unstable();
        *level = out;
    }
    if let Some(tables) = &mut plan.cluster_tables {
        for table in tables.iter_mut() {
            *table = table
                .drain()
                .filter_map(|(row, entry)| shard_row(&runs, row).map(|r| (r, entry)))
                .collect();
        }
    }
    // Line coalescing is refused alongside `--shard` up front (a merged chain
    // spans whatever rows the chain touched, which no single row group's bbox
    // bounds), so this section must be absent by the time we get here.
    debug_assert!(
        plan.coalesce.is_none(),
        "--shard with line coalescing must have been refused in validate_options"
    );

    // The totals describe what this run will read, not the dataset: they feed
    // the report and the pass-2 RAM-vs-spill decision, and a global
    // `geom_bytes` on a one-sixteenth shard would send every shard to disk.
    // The two counts are exact (recounted from the compacted winner table);
    // `geom_bytes` is prorated, which is all it ever was — an estimate.
    let assigned = plan
        .min_levels
        .iter()
        .filter(|&&ml| ml != super::stream::UNASSIGNED_LEVEL)
        .count();
    let ratio = shard_cursor as f64 / plan_cursor as f64;
    plan.totals.geom_bytes = (plan.totals.geom_bytes as f64 * ratio).round() as u64;
    plan.totals.n_rows = shard_cursor;
    plan.totals.n_features = assigned;
    plan.totals.skipped_rows = shard_cursor - assigned;
    log::info!(
        "[convert] shard: re-addressed the convert plan onto {shard_cursor} of its \
         {plan_cursor} row(s) in {} contiguous run(s); {assigned} feature(s) to build",
        runs.len()
    );
    Ok(shard_cursor)
}

/// Map a plan-stream row index into the shard's stream, or `None` when the
/// shard does not read that row.
fn shard_row(runs: &[KeptRun], row: usize) -> Option<usize> {
    // Rightmost run starting at or before `row`.
    let idx = runs
        .partition_point(|r| r.plan_base <= row)
        .checked_sub(1)?;
    let r = runs[idx];
    (row < r.plan_base + r.len).then(|| r.shard_base + (row - r.plan_base))
}

/// The first row group `now` selects that `saved` did not, or `None` when
/// `now` is a subset of `saved`.
///
/// `None` on either side means "every row group", so `saved = None` accepts
/// anything within `total`, and `now = None` is a subset only when `saved`
/// already covered everything.
fn selection_excess(
    saved: Option<&[usize]>,
    now: Option<&[usize]>,
    total: Option<usize>,
) -> Option<usize> {
    let within_total = |g: usize| total.is_none_or(|t| g < t);
    match (saved, now) {
        (None, None) => None,
        (None, Some(now)) => now.iter().copied().find(|&g| !within_total(g)),
        (Some(saved), now_sel) => {
            let saved: std::collections::BTreeSet<usize> = saved.iter().copied().collect();
            match now_sel {
                Some(now) => now.iter().copied().find(|g| !saved.contains(g)),
                // "Every row group" is a subset only when the plan already
                // held every row group.
                None => (0..total.unwrap_or(0)).find(|g| !saved.contains(g)),
            }
        }
    }
}

/// One-line honesty note about what a plan does and does not pin for a
/// **remote** part (#511), emitted on save and on load alike.
///
/// A local file is pinned by path, size, mtime, row count and row-group
/// layout. A remote object has no mtime and no ETag here — the input layer
/// does not carry either past `connect()` — so its binding is URL,
/// Content-Length, row count and row-group layout. That catches every
/// replay that would corrupt the pyramid (the row-indexed winner table
/// cannot survive a row-count change), but it does not catch an object
/// rewritten in place with the identical size, row count and row-group
/// layout.
fn warn_remote_binding(source: &ConvertSource, flag: &str) {
    let remote = source
        .parts()
        .iter()
        .filter(|p| p.is_remote())
        .map(|p| p.display_name())
        .collect::<Vec<_>>();
    let Some(first) = remote.first() else {
        return;
    };
    log::warn!(
        "--{flag}: {} remote input part(s) (e.g. {first}) are pinned by URL, object size, \
         row count and row-group layout only — an object has no mtime here, so one rewritten \
         in place with the same size and row count would NOT be detected. Local parts also \
         pin mtime.",
        remote.len(),
    );
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

impl ConvertPlan {
    /// Build a plan from the winner tables the assignment just produced,
    /// plus the pass-1 scalars pass 2 and the report consume.
    pub fn from_winner_tables(
        tables: &WinnerTables,
        fingerprint: Fingerprint,
        rank_provenance: &RankingProvenance,
        ladder: Option<&EntryZoomSpec>,
        totals: PlanTotals,
    ) -> Result<ConvertPlan, ConvertError> {
        let coalesce = tables
            .coalesce_scratch
            .as_ref()
            .map(|s| {
                let wkb = s
                    .geoms
                    .iter()
                    .map(crate::wkb::geometry_to_wkb)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        ConvertError::InvalidConfig(format!(
                            "--save-plan: encoding a coalesce line failed: {e}"
                        ))
                    })?;
                Ok::<_, ConvertError>(CoalescePlan {
                    rows: s.rows.clone(),
                    wkb,
                    sort_keys: s.sort_keys.clone(),
                    groups: s.groups.clone(),
                })
            })
            .transpose()?;
        Ok(ConvertPlan {
            version: PLAN_FORMAT_VERSION,
            fingerprint,
            level_specs: tables.level_specs.clone(),
            counts: tables.counts.clone(),
            finest: tables.finest,
            min_levels: tables.min_levels.clone(),
            kinds: tables.kinds.clone(),
            carriers: tables.carriers.clone(),
            cluster_tables: tables.cluster_tables.clone(),
            coalesce,
            rank_provenance: rank_provenance.clone(),
            rank_plan: RankPlanProvenance::from_provenance(rank_provenance),
            ladder: ladder.map(|s| LadderProvenance {
                column: s.column.clone(),
                kind: format!("{:?}", s.kind),
            }),
            totals,
        })
    }

    /// Rebuild the winner tables pass 2 addresses. Consumes the plan's
    /// O(dataset) tables rather than cloning them.
    pub fn into_winner_tables(self) -> Result<WinnerTables, ConvertError> {
        let coalesce_scratch = self
            .coalesce
            .map(|c| {
                let geoms = c
                    .wkb
                    .iter()
                    .map(|w| crate::wkb::wkb_to_geometry(w))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        ConvertError::InvalidConfig(format!(
                            "--plan: decoding a coalesce line failed: {e}"
                        ))
                    })?;
                Ok::<_, ConvertError>(CoalesceScratch {
                    rows: c.rows,
                    geoms,
                    sort_keys: c.sort_keys,
                    groups: c.groups,
                })
            })
            .transpose()?;
        Ok(WinnerTables {
            level_specs: self.level_specs,
            cluster_tables: self.cluster_tables,
            kinds: self.kinds,
            coalesce_scratch,
            min_levels: self.min_levels,
            counts: self.counts,
            carriers: self.carriers,
            finest: self.finest,
        })
    }
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
        write_plan_file(path, &schema, &batch)
    }

    /// Read a plan back from `path`.
    ///
    /// The file is *verified* before it is decoded: magic bytes, then an
    /// xxh3-64 over the whole Arrow IPC payload. Only then does any byte
    /// reach an arrow decoder, and every structural check past that point
    /// reports through [`plan_err`] — a hostile or damaged plan is an error
    /// naming `--plan` and the path, never a panic (#512).
    pub fn load(path: &Path) -> Result<ConvertPlan, ConvertError> {
        let mut file = File::open(path).map_err(|e| plan_err(path, &format!("{e}")))?;
        let mut header = [0u8; PLAN_HEADER_LEN];
        file.read_exact(&mut header).map_err(|_| {
            plan_err(
                path,
                "is too short to be a tylertoo convert plan (no header)",
            )
        })?;
        if !header.starts_with(PLAN_MAGIC_PREFIX) {
            return Err(plan_err(
                path,
                "is not a tylertoo convert plan (bad magic bytes). Plans written by an \
                 older tylertoo must be re-created with --save-plan.",
            ));
        }
        // Version byte, read separately from the prefix so a FUTURE format is
        // named as such instead of being reported as corruption.
        let version = header[PLAN_MAGIC_PREFIX.len()];
        if version != PLAN_FORMAT_VERSION_BYTE {
            return Err(plan_err(
                path,
                &format!(
                    "is a tylertoo convert plan in format v{version}, but this tylertoo \
                     reads v{PLAN_FORMAT_VERSION_BYTE}. Re-create it with --save-plan."
                ),
            ));
        }
        let want = u64::from_le_bytes(
            header[PLAN_MAGIC.len()..]
                .try_into()
                .expect("header is 8 + 8 bytes"),
        );
        // Seek explicitly to the payload rather than relying on the cursor
        // the header read happened to leave behind (a `try_clone` shares the
        // file offset, so this worked only as a side effect of the
        // `read_exact` above). The writer states the same offset the same
        // way; an implicit, asymmetric version of it is one refactor away
        // from hashing the header too and failing every load.
        file.seek(SeekFrom::Start(PLAN_HEADER_LEN as u64))
            .map_err(|e| plan_err(path, &format!("reading it failed: {e}")))?;
        let got = hash_payload(BufReader::new(
            file.try_clone()
                .map_err(|e| plan_err(path, &format!("{e}")))?,
        ))
        .map_err(|e| plan_err(path, &format!("reading it failed: {e}")))?;
        if got != want {
            return Err(plan_err(
                path,
                &format!(
                    "is corrupt: the payload hashes to {got:016x} but the header records \
                     {want:016x}. The file was truncated, edited, or damaged in transit — \
                     re-create it with --save-plan."
                ),
            ));
        }
        let mut reader = FileReader::try_new(ipc_view(file), None)
            .map_err(|e| plan_err(path, &format!("the Arrow IPC payload is unreadable: {e}")))?;
        let schema = reader.schema();
        let meta_json = schema.metadata().get(PLAN_META_KEY).ok_or_else(|| {
            plan_err(
                path,
                &format!("is not a tylertoo convert plan (no {PLAN_META_KEY} metadata)"),
            )
        })?;
        let meta: PlanMeta = serde_json::from_str(meta_json)
            .map_err(|e| plan_err(path, &format!("its metadata block is unreadable: {e}")))?;
        if meta.version != PLAN_FORMAT_VERSION {
            return Err(plan_err(
                path,
                &format!(
                    "was written by plan format v{} but this tylertoo reads \
                     v{PLAN_FORMAT_VERSION}",
                    meta.version,
                ),
            ));
        }
        // The JSON block is user-reachable input too: `num_levels` sizes an
        // allocation and `finest`/`counts` index it, so bound them here
        // rather than trusting them into a `vec![_; n]`.
        if meta.num_levels != meta.level_specs.len() {
            return Err(plan_err(
                path,
                &format!(
                    "declares {} level(s) but carries {} level spec(s)",
                    meta.num_levels,
                    meta.level_specs.len(),
                ),
            ));
        }
        if meta.num_levels > super::convert::MAX_LEVELS {
            return Err(plan_err(
                path,
                &format!(
                    "declares {} level(s); at most {} are supported",
                    meta.num_levels,
                    super::convert::MAX_LEVELS,
                ),
            ));
        }
        let batch = reader
            .next()
            .transpose()
            .map_err(|e| plan_err(path, &format!("its record batch is unreadable: {e}")))?
            .ok_or_else(|| plan_err(path, "holds no record batch"))?;
        // Every section is one list VALUE of a single-row batch. A zero-row
        // batch is schema-valid and used to panic inside arrow at
        // `list.value(0)`.
        if batch.num_rows() != 1 {
            return Err(plan_err(
                path,
                &format!(
                    "holds a {}-row record batch; a plan is exactly one row",
                    batch.num_rows(),
                ),
            ));
        }

        let min_levels: Vec<u8> = u8_section(&batch, "min_levels", path)?.unwrap_or_default();
        let kinds = u8_section(&batch, "kinds", path)?
            .map(|codes| {
                codes
                    .into_iter()
                    .map(|c| kind_from_code(c, path))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;

        // #512 follow-up: `kinds` is addressed by the SAME row position as
        // `min_levels` (`finest.kinds[g]` in pass 2's batch fan-out), so a
        // checksum-valid plan whose `kinds` is short is an out-of-bounds index
        // waiting to happen — the checksum proves the bytes are the ones that
        // were written, never that they are consistent.
        //
        // The other sections were audited for the same hole and are already
        // closed: `carriers` is bounded by `meta.num_levels` in
        // `rebuild_carriers` and its row values are only ever `binary_search`ed
        // (`is_carrier`); the coalesce sections are length-matched against each
        // other in `rebuild_coalesce` and their row values key a `HashMap`; the
        // cluster sections are length-matched in `rebuild_cluster_tables` and
        // are likewise `HashMap`-keyed by row. Polygon `areas` never reach the
        // artifact — they are consumed into `carriers` before the plan is
        // built. `min_levels` and `kinds` are the only two indexed by raw row.
        if let Some(kinds) = &kinds {
            if kinds.len() != min_levels.len() {
                return Err(plan_err(
                    path,
                    &format!(
                        "does not match itself: section \"kinds\" holds {} row(s) but section \
                         \"min_levels\" holds {}. Every row-indexed section must cover the \
                         same input rows — re-create the plan with --save-plan.",
                        kinds.len(),
                        min_levels.len(),
                    ),
                ));
            }
        }

        let carrier_offsets = u64_section(&batch, "carrier_offsets", path)?.unwrap_or_default();
        let carrier_rows = u64_section(&batch, "carrier_rows", path)?.unwrap_or_default();
        let carriers = rebuild_carriers(&carrier_offsets, &carrier_rows, meta.num_levels, path)?;

        let cluster_tables = if meta.has_cluster {
            Some(rebuild_cluster_tables(&batch, &meta, path)?)
        } else {
            None
        };

        let coalesce = if meta.has_coalesce {
            Some(rebuild_coalesce(&batch, path)?)
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

/// Write one record batch to `path` as a plan artifact: the [`PLAN_MAGIC`]
/// header, the Arrow IPC payload, then the payload's checksum patched back
/// into the header.
///
/// Two-pass rather than buffering the payload in RAM: the artifact is
/// O(input rows) and `save` runs at pass 1's memory peak, so an extra whole
/// copy of it is exactly what a planet-scale run cannot afford.
fn write_plan_file(path: &Path, schema: &Schema, batch: &RecordBatch) -> Result<(), ConvertError> {
    let file = File::create(path).map_err(|e| save_plan_err(path, &format!("{e}")))?;
    let mut w = BufWriter::new(file);
    w.write_all(PLAN_MAGIC)
        .and_then(|()| w.write_all(&0u64.to_le_bytes()))
        .map_err(|e| save_plan_err(path, &format!("{e}")))?;
    // Arrow's own errors carry no path: `--save-plan /mnt/full/x.plan` used to
    // fail with a bare "No space left on device" naming neither the flag nor
    // the file. Every arrow step is prefixed the same way the io steps are.
    let arrow = |e: arrow_schema::ArrowError| save_plan_err(path, &format!("{e}"));
    let mut ipc = FileWriter::try_new(w, schema).map_err(arrow)?;
    ipc.write(batch).map_err(arrow)?;
    ipc.finish().map_err(arrow)?;
    let mut w = ipc.into_inner().map_err(arrow)?;
    w.flush()
        .map_err(|e| save_plan_err(path, &format!("{e}")))?;
    drop(w);

    let checksum = (|| -> std::io::Result<u64> {
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(PLAN_HEADER_LEN as u64))?;
        hash_payload(BufReader::new(f))
    })()
    .map_err(|e| save_plan_err(path, &format!("checksumming what was written failed: {e}")))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| save_plan_err(path, &format!("{e}")))?;
    f.seek(SeekFrom::Start(PLAN_MAGIC.len() as u64))
        .and_then(|_| f.write_all(&checksum.to_le_bytes()))
        .and_then(|()| f.flush())
        .map_err(|e| save_plan_err(path, &format!("writing the checksum failed: {e}")))?;
    Ok(())
}

/// Decode a geometry-kind byte. Unknown codes are **rejected**: silently
/// folding them to `Point` (#512) would reroute Q3 coalescing and the
/// polygon carriers on a byte the checksum had not yet caught.
fn kind_from_code(c: u8, path: &Path) -> Result<FeatureKind, ConvertError> {
    match c {
        0 => Ok(FeatureKind::Point),
        1 => Ok(FeatureKind::Line),
        2 => Ok(FeatureKind::Polygon),
        other => Err(plan_err(
            path,
            &format!("section \"kinds\" holds the unknown geometry-kind code {other}"),
        )),
    }
}

/// The single list value of `name`, or `None` when the section is absent.
fn section(
    batch: &RecordBatch,
    name: &str,
    path: &Path,
) -> Result<Option<arrow_array::ArrayRef>, ConvertError> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| plan_err(path, &format!("is missing section {name:?}")))?;
    let col = batch.column(idx);
    let list = col
        .as_any()
        .downcast_ref::<LargeListArray>()
        .ok_or_else(|| plan_err(path, &format!("section {name:?} has the wrong layout")))?;
    // `value(0)` on an empty list array indexes arrow's offset buffer out of
    // bounds and panics (#512). The caller guarantees a one-row batch; this
    // is the local guard that makes the indexing itself total.
    if list.is_empty() {
        return Err(plan_err(
            path,
            &format!("section {name:?} carries no list value"),
        ));
    }
    if list.is_null(0) {
        return Ok(None);
    }
    Ok(Some(list.value(0)))
}

macro_rules! typed_section {
    ($fn_name:ident, $arr:ty, $native:ty) => {
        /// A non-nullable typed section. Nulls are **rejected**: `values()`
        /// discards the null mask, so a nulled `min_level` would decode as
        /// level 0 — the coarsest level, i.e. drawn everywhere (#512).
        fn $fn_name(
            batch: &RecordBatch,
            name: &str,
            path: &Path,
        ) -> Result<Option<Vec<$native>>, ConvertError> {
            let Some(values) = section(batch, name, path)? else {
                return Ok(None);
            };
            let a = values.as_any().downcast_ref::<$arr>().ok_or_else(|| {
                plan_err(path, &format!("section {name:?} has the wrong value type"))
            })?;
            if a.null_count() > 0 {
                return Err(plan_err(
                    path,
                    &format!(
                        "section {name:?} holds {} null value(s); it must not be nullable",
                        a.null_count(),
                    ),
                ));
            }
            Ok(Some(a.values().to_vec()))
        }
    };
}

typed_section!(u8_section, UInt8Array, u8);
typed_section!(u64_section, UInt64Array, u64);
typed_section!(u32_section, UInt32Array, u32);
typed_section!(i64_section, Int64Array, i64);

/// A nullable `Float64` section, preserving nulls (a missing aggregate / a
/// missing sort key are both real values here).
fn f64_opt_section(
    batch: &RecordBatch,
    name: &str,
    path: &Path,
) -> Result<Option<Vec<Option<f64>>>, ConvertError> {
    let Some(values) = section(batch, name, path)? else {
        return Ok(None);
    };
    let a = values
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| plan_err(path, &format!("section {name:?} has the wrong value type")))?;
    Ok(Some(a.iter().collect()))
}

fn rebuild_carriers(
    offsets: &[u64],
    rows: &[u64],
    num_levels: usize,
    path: &Path,
) -> Result<Vec<Vec<usize>>, ConvertError> {
    let want = num_levels
        .checked_add(1)
        .ok_or_else(|| plan_err(path, "declares an impossible level count"))?;
    if offsets.len() != want {
        return Err(plan_err(
            path,
            &format!(
                "carrier offsets have {} entries but the plan has {num_levels} level(s)",
                offsets.len(),
            ),
        ));
    }
    let mut out = Vec::with_capacity(num_levels);
    for w in offsets.windows(2) {
        let (a, b) = (w[0] as usize, w[1] as usize);
        if a > b || b > rows.len() {
            return Err(plan_err(path, "carrier offsets are out of range"));
        }
        out.push(rows[a..b].iter().map(|&r| r as usize).collect());
    }
    Ok(out)
}

fn rebuild_cluster_tables(
    batch: &RecordBatch,
    meta: &PlanMeta,
    path: &Path,
) -> Result<ClusterTables, ConvertError> {
    let levels = u32_section(batch, "cluster_level", path)?.unwrap_or_default();
    let rows = u64_section(batch, "cluster_row", path)?.unwrap_or_default();
    let counts = i64_section(batch, "cluster_point_count", path)?.unwrap_or_default();
    let aggs = f64_opt_section(batch, "cluster_agg", path)?.unwrap_or_default();
    let stride = meta.cluster_agg_stride;
    if levels.len() != rows.len() || levels.len() != counts.len() {
        return Err(plan_err(path, "cluster sections have mismatched lengths"));
    }
    // `levels.len() * stride` with a JSON-supplied stride overflowed in debug
    // and wrapped past this very length guard in release (#512). Checked, a
    // hostile stride simply cannot name a length the section has.
    let want = levels.len().checked_mul(stride).ok_or_else(|| {
        plan_err(
            path,
            &format!(
                "declares {} aggregate(s) per cluster entry over {} entries, which is not a \
                 possible section length",
                stride,
                levels.len(),
            ),
        )
    })?;
    if aggs.len() != want {
        return Err(plan_err(
            path,
            &format!(
                "cluster aggregate section holds {} value(s) but the plan declares {} entries \
                 × {stride} aggregate(s) = {want}",
                aggs.len(),
                levels.len(),
            ),
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
            plan_err(
                path,
                &format!(
                    "a cluster entry names level {level}, but the plan has {} level(s)",
                    meta.num_levels,
                ),
            )
        })?;
        table.insert(
            row as usize,
            ClusterEntry {
                point_count,
                // In range: `aggs.len() == levels.len() * stride` exactly,
                // and `i < levels.len()`.
                aggregates: aggs[i * stride..(i + 1) * stride].to_vec(),
            },
        );
    }
    Ok(tables)
}

fn rebuild_coalesce(batch: &RecordBatch, path: &Path) -> Result<CoalescePlan, ConvertError> {
    let rows: Vec<usize> = u64_section(batch, "coalesce_row", path)?
        .unwrap_or_default()
        .into_iter()
        .map(|r| r as usize)
        .collect();
    let sort_keys = f64_opt_section(batch, "coalesce_sort_key", path)?.unwrap_or_default();
    let groups = u32_section(batch, "coalesce_group", path)?;
    let wkb = match section(batch, "coalesce_wkb", path)? {
        None => Vec::new(),
        Some(values) => {
            let a = values
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| {
                    plan_err(path, "section \"coalesce_wkb\" has the wrong value type")
                })?;
            if a.null_count() > 0 {
                return Err(plan_err(
                    path,
                    &format!(
                        "section \"coalesce_wkb\" holds {} null geometry/geometries",
                        a.null_count(),
                    ),
                ));
            }
            (0..a.len()).map(|i| a.value(i).to_vec()).collect()
        }
    };
    if wkb.len() != rows.len() || sort_keys.len() != rows.len() {
        return Err(plan_err(path, "coalesce sections have mismatched lengths"));
    }
    if let Some(g) = &groups {
        if g.len() != rows.len() {
            return Err(plan_err(path, "coalesce sections have mismatched lengths"));
        }
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
                num_rows: Some(9),
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

    /// #511: the row count is the binding that survives a remote input, where
    /// `fs::metadata` sees nothing. Both directions are refused by name —
    /// fewer rows (which would silently truncate the pyramid) and more rows
    /// (which would index the winner table out of bounds).
    #[test]
    fn plan_fingerprint_rejects_changed_row_count() {
        let saved = tiny_fingerprint();

        for rows in [Some(8), Some(10), None] {
            let mut changed = tiny_fingerprint();
            changed.inputs[0].num_rows = rows;
            let err = touched_err(&saved, &changed);
            assert!(err.contains("row count"), "names the field: {err}");
            assert!(err.contains("/tmp/in.parquet"), "names the part: {err}");
        }

        let mut regrouped = tiny_fingerprint();
        regrouped.inputs[0].row_groups_total = Some(4);
        let err = touched_err(&saved, &regrouped);
        assert!(err.contains("row group count"), "names the field: {err}");
    }

    /// #511: a remote part carries neither a local size nor an mtime, so the
    /// row count / row-group layout IS the whole content binding — and it
    /// must still refuse a swapped object.
    #[test]
    fn plan_fingerprint_binds_remote_shaped_part_by_row_count() {
        let remote = |rows: i64, byte_len: Option<u64>| Fingerprint {
            tylertoo_version: env!("CARGO_PKG_VERSION").to_string(),
            options: BTreeMap::new(),
            inputs: vec![InputFingerprint {
                path: "s3://bucket/roads.parquet".to_string(),
                // The metadata-unavailable branch: no local stat, so mtime is
                // absent and the size is whatever the object store reported.
                byte_len,
                mtime_nanos: None,
                row_groups: None,
                row_groups_total: Some(12),
                num_rows: Some(rows),
            }],
        };
        // Same URL, same size, same row groups, different contents: the
        // pre-#511 fingerprint accepted this and replayed into corruption.
        let err = remote(1_000_000, Some(4096))
            .verify(&remote(999_999, Some(4096)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("row count"), "names the field: {err}");
        assert!(err.contains("1000000") && err.contains("999999"), "{err}");

        // And the size, when the object store does report one.
        let err = remote(1_000_000, Some(4096))
            .verify(&remote(1_000_000, Some(8192)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("byte_len"), "{err}");

        // Unchanged verifies, with no local metadata anywhere.
        remote(1_000_000, None)
            .verify(&remote(1_000_000, None))
            .unwrap();
    }

    fn touched_err(saved: &Fingerprint, current: &Fingerprint) -> String {
        saved.verify(current).unwrap_err().to_string()
    }

    /// The #498 relaxation, stated as a pair: a shard may NARROW the plan's
    /// row-group selection and may not widen it. Outside shard mode the rule
    /// stays an equality, so the narrowing that is fine for a shard is still
    /// refused for an ordinary `--plan` replay.
    #[test]
    fn shard_mode_accepts_a_subset_of_the_plans_row_groups_and_nothing_else() {
        let with = |groups: Option<Vec<usize>>| {
            let mut fp = tiny_fingerprint();
            fp.inputs[0].row_groups = groups;
            fp
        };
        let saved = with(Some(vec![0, 2, 3]));

        // Narrower: accepted under the shard rule, refused under the
        // ordinary one.
        let narrower = with(Some(vec![2]));
        saved
            .verify_with(&narrower, SelectionRule::SubsetAllowed)
            .expect("a shard may read a subset of the plan's row groups");
        let err = saved
            .verify_with(&narrower, SelectionRule::Identical)
            .unwrap_err()
            .to_string();
        assert!(err.contains("selected row groups"), "{err}");

        // Identical: accepted under both.
        saved
            .verify_with(&saved, SelectionRule::SubsetAllowed)
            .unwrap();

        // Wider: refused even under the shard rule, naming the row group the
        // plan has no winner byte for.
        let wider = with(Some(vec![0, 1, 2, 3]));
        let err = saved
            .verify_with(&wider, SelectionRule::SubsetAllowed)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("row group 1") && err.contains("never saw"),
            "must name the offending group: {err}"
        );

        // "Every row group" is a subset only of a plan that held every one.
        let err = saved
            .verify_with(&with(None), SelectionRule::SubsetAllowed)
            .unwrap_err()
            .to_string();
        assert!(err.contains("row group 1"), "{err}");
        with(None)
            .verify_with(&with(Some(vec![0, 1, 2])), SelectionRule::SubsetAllowed)
            .expect("a plan over everything accepts any subset");
    }

    #[test]
    fn selection_excess_is_a_subset_test() {
        // saved = all, now = some: fine while inside the total.
        assert_eq!(selection_excess(None, Some(&[0, 2]), Some(3)), None);
        assert_eq!(selection_excess(None, Some(&[0, 7]), Some(3)), Some(7));
        // saved = some, now = subset / superset.
        assert_eq!(selection_excess(Some(&[0, 2]), Some(&[2]), Some(3)), None);
        assert_eq!(
            selection_excess(Some(&[0, 2]), Some(&[1]), Some(3)),
            Some(1)
        );
        // saved = some, now = all.
        assert_eq!(selection_excess(Some(&[0, 1, 2]), None, Some(3)), None);
        assert_eq!(selection_excess(Some(&[0, 2]), None, Some(3)), Some(1));
    }

    /// The re-addressing itself, on a hand-built domain: the plan's stream is
    /// row groups `[0, 1, 2]` of 3, 4 and 2 rows; the shard reads `[0, 2]`.
    /// Every kept row must carry the winner byte it had in the plan, and
    /// every side-table key must follow it.
    #[test]
    fn shard_row_mapping_moves_values_without_recomputing_them() {
        // Plan stream: rows 0..2 (group 0), 3..6 (group 1), 7..8 (group 2).
        // Shard keeps groups 0 and 2, so plan rows 0,1,2,7,8 become shard
        // rows 0,1,2,3,4.
        let runs = vec![
            KeptRun {
                plan_base: 0,
                shard_base: 0,
                len: 3,
            },
            KeptRun {
                plan_base: 7,
                shard_base: 3,
                len: 2,
            },
        ];
        assert_eq!(shard_row(&runs, 0), Some(0));
        assert_eq!(shard_row(&runs, 2), Some(2));
        // Rows of the pruned group map nowhere.
        for row in 3..7 {
            assert_eq!(shard_row(&runs, row), None, "plan row {row}");
        }
        assert_eq!(shard_row(&runs, 7), Some(3));
        assert_eq!(shard_row(&runs, 8), Some(4));
        // Past the end.
        assert_eq!(shard_row(&runs, 9), None);
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

    // ------------------------------------------------------------------
    // #512: hostile / damaged plan files. A plan travels between machines
    // and is named by a user-supplied `--plan PATH`, so the reader is held
    // to the bar #417/#489 set for PMTiles reading: hostile input produces
    // an ERROR naming the flag and the path, never a panic.
    // ------------------------------------------------------------------

    /// Re-open a saved plan's IPC payload, so a test can forge a hostile
    /// variant of a *real* artifact.
    fn reopen(path: &Path) -> (Arc<Schema>, RecordBatch) {
        let mut r = FileReader::try_new(ipc_view(File::open(path).unwrap()), None).unwrap();
        let schema = r.schema();
        let batch = r.next().unwrap().unwrap();
        (schema, batch)
    }

    /// The forge: write `columns` under `meta_json` with a *valid* header
    /// and checksum, so the load under test reaches the decoder rather than
    /// tripping the corruption guard first.
    fn forge(path: &Path, meta_json: &str, columns: Vec<arrow_array::ArrayRef>) {
        let schema = Arc::new(plan_schema(meta_json.to_string()));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        write_plan_file(path, &schema, &batch).unwrap();
    }

    /// The saved plan's metadata JSON, as a mutable value.
    fn meta_of(schema: &Schema) -> serde_json::Value {
        serde_json::from_str(schema.metadata().get(PLAN_META_KEY).unwrap()).unwrap()
    }

    fn load_err(path: &Path) -> String {
        let err = ConvertPlan::load(path).unwrap_err().to_string();
        assert!(err.contains("--plan"), "names the flag: {err}");
        assert!(
            err.contains(&path.display().to_string()),
            "names the path: {err}"
        );
        err
    }

    /// Class (a): a schema-valid plan whose record batch has ZERO rows used
    /// to panic inside arrow at `list.value(0)`.
    #[test]
    fn plan_rejects_zero_row_batch() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = tiny_plan();
        plan.save(f.path()).unwrap();
        let (schema, _) = reopen(f.path());
        let empty = RecordBatch::new_empty(schema.clone());
        write_plan_file(f.path(), &schema, &empty).unwrap();
        let err = load_err(f.path());
        assert!(err.contains("0-row"), "{err}");
    }

    /// Class (b): `levels.len() * cluster_agg_stride` with a stride read
    /// from the JSON block overflowed in debug and wrapped past the length
    /// guard into an out-of-range slice in release.
    #[test]
    fn plan_rejects_hostile_cluster_agg_stride() {
        let f = tempfile::NamedTempFile::new().unwrap();
        tiny_plan().save(f.path()).unwrap();
        let (schema, batch) = reopen(f.path());
        let columns = batch.columns().to_vec();

        // usize::MAX × 2 entries: the multiplication itself overflows.
        let mut meta = meta_of(&schema);
        meta["cluster_agg_stride"] = serde_json::json!(usize::MAX);
        forge(
            f.path(),
            &serde_json::to_string(&meta).unwrap(),
            columns.clone(),
        );
        let err = load_err(f.path());
        assert!(err.contains("not a possible section length"), "{err}");

        // Large but non-overflowing: caught by the length comparison.
        let mut meta = meta_of(&schema);
        meta["cluster_agg_stride"] = serde_json::json!(1_000_000u64);
        forge(f.path(), &serde_json::to_string(&meta).unwrap(), columns);
        let err = load_err(f.path());
        assert!(err.contains("cluster aggregate section"), "{err}");
    }

    /// Class (c): a single flipped byte anywhere in the payload used to be
    /// accepted (121/203 positions), quietly wrong, or panic inside arrow's
    /// buffer/IPC decoders (11/203). The checksum turns every one of those
    /// into the same named error, before a byte reaches a decoder.
    #[test]
    fn plan_rejects_a_flipped_byte() {
        let f = tempfile::NamedTempFile::new().unwrap();
        tiny_plan().save(f.path()).unwrap();
        let original = std::fs::read(f.path()).unwrap();

        // The whole sweep the issue ran, deterministically: flip bit 0 of
        // EVERY byte of the payload and require the same named error each
        // time. Nothing is accepted, and nothing panics.
        for at in PLAN_HEADER_LEN..original.len() {
            let mut bytes = original.clone();
            bytes[at] ^= 0x01;
            std::fs::write(f.path(), &bytes).unwrap();
            let err = ConvertPlan::load(f.path()).unwrap_err().to_string();
            assert!(err.contains("is corrupt"), "byte {at}: {err}");
        }

        // The checksum field itself, and the magic.
        for at in PLAN_MAGIC.len()..PLAN_HEADER_LEN {
            let mut bytes = original.clone();
            bytes[at] ^= 0xff;
            std::fs::write(f.path(), &bytes).unwrap();
            assert!(load_err(f.path()).contains("is corrupt"), "byte {at}");
        }
        // The magic PREFIX: not a plan at all.
        for at in 0..PLAN_MAGIC_PREFIX.len() {
            let mut bytes = original.clone();
            bytes[at] ^= 0xff;
            std::fs::write(f.path(), &bytes).unwrap();
            assert!(load_err(f.path()).contains("bad magic"), "byte {at}");
        }
        // ...and the version byte that closes it: a plan in a format this
        // build does not read, reported as the version skew it is rather
        // than as corruption.
        let at = PLAN_MAGIC_PREFIX.len();
        let mut bytes = original.clone();
        bytes[at] = PLAN_FORMAT_VERSION_BYTE + 1;
        std::fs::write(f.path(), &bytes).unwrap();
        let err = load_err(f.path());
        assert!(
            err.contains(&format!("format v{}", PLAN_FORMAT_VERSION_BYTE + 1)),
            "names the version it found: {err}"
        );
        assert!(!err.contains("bad magic"), "{err}");

        // Truncation, and a file that is not a plan at all.
        std::fs::write(f.path(), &original[..PLAN_HEADER_LEN + 8]).unwrap();
        assert!(load_err(f.path()).contains("is corrupt"));
        std::fs::write(f.path(), b"hello").unwrap();
        assert!(load_err(f.path()).contains("too short"));
        std::fs::write(f.path(), b"").unwrap();
        assert!(load_err(f.path()).contains("too short"));

        // Restored, it loads again — the guard is not simply refusing.
        std::fs::write(f.path(), &original).unwrap();
        assert_eq!(ConvertPlan::load(f.path()).unwrap(), tiny_plan());
    }

    /// Class (d): an unknown geometry-kind byte silently decoded as `Point`,
    /// rerouting Q3 coalescing and the polygon carriers.
    #[test]
    fn plan_rejects_unknown_kind_code() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = tiny_plan();
        plan.save(f.path()).unwrap();
        let (schema, batch) = reopen(f.path());
        let mut columns = batch.columns().to_vec();
        let mut b = LargeListBuilder::new(UInt8Builder::new());
        b.values().append_slice(&[0, 1, 2, 7, 1, 1, 2, 2, 1]);
        b.append(true);
        columns[schema.index_of("kinds").unwrap()] = Arc::new(b.finish());
        forge(
            f.path(),
            &serde_json::to_string(&meta_of(&schema)).unwrap(),
            columns,
        );
        let err = load_err(f.path());
        assert!(err.contains("unknown geometry-kind code 7"), "{err}");
    }

    /// Class (e): a plan whose ROW-INDEXED sections disagree with each other.
    ///
    /// The checksum (#512) proves the bytes are the ones that were written;
    /// it proves nothing about whether they are consistent, and anyone who
    /// can forge a plan can re-checksum it. `kinds` is addressed by the same
    /// row position as `min_levels` (`finest.kinds[g]` in pass 2's batch
    /// fan-out), so a short `kinds` was an out-of-bounds index — a PANIC —
    /// reached through a fully valid header.
    #[test]
    fn plan_rejects_row_sections_of_disagreeing_length() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let plan = tiny_plan();
        assert_eq!(
            plan.kinds.as_ref().unwrap().len(),
            plan.min_levels.len(),
            "precondition: the honest plan agrees with itself"
        );
        plan.save(f.path()).unwrap();
        let (schema, batch) = reopen(f.path());
        let mut columns = batch.columns().to_vec();

        // One row short of `min_levels`, checksum repaired by `forge`.
        let mut b = LargeListBuilder::new(UInt8Builder::new());
        b.values().append_slice(&[0, 1, 2, 1, 1, 1, 2, 2]);
        b.append(true);
        columns[schema.index_of("kinds").unwrap()] = Arc::new(b.finish());
        forge(
            f.path(),
            &serde_json::to_string(&meta_of(&schema)).unwrap(),
            columns,
        );
        let err = load_err(f.path());
        assert!(err.contains("does not match itself"), "{err}");
        assert!(
            err.contains("\"kinds\" holds 8") && err.contains("\"min_levels\" holds 9"),
            "names both sections and both lengths: {err}"
        );
    }

    /// Class (d'): `typed_section!` read `values()`, which discards the null
    /// mask — a nulled `min_level` decoded as level 0, the coarsest level,
    /// i.e. the feature drawn everywhere.
    #[test]
    fn plan_rejects_nulls_in_a_non_nullable_section() {
        let f = tempfile::NamedTempFile::new().unwrap();
        tiny_plan().save(f.path()).unwrap();
        let (schema, batch) = reopen(f.path());
        let mut columns = batch.columns().to_vec();
        let mut b = LargeListBuilder::new(UInt8Builder::new());
        for (i, v) in [0u8, 1, 2, 255, 1, 0, 2, 2, 1].into_iter().enumerate() {
            if i == 3 {
                b.values().append_null();
            } else {
                b.values().append_value(v);
            }
        }
        b.append(true);
        columns[schema.index_of("min_levels").unwrap()] = Arc::new(b.finish());
        forge(
            f.path(),
            &serde_json::to_string(&meta_of(&schema)).unwrap(),
            columns,
        );
        let err = load_err(f.path());
        assert!(err.contains("min_levels") && err.contains("null"), "{err}");
    }

    /// The JSON block sizes allocations and indexes them; a hostile level
    /// count must not reach `vec![_; n]`.
    #[test]
    fn plan_rejects_hostile_level_count() {
        let f = tempfile::NamedTempFile::new().unwrap();
        tiny_plan().save(f.path()).unwrap();
        let (schema, batch) = reopen(f.path());
        let columns = batch.columns().to_vec();

        let mut meta = meta_of(&schema);
        meta["num_levels"] = serde_json::json!(usize::MAX);
        forge(
            f.path(),
            &serde_json::to_string(&meta).unwrap(),
            columns.clone(),
        );
        let err = load_err(f.path());
        assert!(err.contains("level spec(s)"), "{err}");

        // Consistent with `level_specs`, but past the 255-level ceiling.
        let mut meta = meta_of(&schema);
        let specs: Vec<(f64, Option<u8>)> = (0..300).map(|i| (1.0 + f64::from(i), None)).collect();
        meta["num_levels"] = serde_json::json!(300);
        meta["level_specs"] = serde_json::to_value(&specs).unwrap();
        forge(f.path(), &serde_json::to_string(&meta).unwrap(), columns);
        let err = load_err(f.path());
        assert!(err.contains("at most 255"), "{err}");
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

    /// A grid of points: enough for a multi-level pyramid with real
    /// thinning and a non-trivial winner table.
    fn point_fixture() -> Vec<Option<geo::Geometry<f64>>> {
        use geo::Point;

        let mut geoms: Vec<Option<geo::Geometry<f64>>> = Vec::new();
        for i in 0..24 {
            for j in 0..24 {
                let x = -20.0 + i as f64 * 1.7;
                let y = -15.0 + j as f64 * 1.3;
                geoms.push(Some(geo::Geometry::Point(Point::new(x, y))));
            }
        }
        // One skipped row, so the UNASSIGNED sentinel is exercised too.
        geoms.push(None);
        geoms
    }

    /// Squares of widely differing size: drives the visibility gate, the
    /// simplify cascade, and the tiny-polygon carrier accumulator (#384).
    fn polygon_fixture() -> Vec<Option<geo::Geometry<f64>>> {
        use geo::{Coord, LineString, Polygon};

        let mut geoms: Vec<Option<geo::Geometry<f64>>> = Vec::new();
        for i in 0..14 {
            for j in 0..14 {
                let x = -30.0 + i as f64 * 3.1;
                let y = -20.0 + j as f64 * 2.7;
                let w = 0.02 + ((i * 14 + j) % 11) as f64 * 0.24;
                geoms.push(Some(geo::Geometry::Polygon(Polygon::new(
                    LineString(vec![
                        Coord { x, y },
                        Coord { x: x + w, y },
                        Coord { x: x + w, y: y + w },
                        Coord { x, y: y + w },
                        Coord { x, y },
                    ]),
                    vec![],
                ))));
            }
        }
        geoms.push(None);
        geoms
    }

    /// **The oracle for the whole design**: a convert that writes a plan and
    /// a convert that replays it must produce byte-identical output — both
    /// the overview GeoParquet and the PMTiles archive exported from it.
    ///
    /// If the artifact were missing anything pass 2, the writer, or the
    /// export reads out of pass 1, these bytes would differ.
    ///
    /// The two fixtures are deliberately single-geometry-type. A MIXED input
    /// makes tylertoo's own output non-reproducible run to run: the
    /// GeoParquet `geo` metadata's `geometry_types` array is built from an
    /// unordered set upstream, so `["Point","Polygon"]` and
    /// `["Polygon","Point"]` alternate between runs of the SAME conversion.
    /// That is a pre-existing upstream ordering bug, unrelated to the plan
    /// artifact, and excluding it here keeps this test a test of the plan.
    /// Touching lines, so the Q3 coalescing scratch (decoded geometries,
    /// sort keys, class groups) actually has to survive the artifact.
    fn line_fixture() -> Vec<Option<geo::Geometry<f64>>> {
        use geo::{Coord, LineString};

        let mut geoms: Vec<Option<geo::Geometry<f64>>> = Vec::new();
        for i in 0..18 {
            let y = -20.0 + i as f64 * 2.3;
            for seg in 0..6 {
                let x = -30.0 + seg as f64 * 4.0;
                geoms.push(Some(geo::Geometry::LineString(LineString(vec![
                    Coord { x, y },
                    Coord {
                        x: x + 2.0,
                        y: y + 0.4,
                    },
                    Coord { x: x + 4.0, y },
                ]))));
            }
        }
        geoms.push(None);
        geoms
    }

    #[test]
    fn convert_with_saved_plan_is_byte_identical() {
        use super::super::convert::LevelPlan;

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        type Case = (
            &'static str,
            Vec<Option<geo::Geometry<f64>>>,
            ConvertOptions,
        );
        let cases: Vec<Case> = vec![
            ("points", point_fixture(), base.clone()),
            ("polygons", polygon_fixture(), base.clone()),
            // Lines exercise the coalescing scratch the plan carries as WKB.
            ("lines", line_fixture(), base.clone()),
            // Clustering exercises the per-level cluster tables.
            (
                "points+cluster",
                point_fixture(),
                ConvertOptions {
                    cluster: true,
                    ..base.clone()
                },
            ),
        ];
        for (name, geoms, opts) in &cases {
            assert_plan_replay_is_byte_identical(name, geoms, opts);
        }
    }

    fn assert_plan_replay_is_byte_identical(
        name: &str,
        geoms: &[Option<geo::Geometry<f64>>],
        base: &ConvertOptions,
    ) {
        use super::super::convert::convert_to_overviews;
        use super::super::export::{export_pmtiles, ExportOptions};
        use super::super::testutil::write_input;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        write_input(&input, geoms, true, None);

        let plan_path = dir.path().join("convert.plan");

        // Run A: compute pass 1 + the assignment, and persist them.
        let out_a = dir.path().join("a.parquet");
        let report_a = convert_to_overviews(
            &input,
            &out_a,
            &ConvertOptions {
                save_plan: Some(plan_path.clone()),
                ..base.clone()
            },
        )
        .unwrap();
        let plan_bytes = std::fs::metadata(&plan_path).unwrap().len();
        eprintln!(
            "[plan] {name}: {} input geometries -> {plan_bytes} byte artifact",
            geoms.len()
        );
        // The artifact is O(input rows), not O(geometry): one winner byte per
        // row plus the small side tables (the line scratch aside).
        assert!(plan_bytes > 0, "{name}: --save-plan wrote the artifact");

        // Run B: replay it. Pass 1 and the assignment never run.
        let out_b = dir.path().join("b.parquet");
        let report_b = convert_to_overviews(
            &input,
            &out_b,
            &ConvertOptions {
                plan: Some(plan_path.clone()),
                ..base.clone()
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&out_a).unwrap(),
            std::fs::read(&out_b).unwrap(),
            "{name}: the overview GeoParquet must be byte-identical"
        );
        assert_eq!(report_a.input_features, report_b.input_features, "{name}");
        assert_eq!(report_a.total_rows, report_b.total_rows, "{name}");
        assert_eq!(report_a.total_vertices, report_b.total_vertices, "{name}");
        assert_eq!(report_a.levels.len(), report_b.levels.len(), "{name}");
        assert_eq!(
            report_a.antimeridian_suspect_features, report_b.antimeridian_suspect_features,
            "{name}"
        );
        assert!(report_a.total_rows > 0, "{name}: the fixture produced rows");

        // ...and the archive exported from each is byte-identical too.
        let pm_a = dir.path().join("a.pmtiles");
        let pm_b = dir.path().join("b.pmtiles");
        let export = ExportOptions {
            layer_name: "plan".to_string(),
            ..Default::default()
        };
        export_pmtiles(&out_a, &pm_a, &export).unwrap();
        export_pmtiles(&out_b, &pm_b, &export).unwrap();
        assert_eq!(
            std::fs::read(&pm_a).unwrap(),
            std::fs::read(&pm_b).unwrap(),
            "{name}: the exported PMTiles archive must be byte-identical"
        );
    }

    /// Forge a plan that is already on disk: truncate the named row-indexed
    /// sections by one row, optionally dropping `totals.n_rows` to match, and
    /// repair the header checksum. The result is a plan any reader accepts as
    /// intact — which is the whole point: a checksum is an integrity check,
    /// not a consistency check, and anyone who can edit a plan can re-compute
    /// it.
    fn forge_shorter_plan(path: &Path, sections: &[&str], fix_totals: bool) {
        let (schema, batch) = reopen(path);
        let mut columns = batch.columns().to_vec();
        for name in sections {
            let idx = schema.index_of(name).unwrap();
            let values = u8_section(&batch, name, path).unwrap().unwrap();
            let mut b = LargeListBuilder::new(UInt8Builder::new());
            b.values().append_slice(&values[..values.len() - 1]);
            b.append(true);
            columns[idx] = Arc::new(b.finish());
        }
        let mut meta = meta_of(&schema);
        if fix_totals {
            let n = meta["totals"]["n_rows"].as_u64().unwrap();
            meta["totals"]["n_rows"] = serde_json::json!(n - 1);
        }
        forge(path, &serde_json::to_string(&meta).unwrap(), columns);
    }

    /// The review's first forge probe, end to end: a checksum-VALID plan
    /// whose `kinds` is one row short of `min_levels`, replayed with NO
    /// pruning. Pass 2's batch fan-out does `finest.kinds[g]` for every row
    /// it reads, so this used to be an out-of-bounds PANIC on the last row —
    /// reached through a header that verified perfectly.
    #[test]
    fn convert_with_forged_short_kinds_is_refused() {
        use super::super::convert::{convert_to_overviews, LevelPlan};
        use super::super::testutil::write_input;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        // Lines: coalescing is what puts `kinds` on the pass-2 hot path.
        write_input(&input, &line_fixture(), true, None);
        let plan_path = dir.path().join("convert.plan");
        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 7,
            },
            ..Default::default()
        };
        convert_to_overviews(
            &input,
            dir.path().join("a.parquet"),
            &ConvertOptions {
                save_plan: Some(plan_path.clone()),
                ..base.clone()
            },
        )
        .unwrap();

        forge_shorter_plan(&plan_path, &["kinds"], false);

        let err = convert_to_overviews(
            &input,
            dir.path().join("b.parquet"),
            &ConvertOptions {
                plan: Some(plan_path),
                ..base
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--plan"), "names the flag: {err}");
        assert!(err.contains("does not match"), "{err}");
        assert!(err.contains("kinds"), "names the section: {err}");
    }

    /// The review's second forge probe: the same class of forgery under
    /// `--bbox`. The dataset-level row-domain check used to be gated on
    /// `unpruned_total_rows()`, which gives up the moment ANY part is
    /// row-group pruned — so the one structural check between a forged plan
    /// and an out-of-bounds index in pass 2 simply did not run under
    /// `--bbox` / `--filter`. The identical forgery WITHOUT a bbox was caught
    /// cleanly, which is what made it a hole rather than a gap.
    #[test]
    fn convert_with_forged_truncated_tables_is_refused_under_bbox() {
        use super::super::convert::{convert_to_overviews, LevelPlan};
        use super::super::testutil::write_input;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        write_input(&input, &point_fixture(), true, None);
        let plan_path = dir.path().join("convert.plan");
        // A bbox that keeps every row: pruning is what disabled the check,
        // not the rows it removes.
        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 7,
            },
            bbox: Some([-180.0, -90.0, 180.0, 90.0]),
            ..Default::default()
        };
        convert_to_overviews(
            &input,
            dir.path().join("a.parquet"),
            &ConvertOptions {
                save_plan: Some(plan_path.clone()),
                ..base.clone()
            },
        )
        .unwrap();
        let saved = ConvertPlan::load(&plan_path).unwrap();
        assert!(
            saved.fingerprint.inputs[0].row_groups.is_some(),
            "precondition: --bbox really did record a row-group selection"
        );

        // Truncate BOTH row-indexed sections and the declared row total, so
        // every other consistency check still passes and only the row-domain
        // check can catch it.
        let mut sections = vec!["min_levels"];
        if saved.kinds.is_some() {
            sections.push("kinds");
        }
        forge_shorter_plan(&plan_path, &sections, true);
        let forged =
            ConvertPlan::load(&plan_path).expect("the forgery loads: it is self-consistent");
        assert_eq!(forged.min_levels.len(), saved.min_levels.len() - 1);

        let err = convert_to_overviews(
            &input,
            dir.path().join("b.parquet"),
            &ConvertOptions {
                plan: Some(plan_path),
                ..base
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--plan"), "names the flag: {err}");
        assert!(err.contains("does not match"), "{err}");
        assert!(
            err.contains(&format!("{} winner-table", saved.min_levels.len() - 1)),
            "names what the plan holds: {err}"
        );
        assert!(
            err.contains(&format!("{} row(s)", saved.min_levels.len())),
            "names what this run will read: {err}"
        );
    }

    /// Replaying a plan against a *changed* input is refused, naming the
    /// field — the end-to-end form of the fingerprint unit tests.
    #[test]
    fn convert_with_stale_plan_is_refused() {
        use super::super::convert::{convert_to_overviews, LevelPlan};
        use super::super::testutil::write_input;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        write_input(&input, &point_fixture(), true, None);
        let plan_path = dir.path().join("convert.plan");
        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 7,
            },
            ..Default::default()
        };
        convert_to_overviews(
            &input,
            dir.path().join("a.parquet"),
            &ConvertOptions {
                save_plan: Some(plan_path.clone()),
                ..base.clone()
            },
        )
        .unwrap();

        // #511's probe, both directions. Rewrite the input at the SAME path
        // with FEWER rows: `min_levels` is indexed by row position, so a
        // replay would silently truncate the pyramid. Then with MORE rows,
        // which used to panic with an out-of-bounds index in pass 2. The
        // error must name the row count, because that is the one term of the
        // fingerprint a remote input also has (size/mtime do not survive an
        // `s3://` display name).
        let replay = |input: &std::path::Path, out: &str| {
            convert_to_overviews(
                input,
                dir.path().join(out),
                &ConvertOptions {
                    plan: Some(plan_path.clone()),
                    ..base.clone()
                },
            )
            .unwrap_err()
            .to_string()
        };

        write_input(&input, &point_fixture()[..100], true, None);
        let err = replay(&input, "b.parquet");
        assert!(
            err.contains("row count"),
            "names the offending field: {err}"
        );
        assert!(err.contains("100"), "names the new row count: {err}");

        let mut more = point_fixture();
        more.extend(point_fixture());
        write_input(&input, &more, true, None);
        let err = replay(&input, "b2.parquet");
        assert!(
            err.contains("row count"),
            "names the offending field: {err}"
        );

        // Restore the original input for the options check below.
        write_input(&input, &point_fixture(), true, None);

        // A changed thinning knob is refused by name as well.
        let mut thinned = base.clone();
        thinned.assign.point_thinning *= 2.0;
        thinned.plan = Some(plan_path);
        let err = convert_to_overviews(&input, dir.path().join("c.parquet"), &thinned)
            .unwrap_err()
            .to_string();
        assert!(err.contains("option "), "names the offending option: {err}");
    }

    /// The two flags are opposite ends of one run and cannot be combined.
    #[test]
    fn save_plan_and_plan_are_mutually_exclusive() {
        use super::super::convert::convert_to_overviews;

        let dir = tempfile::tempdir().unwrap();
        let err = convert_to_overviews(
            dir.path().join("nope.parquet"),
            dir.path().join("out.parquet"),
            &ConvertOptions {
                save_plan: Some(dir.path().join("p")),
                plan: Some(dir.path().join("p")),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
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
