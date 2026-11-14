//! One conservative disk footprint per conversion, grouped by destination volume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::input_set::{ConvertSource, RowGroupSelection};

use super::convert::{ConvertError, ConvertOptions};
use super::level::{MemoryProfile, Mode};
use super::memory::MemoryBudget;
use super::pipeline::{spill_bytes_per_arrow_byte, spill_codec};
use super::stream::select_row_groups_streaming;
use super::tiles::SpaceRemedies;

const MARGIN_DENOM: u64 = 20;
// shortcut: empirical export allowance, not a fanout/codec bound; upgrade
// at the export owner if remeasurement underprices archive/member spill.
// Charge the archive destination while the intermediate remains. Half an estimated
// intermediate is the smallest positive half-step covering all seven #872
// peaks: I_est/peak GiB = Madagascar .257/.192, Moldova .85/.736,
// Places 5.69/3.425, BR_MT filtered (auto/bounded) 4.2/2.965, all 9.4/4.91,
// segments 7.3/6.15 (those peaks included export member spill: under bounded
// it is now priced separately as "export member spill"; under auto this
// allowance is what covers it). I_est already exceeds I_actual.
const OUTPUT_PER_INTERMEDIATE: (u64, u64) = (1, 2);
// shortcut: empirical, not derived from the member encoding. Export's member
// spill holds the finest levels' full-detail rows, which the pass-2 row price
// undercounts: measured/priced was 1.95x (BR_MT filtered, bounded, 16 cores:
// 2.94 vs 1.51 GiB) and 1.40x (segments). 3x leaves 50% over the worst.
// Upgrade by pricing `Partition::members` and the member codec at export.
const MEMBER_SPILL_FACTOR: u64 = 3;

/// One slot per buffered level; the separately streamed canonical is absent.
pub(super) type DiskCheck<'a> = dyn FnMut(&[Option<u64>]) -> Result<(), ConvertError> + 'a;

/// Warnings core emits through diagnostics reach both frontends.
pub(super) const CORE_REMEDIES: SpaceRemedies<'_> = SpaceRemedies {
    spill_dir: "--spill-dir (CLI) or spill_dir (Python)",
    keep_overview: "--keep-overview (CLI) or keep_overview (Python)",
};

#[derive(Debug, Default)]
pub(super) struct DiskFootprint {
    components: Vec<Component>,
    post_pass1: bool,
}

/// The same kept-row footprint, repriced when pass 2 first measures the
/// selected properties. Both checks share one warning allowance.
pub(super) struct DiskRecheck<'a> {
    pub(super) source: &'a ConvertSource,
    pub(super) selection: Option<&'a RowGroupSelection>,
    pub(super) options: &'a ConvertOptions,
    pub(super) overview: &'a Path,
    pub(super) archive: Option<&'a Path>,
    pub(super) level_rows: &'a [usize],
    pub(super) spilling_levels: usize,
    /// Original pass-1 whole-population price, retained for unseen levels.
    pub(super) price: u64,
    pub(super) prices: Vec<u64>,
    pub(super) warned: bool,
}

impl DiskRecheck<'_> {
    pub(super) fn check(
        &mut self,
        measured: &[Option<u64>],
    ) -> Result<Option<String>, ConvertError> {
        self.check_with(measured, |disk| disk.warning(CORE_REMEDIES))
    }

    pub(super) fn check_with(
        &mut self,
        measured: &[Option<u64>],
        warning: impl FnOnce(&DiskFootprint) -> Option<String>,
    ) -> Result<Option<String>, ConvertError> {
        // Cheap coarse rows cannot discount unseen detail or filtered properties.
        let fallback = measured
            .iter()
            .flatten()
            .copied()
            .fold(self.price, u64::max);
        let mut prices: Vec<_> = (0..self.level_rows.len())
            .map(|li| measured.get(li).copied().flatten().unwrap_or(fallback))
            .collect();
        if measured.len() + 1 == prices.len()
            && self.level_rows[measured.len()] as u64
                == self.source.selected_row_count(self.selection)?.max(0) as u64
        {
            // An unchanged whole source population retains its pass-1 bound;
            // coalesced coarse rows can be much larger individually.
            prices[measured.len()] = self.price;
        }
        if prices == self.prices {
            return Ok(None);
        }
        let footprint = DiskFootprint::post_pass1(
            self.source,
            self.selection,
            self.options,
            (self.overview, self.archive),
            self.level_rows,
            self.spilling_levels,
            &prices,
        )?;
        self.prices = prices;
        // Constructing the footprint still logs its [disk] info line, even
        // when the earlier check already warned.
        let message = (!self.warned).then(|| warning(&footprint)).flatten();
        self.warned |= message.is_some();
        Ok(message)
    }
}

#[derive(Debug)]
struct Component {
    dir: PathBuf,
    name: &'static str,
    bytes: u64,
    projected: bool,
    /// Coexistent bytes: pre-drain, each level's drain, then export.
    /// Empty means this component is resident throughout.
    phases: Vec<u64>,
}

/// Raw payload and framing bounds. Only the post-pass-1 intermediate payload
/// uses the input's compression ratio; staging and pre-read projections stay
/// raw. The payload shares `MemoryBudget`'s sink sizing, augmented by every
/// selected property column's footer weight and IPC schema/batch framing.
#[derive(Debug, Clone, Copy)]
struct RawSinkSize {
    payload: u64,
    framing: u64,
    intermediate_factor: f64,
}

