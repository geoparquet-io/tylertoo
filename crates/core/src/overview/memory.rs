//! Per-conversion memory estimates and limits.

pub(super) const SINK_ROW_ALLOC_OVERHEAD_BYTES: u64 = 256;
use super::assign::FeatureTable;
use super::convert::ConvertError;
use super::level::{MemoryProfile, Mode};
use super::pipeline::{probe_preflight_memory_limit, MemoryLimit};

#[derive(Debug, Clone, Copy)]
pub(super) enum MemoryPhase {
    EagerInput,
    Scan,
    Assign,
    Read,
    Compute,
    Write,
}

impl MemoryPhase {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::EagerInput => "eager input",
            Self::Scan => "pass-1 scan",
            Self::Assign => "assignment",
            Self::Read => "read-ahead",
            Self::Compute => "pass-2 compute",
            Self::Write => "writer",
        }
    }
}

/// One snapshot belongs to a conversion; subsequent phases never re-probe or
/// inherit another conversion's process-wide cached headroom.
#[derive(Debug, Clone, Copy)]
pub(super) struct MemoryBudget {
    pub(super) limit: Option<MemoryLimit>,
}

impl MemoryBudget {
    pub(super) fn probe() -> Self {
        Self {
            limit: probe_preflight_memory_limit(),
        }
    }
    pub(super) fn with_available(available: Option<u64>) -> Self {
        Self {
            limit: available.map(|bytes| MemoryLimit {
                bytes,
                source: super::pipeline::MemoryLimitSource::MemAvailable,
            }),
        }
    }
    pub(super) fn available(self) -> Option<u64> {
        self.limit.map(|l| l.bytes)
    }
    /// Python list allowance; exact Rust reports always stream from spill.
    pub(super) fn python_loss_allowance(self) -> usize {
        let cap = crate::diagnostics::MAX_PYTHON_LOSS_BYTES as u64;
        self.available().map_or(cap, |b| (b / 10).min(cap)) as usize
    }
    pub(super) fn allowance(self, profile: MemoryProfile) -> u64 {
        match profile {
            MemoryProfile::Speed => u64::MAX,
            _ => self
                .available()
                .map_or(4 << 30, |b| (b as f64 * 0.6) as u64),
        }
    }
    #[expect(
        clippy::unused_self,
        reason = "#779: estimates and enforcement share the per-run MemoryBudget API"
    )]
    pub(super) fn estimate(self, phase: MemoryPhase, rows: u64, geometry: Option<u64>) -> u64 {
        let per_row = match phase {
            MemoryPhase::EagerInput | MemoryPhase::Scan => FeatureTable::MAX_BYTES_PER_ROW,
            MemoryPhase::Assign => FeatureTable::MAX_BYTES_PER_ROW,
            MemoryPhase::Read | MemoryPhase::Compute => 4096u64.saturating_add(
                geometry
                    .filter(|b| *b > 0)
                    .map_or(1024, |b| b.saturating_mul(2)),
            ),
            MemoryPhase::Write => geometry.filter(|b| *b > 0).map_or(8192, |b| {
                SINK_ROW_ALLOC_OVERHEAD_BYTES.saturating_add(b.saturating_mul(2))
            }),
        };
        rows.saturating_mul(per_row)
    }
    pub(super) fn sink_estimate(self, mode: Mode, rows: usize, geometry: Option<u64>) -> u64 {
        if matches!(mode, Mode::Duplicating) {
            self.estimate(MemoryPhase::Write, rows as u64, geometry)
        } else {
            (rows as u64).saturating_mul(geometry.filter(|b| *b > 0).map_or(16384, |b| {
                SINK_ROW_ALLOC_OVERHEAD_BYTES.saturating_add(b.saturating_mul(4))
            }))
        }
    }
    pub(super) fn compute_window(
        self,
        profile: MemoryProfile,
        requested: usize,
        batch_rows: usize,
        levels: usize,
        geometry: Option<u64>,
    ) -> usize {
        if matches!(profile, MemoryProfile::Speed) {
            return requested.max(1);
        }
        let per_batch = self
            .estimate(MemoryPhase::Compute, batch_rows as u64, geometry)
            .saturating_mul(levels.max(1) as u64)
            .max(1);
        let available = self.allowance(profile) / 10;
        requested
            .max(1)
            .min((available / per_batch).max(1) as usize)
    }
    pub(super) fn require(self, phase: MemoryPhase, needed: u64) -> Result<(), ConvertError> {
        if let Some(available) = self.available() {
            if needed > available {
                return Err(ConvertError::BudgetExceeded {
                    phase: phase.name(),
                    needed,
                    available,
                });
            }
        }
        Ok(())
    }

    /// Retained input stays resident across every subsequent phase. Charge
    /// it once against this run's snapshot before calculating any allowances.
    pub(super) fn reserve(&mut self, phase: MemoryPhase, bytes: u64) -> Result<(), ConvertError> {
        self.require(phase, bytes)?;
        if let Some(limit) = &mut self.limit {
            limit.bytes = limit.bytes.saturating_sub(bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exceeded_names_each_phase() {
        let budget = MemoryBudget::with_available(Some(100));
        for phase in [
            MemoryPhase::EagerInput,
            MemoryPhase::Scan,
            MemoryPhase::Assign,
            MemoryPhase::Read,
            MemoryPhase::Compute,
            MemoryPhase::Write,
        ] {
            let error = budget.require(phase, 101).unwrap_err();
            assert!(
                matches!(error, super::super::convert::ConvertError::BudgetExceeded {
                phase: name, needed: 101, available: 100
            } if name == phase.name())
            );
            budget.require(phase, 100).unwrap();
        }
    }

    #[test]
    fn retained_input_reduces_every_later_allowance() {
        let mut budget = MemoryBudget::with_available(Some(1000));
        budget.reserve(MemoryPhase::EagerInput, 600).unwrap();
        assert_eq!(budget.available(), Some(400));
        assert_eq!(budget.allowance(MemoryProfile::Bounded), 240);
        assert!(budget.require(MemoryPhase::Scan, 401).is_err());
        assert!(budget.reserve(MemoryPhase::EagerInput, 401).is_err());
        assert_eq!(budget.available(), Some(400));
    }

    /// The phase-1 stand-in for a reserving grant: `&mut` so the call site
    /// stays the same once choosing a window charges the budget.
    fn grant_window(
        budget: &mut MemoryBudget,
        requested: usize,
        batch_rows: usize,
        levels: usize,
    ) -> usize {
        budget.compute_window(
            MemoryProfile::Bounded,
            requested,
            batch_rows,
            levels,
            Some(600),
        )
    }

    /// #716: the pass-2 compute window is resident at the same time as the
    /// read-ahead, the drain's encodes and the sink, so choosing it has to
    /// charge the run's budget; every phase sized after it must see what is
    /// left. On main each of those phases takes its own slice of the same
    /// allowance and nothing sums them. Profiled (`.788/PROFILE-LOG.md`):
    /// Moldova `--profile bounded`, window 8 against window 1: +230 MiB live
    /// heap (dhat, 8 threads) and +421 MiB anonymous RSS (64 threads); NZ
    /// parcels under a 2 GiB budget peaked at 1,472 MiB anonymous against a
    /// 1,229 MiB allowance.
    #[test]
    #[ignore = "#716: fails on origin/main by design (#788 C2 phase 1); the fix un-ignores it"]
    fn the_compute_window_is_reserved_from_the_run_budget() {
        let available = 16u64 << 30;
        let mut budget = MemoryBudget::with_available(Some(available));
        let window = grant_window(&mut budget, 8, 8192, 15);
        assert!(window >= 1);
        assert!(
            budget.available() < Some(available),
            "a window of {window} batch(es) was granted without charging the budget"
        );
        assert!(
            budget.allowance(MemoryProfile::Bounded)
                < MemoryBudget::with_available(Some(available)).allowance(MemoryProfile::Bounded),
            "phases sized after the window must price against what it left"
        );
    }

    #[test]
    fn memory_estimates_saturate_and_unknown_limits_do_not_reject() {
        let budget = MemoryBudget::with_available(None);
        let estimate = budget.estimate(MemoryPhase::Scan, u64::MAX, None);
        assert_eq!(estimate, u64::MAX);
        budget.require(MemoryPhase::Scan, estimate).unwrap();
    }
}
