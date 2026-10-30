//! One conservative disk footprint per conversion, grouped by destination volume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::input_set::{ConvertSource, RowGroupSelection};

use super::convert::{ConvertError, ConvertOptions};
use super::level::{MemoryProfile, Mode};
use super::memory::MemoryBudget;
use super::tiles::SpaceRemedies;

const MARGIN_DENOM: u64 = 20;

#[derive(Debug, Default)]
pub(super) struct DiskFootprint {
    components: Vec<Component>,
}

#[derive(Debug)]
struct Component {
    dir: PathBuf,
    name: &'static str,
    bytes: u64,
}

/// Raw estimates never assume a compression ratio. The payload model shares
/// `MemoryBudget`'s sink sizing, augmented by every selected property column's
/// footer weight and IPC schema/batch framing. Simplification may remove rows
/// and coordinates; the preflight conservatively retains all selected rows.
#[derive(Debug, Clone, Copy)]
struct RawSinkSize {
    payload: u64,
    framing: u64,
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
        for column in 0..schema.fields().len() {
            columns = columns.saturating_add(source.selected_column_bytes(selection, column)?);
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
        })
    }

    fn bytes(self, copies: usize, levels: usize) -> u64 {
        self.payload
            .saturating_mul(copies as u64)
            .saturating_add(self.framing.saturating_mul(levels as u64))
    }
}

impl DiskFootprint {
    pub(super) fn conversion(
        source: &ConvertSource,
        selection: Option<&RowGroupSelection>,
        options: &ConvertOptions,
        overview: Option<&Path>,
    ) -> Result<Self, ConvertError> {
        let mut footprint = Self::default();
        let scratch = options.spill_dir.clone().unwrap_or_else(std::env::temp_dir);
        // The staging owner accounts for projected columns and selected row groups.
        let staged = source.bytes_to_stage(selection)?;
        footprint.add(&scratch, "staged input", staged);
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
                );
            }
            footprint.add(
                output
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
                "intermediate overview",
                raw.bytes(copies(retained.len()), retained.len()),
            );
        }
        Ok(footprint)
    }

    pub(super) fn add(&mut self, dir: &Path, name: &'static str, bytes: u64) {
        if bytes > 0 {
            self.components.push(Component {
                dir: dir.to_path_buf(),
                name,
                bytes,
            });
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

    /// Inject volume identity as well as free bytes: distinct directories on
    /// one filesystem must be summed before applying the single margin.
    pub(super) fn warning_with(
        &self,
        remedies: SpaceRemedies<'_>,
        mut probe: impl FnMut(&Path) -> Option<(String, u64)>,
    ) -> Option<String> {
        let mut volumes: BTreeMap<String, (u64, u64, Vec<String>)> = BTreeMap::new();
        for component in &self.components {
            let Some((volume, available)) = probe(&component.dir) else {
                continue;
            };
            let entry = volumes
                .entry(volume)
                .or_insert_with(|| (0, available, Vec::new()));
            entry.0 = entry.0.saturating_add(component.bytes);
            entry.1 = entry.1.min(available);
            entry
                .2
                .push(format!("{} in {}", component.name, component.dir.display()));
        }
        let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let warnings: Vec<_> =
            volumes
                .into_values()
                .filter_map(|(estimate, available, parts)| {
                    let need = estimate.saturating_add(estimate / MARGIN_DENOM);
                    (available < need).then(|| {
                        format!(
                "projected disk footprint may not fit: {} need approximately {:.1} GiB; \
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

    #[test]
    fn same_volume_components_share_one_margin() {
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "staged input", 100);
        disk.add(Path::new("/scratch"), "pass-2 spill", 200);
        disk.add(Path::new("/output"), "intermediate overview", 300);
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
    fn separate_volumes_are_not_summed_and_unknown_probe_is_quiet() {
        let mut disk = DiskFootprint::default();
        disk.add(Path::new("/scratch"), "pass-2 spill", 200);
        disk.add(Path::new("/output"), "intermediate overview", 300);
        assert!(disk
            .warning_with(REMEDIES, |p| Some((p.display().to_string(), 315)))
            .is_none());
        assert!(disk.warning_with(REMEDIES, |_| None).is_none());
        disk.add(Path::new("/scratch"), "staged input", u64::MAX);
        assert!(disk
            .warning_with(REMEDIES, |_| Some(("one".into(), u64::MAX - 1)))
            .is_some());
    }
}