impl RawSinkSize {
    fn from_source(
        source: &ConvertSource,
        selection: Option<&RowGroupSelection>,
        options: &ConvertOptions,
    ) -> Result<Self, ConvertError> {
        let schema = source.schema()?;
        let rows = source.selected_row_count(selection)?.max(0) as u64;
        let mut columns = 0u64;
        let mut compressed = 0u64;
        let mut uncompressed = 0u64;
        for column in 0..schema.fields().len() {
            columns = columns.saturating_add(source.selected_column_bytes(selection, column)?);
            let (c, u) = source.selected_column_compression_bytes(selection, column)?;
            compressed = compressed.saturating_add(c);
            uncompressed = uncompressed.saturating_add(u);
        }
        let geometry = source
            .resolved_schema()?
            .geometry_name()
            .and_then(|name| schema.index_of(name).ok())
            .map(|column| source.selected_column_bytes(selection, column))
            .transpose()?;
        let avg_geometry = geometry
            .filter(|_| rows > 0)
            .map(|bytes| bytes.div_ceil(rows));
        let sink = MemoryBudget::with_available(None).sink_estimate(
            options.mode,
            usize::try_from(rows).unwrap_or(usize::MAX),
            avg_geometry,
            None,
        );
        // Arrow's native geometry and property buffers can exceed their
        // encoded Parquet weights. Never size from compressed input bytes.
        let payload = sink.max(columns.saturating_mul(2));
        let groups = selection.map_or(source.num_row_groups_total()? as u64, |s| {
            s.total_selected() as u64
        });
        let batches = rows
            .div_ceil(options.read_batch_size.max(1) as u64)
            .saturating_add(groups);
        let metadata = schema
            .metadata()
            .iter()
            .chain(
                schema
                    .fields()
                    .iter()
                    .flat_map(|field| field.metadata().iter()),
            )
            .fold(0u64, |sum, (key, value)| {
                sum.saturating_add(key.len() as u64)
                    .saturating_add(value.len() as u64)
            });
        let names = schema.fields().iter().fold(0u64, |sum, field| {
            sum.saturating_add(field.name().len() as u64)
        });
        let schema_bytes = 8192u64
            .saturating_add((schema.fields().len() as u64).saturating_mul(1024))
            .saturating_add(metadata.saturating_add(names).saturating_mul(2));
        Ok(Self {
            payload,
            framing: schema_bytes.saturating_add(batches.saturating_mul(4096)),
            intermediate_factor: intermediate_bytes_per_arrow_byte(compressed, uncompressed),
        })
    }

    fn bytes(self, copies: usize, levels: usize) -> u64 {
        self.payload
            .saturating_mul(copies as u64)
            .saturating_add(self.framing.saturating_mul(levels as u64))
    }
}

fn intermediate_bytes_per_arrow_byte(compressed: u64, uncompressed: u64) -> f64 {
    if uncompressed == 0 {
        return 1.0;
    }
    // A bare input ratio underprices BR_MT's filtered intermediate (0.335
    // vs 0.358 of the raw estimate). The 2x margin is empirical (a bare ratio missed BR_MT
    // by 7%), not derived; capped at raw.
    (2.0 * compressed as f64 / uncompressed as f64).min(1.0)
}

impl DiskFootprint {
    pub(super) fn conversion(
        source: &ConvertSource,
        selection: Option<&RowGroupSelection>,
        options: &ConvertOptions,
        overview: Option<&Path>,
    ) -> Result<Self, ConvertError> {
        // The `tiles` facade sizes before the passes select (#817 R1): select
        // here as they will, or a `--bbox` is sized as the whole input.
        let derived = if selection.is_none()
            && (options.bbox.is_some() || options.mask.is_some() || options.shard.is_some())
        {
            source.resolved_schema()?.crs().and_then(|crs| {
                // An unusable mask fails the preflight later; size without it.
                let mask = options.mask.as_ref().and_then(|m| {
                    super::mask::load_for_input(m, crs, &crate::diagnostics::Diagnostics::silent())
                        .ok()
                });
                let bbox = options
                    .bbox
                    .map(|b| super::convert::BboxQuery::new(&b, crs));
                let mask = super::mask::mask_query(mask.as_ref(), crs);
                select_row_groups_streaming(
                    source,
                    bbox.as_ref(),
                    mask.as_ref(),
                    None,
                    options.shard.as_ref(),
                    crs,
                )
                .ok()
                .flatten()
            })
        } else {
            None
        };
        let selection = selection.or(derived.as_ref());
        let mut footprint = Self::default();
        let scratch = options.spill_dir.clone().unwrap_or_else(std::env::temp_dir);
        // The staging owner accounts for projected columns and selected row groups.
        let staged = source.bytes_to_stage(selection)?;
        footprint.add(&scratch, "staged input", staged, false);
        if let Some(output) = overview {
            let levels = options.levels.resolve(options.gsd_base)?;
            let retained: Vec<_> = levels
                .iter()
                .filter(|(_, zoom)| {
                    options
                        .zoom_ceiling
                        .is_none_or(|ceiling| zoom.is_none_or(|z| z <= ceiling))
                })
                .collect();
            let canonical = retained.len() == levels.len();
            let buffered = retained.len().saturating_sub(usize::from(canonical));
            let raw = RawSinkSize::from_source(source, selection, options)?;
            let copies = |n: usize| {
                if matches!(options.mode, Mode::Partitioning) {
                    usize::from(n > 0)
                } else {
                    n
                }
            };
            if !matches!(options.profile, MemoryProfile::Speed) {
                footprint.add(
                    &scratch,
                    "pass-2 spill",
                    raw.bytes(copies(buffered), buffered),
                    true,
                );
            }
            // Raw bytes: an upper bound for either temp codec and a kept
            // ZSTD overview alike.
            footprint.add(
                parent_dir(output),
                "intermediate overview",
                raw.bytes(copies(retained.len()), retained.len()),
                true,
            );
        }
        footprint.log_projection();
        Ok(footprint)
    }

    /// Recheck after assignment using kept rows and the spill reservation's
    /// output-row price. Staged input already occupies disk, so the probe's
    /// free bytes account for it. Keep the raw framing bound for both codecs.
    pub(super) fn post_pass1(
        source: &ConvertSource,
        selection: Option<&RowGroupSelection>,
        options: &ConvertOptions,
        (overview, archive): (&Path, Option<&Path>),
        level_rows: &[usize],
        buffered_levels: usize,
        output_row_bytes: &[u64],
    ) -> Result<Self, ConvertError> {
        let raw = RawSinkSize::from_source(source, selection, options)?;
        debug_assert_eq!(level_rows.len(), output_row_bytes.len());
        let bytes = |rows: usize, price: u64, factor: f64| {
            ((rows as u64).saturating_mul(price) as f64 * factor).ceil() as u64
        };
        let intermediate: Vec<_> = level_rows
            .iter()
            .zip(output_row_bytes)
            .map(|(&rows, &price)| {
                bytes(rows, price, raw.intermediate_factor).saturating_add(raw.framing)
            })
            .collect();
        let spill: Vec<_> = level_rows
            .iter()
            .zip(output_row_bytes)
            .enumerate()
            .map(|(li, (&rows, &price))| {
                if li < buffered_levels && !matches!(options.profile, MemoryProfile::Speed) {
                    bytes(
                        rows,
                        price,
                        spill_bytes_per_arrow_byte(spill_codec(options.profile)),
                    )
                    .saturating_add(raw.framing)
                } else {
                    0
                }
            })
            .collect();
        let sum = |bytes: &[u64]| bytes.iter().fold(0u64, |sum, &n| sum.saturating_add(n));
        let mut remaining = sum(&spill);
        let total = sum(&intermediate);
        let output = total
            .div_ceil(OUTPUT_PER_INTERMEDIATE.1)
            .saturating_mul(OUTPUT_PER_INTERMEDIATE.0);
        // run_pass2_buffered drains in order; drain_sink drops its NamedTempFile
        // before advancing. run_pass2_levels streams canonical after the drain.
        // Keep the current spill until its entire intermediate prefix is written.
        let mut spill_phases = vec![remaining];
        let mut intermediate_phases = vec![0];
        let mut prefix = 0u64;
        for (&s, &i) in spill.iter().zip(&intermediate) {
            prefix = prefix.saturating_add(i);
            spill_phases.push(remaining);
            intermediate_phases.push(prefix);
            remaining = remaining.saturating_sub(s);
        }
        spill_phases.push(0);
        intermediate_phases.push(total);
        // Only the export phase (the last) holds the archive and the member spill.
        let phase_count = intermediate_phases.len();
        let export_only = |bytes: u64| {
            let mut phases = vec![0; phase_count];
            *phases.last_mut().unwrap() = bytes;
            phases
        };
        // `bounded` spills export's members: every level of a partitioning
        // export, and any multi-wave level (fewer cores or less free RAM
        // mean more waves) otherwise. Price the worst case: all levels'
        // members (partitioning) or the largest level's, at the row price
        // times [`MEMBER_SPILL_FACTOR`]; a feature split across partitions
        // adds copies the row count does not see.
        // shortcut: `auto` also spills when free RAM at export time is short,
        // which the preflight cannot know; it is not priced. Upgrade with an
        // export-time recheck.
        let member_levels: Vec<u64> = level_rows
            .iter()
            .zip(output_row_bytes)
            .map(|(&rows, &price)| {
                (rows as u64)
                    .saturating_mul(price)
                    .saturating_mul(MEMBER_SPILL_FACTOR)
            })
            .collect();
        let members = if !matches!(options.profile, MemoryProfile::Bounded) {
            0
        } else if matches!(options.mode, Mode::Partitioning) {
            sum(&member_levels)
        } else {
            member_levels.iter().copied().max().unwrap_or(0)
        };
        let mut footprint = Self {
            post_pass1: true,
            ..Self::default()
        };
        let scratch = options.spill_dir.clone().unwrap_or_else(std::env::temp_dir);
        if !matches!(options.profile, MemoryProfile::Speed) {
            footprint.add_phased(&scratch, "pass-2 spill", spill_phases[0], spill_phases);
        }
        footprint.add_phased(
            parent_dir(overview),
            "intermediate overview",
            total,
            intermediate_phases,
        );
        // A convert-only run never exports, so it has neither.
        if let Some(archive) = archive {
            footprint.add_phased(
                parent_dir(archive),
                "output archive",
                output,
                export_only(output),
            );
            footprint.add_phased(
                &scratch,
                "export member spill",
                members,
                export_only(members),
            );
        }
        footprint.log_projection();
        Ok(footprint)
    }

    fn log_projection(&self) {
        let phase = if self.post_pass1 {
            "after pass 1"
        } else {
            "pre-read"
        };
        log::info!(
            "[disk] {phase} projected footprint: {}",
            self.components
                .iter()
                .filter(|c| self.post_pass1 || c.projected)
                .map(|c| format!(
                    "{} {:.3} GiB in {}",
                    c.name,
                    c.bytes as f64 / (1u64 << 30) as f64,
                    c.dir.display()
                ))
                .collect::<Vec<_>>()
                .join(" + "),
        );
    }

    /// `tiles --layer-column` (#821): the split's uncompressed copy of the
    /// selected input, staged in `dir`.
    pub(super) fn add_staged_layers(
        &mut self,
        source: &ConvertSource,
        selection: Option<&RowGroupSelection>,
        options: &ConvertOptions,
        dir: &Path,
    ) -> Result<(), ConvertError> {
        let raw = RawSinkSize::from_source(source, selection, options)?;
        self.add(dir, "staged layers", raw.bytes(1, 1), false);
        Ok(())
    }

    fn add(&mut self, dir: &Path, name: &'static str, bytes: u64, projected: bool) {
        if bytes > 0 {
            self.components.push(Component {
                dir: dir.to_path_buf(),
                name,
                bytes,
                projected,
                phases: Vec::new(),
            });
        }
    }

    /// A component whose coexistent bytes differ per phase (see [`Component::phases`]).
    fn add_phased(&mut self, dir: &Path, name: &'static str, bytes: u64, phases: Vec<u64>) {
        self.add(dir, name, bytes, false);
        if bytes > 0 {
            self.components.last_mut().unwrap().phases = phases;
        }
    }

    #[cfg(test)]
    pub(super) fn spill_bytes(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| c.name == "pass-2 spill")
            .fold(0, |sum, c| sum.saturating_add(c.bytes))
    }

    pub(super) fn warning(&self, remedies: SpaceRemedies<'_>) -> Option<String> {
        self.warning_with(remedies, |dir| {
            let dir = existing_ancestor(dir)?;
            Some((volume_key(dir)?, probe_available_space(dir)?))
        })
    }

    /// Sum co-resident parts on this volume before taking the maximum.
    fn peak_bytes(&self, components: &[&Component]) -> u64 {
        let phases = self
            .components
            .iter()
            .map(|c| c.phases.len())
            .max()
            .unwrap_or(0)
            .max(1);
        (0..phases)
            .map(|i| {
                components.iter().fold(0u64, |sum, c| {
                    sum.saturating_add(c.phases.get(i).copied().unwrap_or(c.bytes))
                })
            })
            .max()
            .unwrap_or(0)
    }

    /// Inject volume identity as well as free bytes: distinct directories on
    /// one filesystem must be summed before applying the single margin.
    pub(super) fn warning_with(
        &self,
        remedies: SpaceRemedies<'_>,
        mut probe: impl FnMut(&Path) -> Option<(String, u64)>,
    ) -> Option<String> {
        let mut volumes: BTreeMap<String, (Vec<&Component>, u64, Vec<String>)> = BTreeMap::new();
        for component in &self.components {
            if component.projected && !self.post_pass1 {
                continue;
            }
            let Some((volume, available)) = probe(&component.dir) else {
                continue;
            };
            let entry = volumes
                .entry(volume)
                .or_insert_with(|| (Vec::new(), available, Vec::new()));
            entry.0.push(component);
            entry.1 = entry.1.min(available);
            entry
                .2
                .push(format!("{} in {}", component.name, component.dir.display()));
        }
        let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let phase = if self.post_pass1 { " after pass 1" } else { "" };
        let warnings: Vec<_> = volumes
            .into_values()
            .filter_map(|(components, available, parts)| {
                let estimate = self.peak_bytes(&components);
                let need = estimate.saturating_add(estimate / MARGIN_DENOM);
                (available < need).then(|| {
                    format!(
                "projected disk footprint{phase} may not fit: {} need approximately {:.1} GiB; \
                 their volume has {:.1} GiB free ({:.1} GiB short, including a 5% margin). \
                 Point {} (or {}) at a roomier volume, or free up space first.",
                parts.join(" + "), gib(estimate), gib(available), gib(need - available),
                remedies.spill_dir, remedies.keep_overview)
                })
            })
            .collect();
        (!warnings.is_empty()).then(|| warnings.join("\n"))
    }
}

/// The directory a file lands in; the current directory for a bare name.
fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn existing_ancestor(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|path| path.is_dir())
}

fn volume_key(dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("device:{}", std::fs::metadata(dir).ok()?.dev()))
    }
    #[cfg(not(unix))]
    {
        let absolute = dir.canonicalize().ok()?;
        Some(absolute.ancestors().last()?.display().to_string())
    }
}

/// The only filesystem free-space probe used by overview preflights.
fn probe_available_space(dir: &Path) -> Option<u64> {
    fs4::available_space(dir).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REMEDIES: SpaceRemedies<'_> = SpaceRemedies {
        spill_dir: "--spill-dir",
        keep_overview: "--keep-overview",
    };

    #[test]
    fn convert_only_has_no_output_archive() {
        let logging = crate::test_log::install();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1., 2.)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        crate::test_log::take_thread_lines();
        let disk = DiskFootprint::post_pass1(
            &source,
            None,
            &ConvertOptions::default(),
            (&dir.path().join("out.parquet"), None),
            &[1_000, 40_000],
            1,
            &[1_600; 2],
        )
        .unwrap();
        assert!(!disk
            .components
            .iter()
            .any(|c| matches!(c.name, "output archive" | "export member spill")));
        let warning = disk
            .warning_with(REMEDIES, |_| Some(("same".into(), 0)))
            .unwrap();
        assert!(!warning.contains("output archive"), "{warning}");
        if logging {
            let lines = crate::test_log::take_thread_lines();
            assert!(lines.iter().any(|line| line.starts_with("[disk]")));
            assert!(
                lines.iter().all(|line| !line.contains("output archive")),
                "{lines:#?}"
            );
        }
    }

    #[test]
    fn export_member_spill_is_priced_under_bounded_only() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1., 2.)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let archive = dir.path().join("out.pmtiles");
        let member_spill = |mode, profile| {
            let options = ConvertOptions {
                mode,
                profile,
                ..ConvertOptions::default()
            };
            DiskFootprint::post_pass1(
                &source,
                None,
                &options,
                (&dir.path().join("out.parquet"), Some(&archive)),
                &[1_000, 40_000],
                1,
                &[1_600; 2],
            )
            .unwrap()
            .components
            .iter()
            .find(|c| c.name == "export member spill")
            .map(|c| (c.bytes, *c.phases.last().unwrap(), c.phases[0]))
        };
        // Partitioning spills every level's members; duplicating, the largest level's.
        assert_eq!(
            member_spill(Mode::Partitioning, MemoryProfile::Bounded),
            Some((41_000 * 1_600 * 3, 41_000 * 1_600 * 3, 0))
        );
        assert_eq!(
            member_spill(Mode::Duplicating, MemoryProfile::Bounded),
            Some((40_000 * 1_600 * 3, 40_000 * 1_600 * 3, 0))
        );
        assert_eq!(member_spill(Mode::Partitioning, MemoryProfile::Auto), None);
        assert_eq!(member_spill(Mode::Partitioning, MemoryProfile::Speed), None);
    }

    #[test]
    fn projection_log_does_not_claim_a_cross_volume_peak() {
        let logging = crate::test_log::install();
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "pass-2 spill", 100, true);
        disk.add(Path::new("/output"), "intermediate overview", 200, true);
        disk.add(Path::new("/staging"), "staged input", 300, false);
        crate::test_log::take_thread_lines();
        disk.log_projection();
        if logging {
            let lines = crate::test_log::take_thread_lines();
            let line = lines
                .iter()
                .find(|line| line.starts_with("[disk]"))
                .unwrap();
            assert!(line.contains("pass-2 spill") && line.contains("intermediate overview"));
            assert!(!line.contains("staged input"), "{line}");
            assert!(!line.contains("simultaneous peak"), "{line}");
        }
    }

    #[test]
    fn measured_prices_weight_levels_and_protect_unseen_and_canonical_rows() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &vec![Some(geo::Geometry::Point(geo::Point::new(1., 2.))); 10],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions::default();
        let output = dir.path().join("out.parquet");
        let framing = RawSinkSize::from_source(&source, None, &options)
            .unwrap()
            .framing;
        let mut recheck = DiskRecheck {
            source: &source,
            selection: None,
            options: &options,
            overview: &output,
            archive: None,
            level_rows: &[2, 3, 5, 7],
            spilling_levels: 3,
            price: 100,
            prices: Vec::new(),
            warned: false,
        };
        for (measured, expected) in [
            ([Some(10), None, Some(20)], [10, 100, 20, 100]),
            ([Some(10), None, Some(200)], [10, 200, 200, 200]),
        ] {
            recheck
                .check_with(&measured, |disk| {
                    assert_eq!(
                        disk.spill_bytes(),
                        2 * expected[0] + 3 * expected[1] + 5 * expected[2] + 3 * framing
                    );
                    assert_eq!(
                        disk.components
                            .iter()
                            .find(|c| c.name == "intermediate overview")
                            .unwrap()
                            .bytes,
                        2 * expected[0]
                            + 3 * expected[1]
                            + 5 * expected[2]
                            + 7 * expected[3]
                            + 4 * framing
                    );
                    None
                })
                .unwrap();
            assert_eq!(recheck.prices, expected);
        }
        // The final level is separately streamed and retains ALL source rows.
        recheck.level_rows = &[2, 3, 5, 10];
        recheck
            .check_with(&[Some(10), None, Some(200)], |_| None)
            .unwrap();
        assert_eq!(recheck.prices, [10, 200, 200, 100]);
    }

    #[test]
    fn lifecycle_keeps_the_current_spill_through_its_drain() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1., 2.)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions::default();
        let raw = RawSinkSize::from_source(&source, None, &options).unwrap();
        let disk = DiskFootprint::post_pass1(
            &source,
            None,
            &options,
            (&dir.path().join("out.parquet"), None),
            &[40_000, 1_000, 1],
            2,
            &[1_600; 3],
        )
        .unwrap();
        // The first drain dominates: both spills still exist when I_0 finishes.
        let peak = disk.spill_bytes()
            + (40_000.0 * 1_600.0 * raw.intermediate_factor).ceil() as u64
            + raw.framing;
        let need = peak + peak / MARGIN_DENOM;
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), need)))
            .is_none());
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), need - 1)))
            .is_some());
    }

    #[test]
    fn lifecycle_releases_spill_before_canonical_and_groups_each_volume() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1., 2.)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions {
            spill_dir: Some(dir.path().join("scratch")),
            ..Default::default()
        };
        let mut disk = DiskFootprint::post_pass1(
            &source,
            None,
            &options,
            (&dir.path().join("out.parquet"), None),
            &[1_000, 2_000, 40_000],
            2,
            &[1_600; 3],
        )
        .unwrap();
        let intermediate = disk
            .components
            .iter()
            .find(|c| c.name == "intermediate overview")
            .unwrap()
            .bytes;
        let need = intermediate + intermediate / MARGIN_DENOM;
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), need)))
            .is_none());
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), need - 1)))
            .is_some());
        let spill = disk.spill_bytes();
        // Spill on its own volume needs its full maximum, independently of I.
        let separate = |p: &Path| {
            let bytes = if p == options.spill_dir.as_ref().unwrap() {
                spill
            } else {
                intermediate
            };
            Some((p.display().to_string(), bytes + bytes / MARGIN_DENOM))
        };
        assert!(disk.warning_with(REMEDIES, separate).is_none());
        assert!(disk
            .warning_with(REMEDIES, |p| {
                let (key, free) = separate(p).unwrap();
                Some((
                    key,
                    free - u64::from(p == options.spill_dir.as_ref().unwrap()),
                ))
            })
            .is_some());
        // Fixed staging remains resident at every phase.
        disk.add(dir.path(), "staged layers", 100, false);
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), need)))
            .is_some());
    }

    #[test]
    fn export_output_exceeds_the_intermediate_dominated_conversion_peak() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1., 2.)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let archive = dir.path().join("tiles/out.pmtiles");
        let disk = DiskFootprint::post_pass1(
            &source,
            None,
            &ConvertOptions::default(),
            (&dir.path().join("out.parquet"), Some(archive.as_path())),
            &[1_000, 40_000],
            1,
            &[1_600; 2],
        )
        .unwrap();
        let intermediate = disk
            .components
            .iter()
            .find(|c| c.name == "intermediate overview")
            .unwrap()
            .bytes;
        let output = disk
            .components
            .iter()
            .find(|c| c.name == "output archive")
            .expect("export needs space while the whole intermediate exists");
        assert_eq!(output.bytes, intermediate.div_ceil(2));
        assert_eq!(output.dir, archive.parent().unwrap());
        // Separate volumes each need only their own export-phase bytes.
        let separate = |p: &Path| {
            let bytes = if p == output.dir {
                output.bytes
            } else {
                intermediate
            };
            Some((p.display().to_string(), bytes + bytes / MARGIN_DENOM))
        };
        assert!(disk.warning_with(REMEDIES, separate).is_none());
        for (short_dir, component, absent) in [
            (
                output.dir.as_path(),
                "output archive",
                "intermediate overview",
            ),
            (dir.path(), "intermediate overview", "output archive"),
        ] {
            let warning = disk
                .warning_with(REMEDIES, |p| {
                    let (key, free) = separate(p).unwrap();
                    Some((key, free - u64::from(p == short_dir)))
                })
                .unwrap();
            assert!(warning.contains(component), "{warning}");
            assert!(!warning.contains(absent), "{warning}");
        }
        let peak = intermediate + output.bytes;
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), peak + peak / 20)))
            .is_none());
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("same".into(), peak + peak / 20 - 1)))
            .is_some());
    }

    #[test]
    fn recheck_warning_names_cli_and_python_remedies() {
        use crate::diagnostics::{Diagnostic, Diagnostics};

        let logging = crate::test_log::install();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions::default();
        let output = dir.path().join("out.parquet");
        // Exercise both the initial and measured-price rechecks. A huge
        // synthetic row price forces a warning without writing any output.
        for price in [0, 1_600] {
            let mut recheck = DiskRecheck {
                source: &source,
                selection: None,
                options: &options,
                overview: &output,
                archive: None,
                level_rows: &[1, 1],
                spilling_levels: 1,
                price,
                prices: Vec::new(),
                warned: false,
            };
            let warning = recheck.check(&[Some(u64::MAX / 4); 2]).unwrap().unwrap();
            assert!(warning.contains("after pass 1"), "{warning}");
            assert!(
                warning.contains("--spill-dir (CLI) or spill_dir (Python)"),
                "{warning}"
            );
            assert!(
                warning.contains("--keep-overview (CLI) or keep_overview (Python)"),
                "{warning}"
            );
            let diagnostics = Diagnostics::default();
            crate::test_log::take_thread_lines();
            diagnostics.disk_shortage(warning.clone());
            diagnostics.disk_shortage(warning.clone());
            assert!(recheck.check(&[Some(u64::MAX / 2); 2]).unwrap().is_none());
            assert_eq!(
                diagnostics.snapshot(),
                vec![Diagnostic::DiskShortage {
                    count: 1,
                    message: warning.clone(),
                }]
            );
            assert!(diagnostics.snapshot_for_plan().is_empty());
            assert_eq!(
                serde_json::to_value(&diagnostics.snapshot()[0]).unwrap(),
                serde_json::json!({"kind": "disk_shortage", "count": 1, "message": warning})
            );
            if logging {
                assert_eq!(
                    crate::test_log::take_thread_lines()
                        .iter()
                        .filter(|line| line.contains("may not fit"))
                        .count(),
                    1
                );
            }
        }
        assert!(!output.exists());
    }

    #[test]
    fn unfiltered_local_input_warns_only_after_pass1() {
        let logging = crate::test_log::install();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions::default();
        let output = dir.path().join("out.parquet");
        crate::test_log::take_thread_lines();
        let mut before = DiskFootprint::conversion(&source, None, &options, Some(&output)).unwrap();
        assert!(before
            .warning_with(REMEDIES, |_| Some(("one".into(), 0)))
            .is_none());
        if logging {
            let lines = crate::test_log::take_thread_lines();
            assert!(
                lines.iter().any(|line| line.starts_with("[disk]")
                    && line.contains("pass-2 spill")
                    && line.contains("intermediate overview")),
                "{lines:#?}"
            );
        }
        // Staging must still warn, without including either projection on
        // the same volume, for both CLI and Python spellings of the remedies.
        before.add(dir.path(), "staged input", 100, false);
        before
            .add_staged_layers(&source, None, &options, dir.path())
            .unwrap();
        for remedies in [
            REMEDIES,
            SpaceRemedies {
                spill_dir: "spill_dir",
                keep_overview: "keep_overview",
            },
        ] {
            let warning = before
                .warning_with(remedies, |_| Some(("one".into(), 0)))
                .unwrap();
            assert!(warning.contains("staged input") && warning.contains("staged layers"));
            assert!(warning.contains(remedies.spill_dir));
            assert!(
                !warning.contains("pass-2 spill") && !warning.contains("intermediate overview")
            );
        }
        // The unfiltered run still gets a shortage warning before pass 2.
        let after = DiskFootprint::post_pass1(
            &source,
            None,
            &options,
            (&output, None),
            &[1, 1],
            1,
            &[1_600; 2],
        )
        .unwrap();
        let warning = after
            .warning_with(REMEDIES, |_| Some(("one".into(), 0)))
            .unwrap();
        assert!(warning.contains("after pass 1"), "{warning}");
        assert!(warning.contains("pass-2 spill") && warning.contains("intermediate overview"));
        assert!(warning.contains("need approximately"), "{warning}");
        assert!(!output.exists(), "the recheck precedes pass-2 output");
    }

    #[test]
    fn post_pass1_counts_only_spilling_levels() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let options = ConvertOptions {
            profile: MemoryProfile::Auto,
            ..ConvertOptions::default()
        };
        let output = dir.path().join("out.parquet");
        for buffered_levels in [0, 1] {
            let footprint = DiskFootprint::post_pass1(
                &source,
                None,
                &options,
                (&output, None),
                &[1, 1],
                buffered_levels,
                &[1_600; 2],
            )
            .unwrap();
            assert_eq!(
                footprint
                    .components
                    .iter()
                    .any(|c| c.name == "pass-2 spill"),
                buffered_levels > 0
            );
            assert!(footprint
                .components
                .iter()
                .any(|c| c.name == "intermediate overview"));
        }
    }

    #[test]
    fn post_pass1_spill_uses_the_shared_codec_price_and_raw_framing() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        for profile in [MemoryProfile::Auto, MemoryProfile::Bounded] {
            let options = ConvertOptions {
                profile,
                ..ConvertOptions::default()
            };
            let framing = RawSinkSize::from_source(&source, None, &options)
                .unwrap()
                .framing;
            let footprint = DiskFootprint::post_pass1(
                &source,
                None,
                &options,
                (&dir.path().join("out.parquet"), None),
                &[10, 20],
                1,
                &[1_600; 2],
            )
            .unwrap();
            let factor = super::super::pipeline::spill_bytes_per_arrow_byte(
                super::super::pipeline::spill_codec(profile),
            );
            assert_eq!(
                footprint.spill_bytes(),
                (16_000.0 * factor) as u64 + framing
            );
        }
    }

    #[test]
    fn intermediate_compression_has_a_twofold_margin_capped_at_raw() {
        for (compressed, uncompressed, expected, actual) in [
            (335, 1_000, 0.670, 0.358), // BR_MT filtered (unfiltered: 0.110)
            (593, 1_000, 1.000, 0.105), // Madagascar
            (163, 1_000, 0.326, 0.083), // Moldova
            (2_000, 1_000, 1.000, 0.0),
            (0, 0, 1.000, 0.0), // no footer payload: retain the raw bound
        ] {
            let factor = intermediate_bytes_per_arrow_byte(compressed, uncompressed);
            assert!((factor - expected).abs() < 1e-12, "{factor} vs {expected}");
            assert!(factor >= actual, "{factor} must cover measured {actual}");
        }
    }

    #[test]
    fn post_pass1_intermediate_prices_selected_compressed_payload() {
        use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
        use std::sync::Arc;
        use tylertoo_io::{OverviewCompression, OverviewWriteOptions};

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        let batch = RecordBatch::try_from_iter(vec![
            (
                "dropped",
                Arc::new(Int64Array::from_iter_values(0..100)) as ArrayRef,
            ),
            (
                "kept",
                Arc::new(StringArray::from(vec!["x".repeat(1_000); 100])) as ArrayRef,
            ),
        ])
        .unwrap();
        let mut writer = tylertoo_io::overview_writer(
            std::fs::File::create(&input).unwrap(),
            batch.schema(),
            &batch.schema(),
            &OverviewWriteOptions {
                compression: OverviewCompression::Zstd(1),
                full_column_stats: true,
                geometry_columns: vec![],
                covering_columns: vec![],
            },
        )
        .unwrap();
        for (offset, len) in [(0, 10), (10, 90)] {
            writer.write(&batch.slice(offset, len)).unwrap();
            writer.flush().unwrap();
        }
        writer.close().unwrap();
        let source = ConvertSource::resolve_path(&input).unwrap();
        source.restrict_columns(vec![1]).unwrap();
        let selection = RowGroupSelection::from_parts(vec![vec![1]]);
        let meta =
            tylertoo_io::read_parquet_metadata(&std::fs::File::open(&input).unwrap()).unwrap();
        let column = meta.row_group(1).column(1);
        let compressed = column.compressed_size() as u64;
        let uncompressed = column.uncompressed_size() as u64;
        assert!(compressed * 2 < uncompressed);
        assert_eq!(
            source
                .selected_column_compression_bytes(Some(&selection), 0)
                .unwrap(),
            (compressed, uncompressed)
        );
        // Dictionary expansion is part of the row price, never the codec ratio.
        assert!(source.selected_column_bytes(Some(&selection), 0).unwrap() > uncompressed);
        let options = ConvertOptions::default();
        let raw = RawSinkSize::from_source(&source, Some(&selection), &options).unwrap();
        let footprint = DiskFootprint::post_pass1(
            &source,
            Some(&selection),
            &options,
            (&dir.path().join("out.parquet"), None),
            &[80, 90],
            1,
            &[1_600; 2],
        )
        .unwrap();
        let intermediate = footprint
            .components
            .iter()
            .find(|c| c.name == "intermediate overview")
            .unwrap();
        let factor = (2.0 * compressed as f64 / uncompressed as f64).min(1.0);
        assert_eq!(
            intermediate.bytes,
            (170.0 * 1_600.0 * factor).ceil() as u64 + 2 * raw.framing
        );
    }

    #[test]
    fn post_pass1_auto_ram_run_has_no_spill_component() {
        use super::super::convert::{convert_to_overviews, LevelPlan};

        let logging = crate::test_log::install();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        let geoms = [
            Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0))),
            Some(geo::Geometry::Point(geo::Point::new(10.0, 20.0))),
        ];
        super::super::testutil::write_input(&input, &geoms, true, None);
        let options = ConvertOptions {
            profile: MemoryProfile::Auto,
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 2,
            },
            ..ConvertOptions::default()
        };
        crate::test_log::take_thread_lines();
        convert_to_overviews(&input, dir.path().join("out.parquet"), &options).unwrap();
        if logging {
            let lines = crate::test_log::take_thread_lines();
            let backing = lines
                .iter()
                .position(|line| line.starts_with("[convert] pass2 auto") && line.contains("→ Ram"))
                .unwrap_or_else(|| panic!("{lines:#?}"));
            let recheck = lines
                .iter()
                .position(|line| line.starts_with("[disk] after pass 1 projected footprint:"))
                .unwrap();
            let footprint = &lines[recheck];
            assert!(footprint.contains("intermediate overview"), "{footprint}");
            assert!(!footprint.contains("pass-2 spill"), "{footprint}");
            assert!(!footprint.contains("output archive"), "{footprint}");
            assert!(backing < recheck, "{lines:#?}");
        }
    }

    #[test]
    fn post_pass1_uses_kept_level_rows_and_still_warns_on_a_small_volume() {
        use super::super::convert::LevelPlan;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        let geoms = vec![Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0))); 10_000];
        super::super::testutil::write_input(&input, &geoms, false, None);
        let source = ConvertSource::resolve_path(&input).unwrap();
        let output = dir.path().join("out.parquet");
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 9,
            },
            spill_dir: Some(dir.path().join("scratch")),
            filter: Some("id < 100".into()),
            ..ConvertOptions::default()
        };
        let before = DiskFootprint::conversion(&source, None, &options, Some(&output)).unwrap();
        // 1,000 emitted rows over ten levels, rather than 10,000 input rows
        // copied to every level. The finest level streams without spilling.
        let rows = [100; 10];
        let row_bytes = 1_600;
        let after = DiskFootprint::post_pass1(
            &source,
            None,
            &options,
            (&output, None),
            &rows,
            9,
            &[row_bytes; 10],
        )
        .unwrap();
        let framing = RawSinkSize::from_source(&source, None, &options)
            .unwrap()
            .framing;
        assert_eq!(after.spill_bytes(), 900 * row_bytes + 9 * framing);
        let intermediate = after
            .components
            .iter()
            .find(|c| c.name == "intermediate overview")
            .unwrap();
        assert_eq!(intermediate.bytes, 1_000 * row_bytes + 10 * framing);
        assert_eq!(intermediate.dir, dir.path());
        assert_eq!(
            after.components[0].dir,
            options.spill_dir.as_ref().unwrap().as_path()
        );
        assert!(after.spill_bytes() * 4 < before.spill_bytes());
        assert!(after
            .warning_with(REMEDIES, |_| Some(("one".into(), u64::MAX)))
            .is_none());
        let warning = after
            .warning_with(REMEDIES, |_| Some(("one".into(), 1)))
            .unwrap();
        assert!(warning.contains("after pass 1"), "{warning}");
        assert!(!warning.contains("is rechecked after pass 1"), "{warning}");
        let speed = ConvertOptions {
            profile: MemoryProfile::Speed,
            ..options
        };
        assert_eq!(
            DiskFootprint::post_pass1(
                &source,
                None,
                &speed,
                (&output, None),
                &rows,
                9,
                &[row_bytes; 10],
            )
            .unwrap()
            .spill_bytes(),
            0
        );
    }

    #[test]
    fn projected_geometry_uses_the_exposed_schema_index() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        source.restrict_columns(vec![2]).unwrap();
        let disk = DiskFootprint::conversion(
            &source,
            None,
            &ConvertOptions::default(),
            Some(&dir.path().join("out.parquet")),
        )
        .unwrap();
        assert!(disk.spill_bytes() > 0);
    }

    /// #817 R1: the `tiles` facade sizes the footprint with no selection; a
    /// `--bbox` must still size only the row groups it selects, not the whole
    /// input (a Connecticut bbox over Overture warned of 33 TiB).
    #[test]
    fn unselected_footprint_still_honors_the_bbox() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        let geoms: Vec<_> = (0..100)
            .map(|i| Some(geo::Geometry::Point(geo::Point::new(f64::from(i), 0.0))))
            .collect();
        super::super::testutil::write_input_with_f64_row_groups(
            &input,
            &geoms,
            "v",
            &[0.0; 100],
            Some(10),
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let out = dir.path().join("out.parquet");
        let spill = |bbox| {
            let options = ConvertOptions {
                bbox,
                ..ConvertOptions::default()
            };
            DiskFootprint::conversion(&source, None, &options, Some(&out))
                .unwrap()
                .spill_bytes()
        };
        let whole = spill(None);
        let clipped = spill(Some([0.0, -1.0, 5.0, 1.0]));
        assert!(clipped > 0 && clipped * 3 < whole, "{clipped} vs {whole}");
    }

    /// #821: a `--layer-column` run's staged copy of the input is named in
    /// the footprint, in the directory it is staged in.
    #[test]
    fn staged_layers_are_part_of_the_footprint() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        super::super::testutil::write_input(
            &input,
            &[Some(geo::Geometry::Point(geo::Point::new(1.0, 2.0)))],
            false,
            None,
        );
        let source = ConvertSource::resolve_path(&input).unwrap();
        let mut disk = DiskFootprint::default();
        disk.add_staged_layers(
            &source,
            None,
            &ConvertOptions::default(),
            Path::new("/scratch"),
        )
        .unwrap();
        let warning = disk
            .warning_with(super::super::tiles::CLI_REMEDIES, |_| Some(("v".into(), 0)))
            .unwrap();
        assert!(warning.contains("staged layers in /scratch"), "{warning}");
    }

    #[test]
    fn same_volume_components_share_one_margin() {
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "staged input", 100, false);
        disk.add(Path::new("/scratch"), "pass-2 spill", 200, false);
        disk.add(Path::new("/output"), "intermediate overview", 300, false);
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("one".into(), 630)))
            .is_none());
        let warning = disk
            .warning_with(REMEDIES, |_| Some(("one".into(), 629)))
            .unwrap();
        for name in [
            "staged input",
            "pass-2 spill",
            "intermediate overview",
            "--spill-dir",
            "/output",
        ] {
            assert!(warning.contains(name), "{warning}");
        }
        assert_eq!(warning.matches("may not fit").count(), 1);
    }

    #[test]
    fn the_warning_states_its_sizes_in_gib() {
        const GIB: u64 = 1 << 30;
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "pass-2 spill", 10 * GIB, false);
        let warning = disk
            .warning_with(REMEDIES, |_| Some(("one".into(), 4 * GIB)))
            .unwrap();
        assert!(
            warning.contains(
                "need approximately 10.0 GiB; their volume has 4.0 GiB free (6.5 GiB short"
            ),
            "{warning}"
        );
        assert!(
            !warning.contains("filter is applied during the read"),
            "{warning}"
        );
    }

    #[test]
    fn separate_volumes_are_not_summed_and_unknown_probe_is_quiet() {
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "pass-2 spill", 200, false);
        disk.add(Path::new("/output"), "intermediate overview", 300, false);
        assert!(disk
            .warning_with(REMEDIES, |p| Some((p.display().to_string(), 315)))
            .is_none());
        assert!(disk.warning_with(REMEDIES, |_| None).is_none());
        disk.add(Path::new("/scratch"), "staged input", u64::MAX, false);
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("one".into(), u64::MAX - 1)))
            .is_some());
    }
}
