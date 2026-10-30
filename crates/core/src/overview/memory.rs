//! The run's memory budget (#779 F1, #716): one probe, one allowance, and
//! the grants every phase draws from it.
//!
//! A phase that must hold some data resident whatever it costs (the eager
//! input, the pass-1 feature table) [`reserve`](MemoryBudget::reserve)s it
//! and fails with [`ConvertError::BudgetExceeded`] when it cannot fit. A phase
//! whose footprint is a choice (how many read-ahead batches, how wide the
//! pass-2 window, how many row groups the drain encodes at once) asks
//! [`fit`](MemoryBudget::fit) how many units of its [`Price`] it may hold,
//! within its share of the allowance. Both charge the budget, so every phase
//! sized afterwards sees only what is left.

use super::assign::FeatureTable;
use super::convert::ConvertError;
use super::level::{MemoryProfile, Mode};
use super::pipeline::{probe_preflight_memory_limit, MemoryLimit};

pub(super) const SINK_ROW_ALLOC_OVERHEAD_BYTES: u64 = 256;

/// Share of the probed memory a non-`speed` run may hold resident. The rest
/// is headroom for what no phase prices: allocator retention, thread stacks,
/// the page cache a cgroup also charges.
const ALLOWANCE_FRACTION: f64 = 0.6;

/// The allowance when nothing could be probed (macOS without the override).
const FALLBACK_ALLOWANCE_BYTES: u64 = 4 << 30;

/// Share of the allowance the read-ahead may hold (#494). Small on purpose:
/// it competes with the pass-2 output sink, which is what the allowance is
/// mostly for (#294).
const READ_SHARE: f64 = 0.10;

/// Share of the allowance the pass-2 compute window may hold (#716).
const COMPUTE_SHARE: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemoryPhase {
    EagerInput,
    Scan,
    Assign,
    /// Remote staging's requests in flight (pass 0, track R).
    Stage,
    Read,
    Compute,
    /// The drain's concurrent row-group encodes (#545).
    #[expect(dead_code, reason = "#788 C2 step 2 grants the drain encodes under it")]
    Encode,
    /// The pass-2 output sink (it takes [`MemoryBudget::remaining`]).
    Write,
    /// Spill queues (track C1).
    #[expect(dead_code, reason = "#788: track C1 grants the spill queues under it")]
    Spill,
    /// Export's tile collection (track E1).
    #[expect(dead_code, reason = "#788: track E1 grants export collection under it")]
    ExportCollect,
    /// Export's concurrent tile encodes (track E1).
    #[expect(dead_code, reason = "#788: track E1 grants export encodes under it")]
    ExportEncode,
    /// Coverage fill (track F).
    #[expect(dead_code, reason = "#788: track F grants coverage fill under it")]
    Fill,
}

impl MemoryPhase {
    /// Number of phases: the length of the per-phase ledger.
    const COUNT: usize = 12;

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::EagerInput => "eager input",
            Self::Scan => "pass-1 scan",
            Self::Assign => "assignment",
            Self::Stage => "remote staging",
            Self::Read => "read-ahead",
            Self::Compute => "pass-2 compute",
            Self::Encode => "pass-2 encode",
            Self::Write => "writer",
            Self::Spill => "spill",
            Self::ExportCollect => "export collect",
            Self::ExportEncode => "export encode",
            Self::Fill => "coverage fill",
        }
    }

    /// The share of the allowance an elastic phase may be granted.
    fn share(self) -> f64 {
        match self {
            Self::Read => READ_SHARE,
            Self::Compute => COMPUTE_SHARE,
            Self::EagerInput
            | Self::Scan
            | Self::Assign
            | Self::Stage
            | Self::Encode
            | Self::Write
            | Self::Spill
            | Self::ExportCollect
            | Self::ExportEncode
            | Self::Fill => 1.0,
        }
    }
}

/// Where a [`Price`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PriceBasis {
    /// Measured on data this run already holds (a built batch, a sample).
    #[expect(
        dead_code,
        reason = "#788: measured prices arrive with C1's sink and C2 step 2"
    )]
    Measured,
    /// Read from the parquet footers before any page is decoded.
    Footer,
    /// A per-row model ([`MemoryBudget::estimate`]), not a measurement.
    Model,
}

/// The resident cost of one unit of a phase: one read batch, one window
/// batch, one row-group encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Price {
    pub(super) bytes: u64,
    pub(super) basis: PriceBasis,
}

impl Price {
    pub(super) fn new(bytes: u64, basis: PriceBasis) -> Self {
        Self { bytes, basis }
    }
    pub(super) fn model(bytes: u64) -> Self {
        Self::new(bytes, PriceBasis::Model)
    }
}

/// What a phase was granted: `units` of `unit_bytes` each, now charged to
/// the budget it came from until [`MemoryBudget::release`]d.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a grant stays charged until it is released"]
pub(super) struct Grant {
    pub(super) phase: MemoryPhase,
    pub(super) units: usize,
    pub(super) unit_bytes: u64,
}

impl Grant {
    pub(super) fn bytes(self) -> u64 {
        (self.units as u64).saturating_mul(self.unit_bytes)
    }
}

/// One snapshot belongs to a conversion; subsequent phases never re-probe or
/// inherit another conversion's process-wide cached headroom.
///
/// Two ledgers, because the two kinds of charge mean different things. A
/// [`reserve`](Self::reserve) is data that must be resident before anything
/// is sized (the eager input): it comes off the probed figure, and the
/// allowance is [`ALLOWANCE_FRACTION`] of what is left. A
/// [`fit`](Self::fit) grant is carved out of that allowance, so it comes off
/// [`remaining`](Self::remaining) one for one.
#[derive(Debug, Clone, Copy)]
pub(super) struct MemoryBudget {
    /// The probed figure, and which probe term supplied it.
    pub(super) limit: Option<MemoryLimit>,
    /// Hard reservations per phase ([`MemoryPhase`] order).
    reserved: [u64; MemoryPhase::COUNT],
    /// Elastic grants per phase, out of the allowance.
    granted: [u64; MemoryPhase::COUNT],
}

fn sum(ledger: &[u64; MemoryPhase::COUNT]) -> u64 {
    ledger.iter().fold(0u64, |a, &b| a.saturating_add(b))
}

impl MemoryBudget {
    pub(super) fn probe() -> Self {
        Self::from_limit(probe_preflight_memory_limit())
    }
    pub(super) fn from_limit(limit: Option<MemoryLimit>) -> Self {
        Self {
            limit,
            reserved: [0; MemoryPhase::COUNT],
            granted: [0; MemoryPhase::COUNT],
        }
    }
    pub(super) fn with_available(available: Option<u64>) -> Self {
        Self::from_limit(available.map(|bytes| MemoryLimit {
            bytes,
            source: super::pipeline::MemoryLimitSource::MemAvailable,
        }))
    }
    /// The probed figure minus every reservation and grant. `None` when
    /// nothing could be probed.
    pub(super) fn available(&self) -> Option<u64> {
        let held = sum(&self.reserved).saturating_add(sum(&self.granted));
        self.limit.map(|l| l.bytes.saturating_sub(held))
    }
    /// Python list allowance; exact Rust reports always stream from spill.
    pub(super) fn python_loss_allowance(&self) -> usize {
        let cap = crate::diagnostics::MAX_PYTHON_LOSS_BYTES as u64;
        self.available().map_or(cap, |b| (b / 10).min(cap)) as usize
    }
    /// What a run under `profile` may still hold resident: unbounded for
    /// `speed`, otherwise [`ALLOWANCE_FRACTION`] of what the reservations
    /// left, less every grant.
    pub(super) fn allowance(&self, profile: MemoryProfile) -> u64 {
        match profile {
            MemoryProfile::Speed => u64::MAX,
            _ => self.remaining(),
        }
    }
    /// The non-`speed` allowance still ungranted: what a phase sized last
    /// (the pass-2 sink) may take.
    pub(super) fn remaining(&self) -> u64 {
        let base = self.limit.map_or(FALLBACK_ALLOWANCE_BYTES, |l| {
            (l.bytes.saturating_sub(sum(&self.reserved)) as f64 * ALLOWANCE_FRACTION) as u64
        });
        base.saturating_sub(sum(&self.granted))
    }
    #[expect(
        clippy::unused_self,
        reason = "#779: estimates and enforcement share the per-run MemoryBudget API"
    )]
    pub(super) fn estimate(self, phase: MemoryPhase, rows: u64, geometry: Option<u64>) -> u64 {
        let per_row = match phase {
            MemoryPhase::EagerInput | MemoryPhase::Scan => FeatureTable::MAX_BYTES_PER_ROW,
            MemoryPhase::Assign => FeatureTable::MAX_BYTES_PER_ROW,
            // The phases that price themselves (staging, encodes, spill,
            // export, fill) have no per-row model of their own; a decoded
            // row is the closest one.
            MemoryPhase::Read
            | MemoryPhase::Compute
            | MemoryPhase::Stage
            | MemoryPhase::Encode
            | MemoryPhase::Spill
            | MemoryPhase::ExportCollect
            | MemoryPhase::ExportEncode
            | MemoryPhase::Fill => 4096u64.saturating_add(
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
    /// Units of `price` that `phase`'s share of the current allowance can
    /// hold, at least 1. A query: it charges nothing.
    pub(super) fn affordable(&self, phase: MemoryPhase, price: Price) -> usize {
        let ceiling = (self.remaining() as f64 * phase.share()) as u64;
        usize::try_from(ceiling / price.bytes.max(1))
            .unwrap_or(usize::MAX)
            .max(1)
    }
    /// Grant `phase` up to `want` units of `price` from its share of the
    /// allowance, and never fewer than `floor` (a phase that cannot run below
    /// it). The grant is charged to this budget.
    pub(super) fn fit(
        &mut self,
        phase: MemoryPhase,
        price: Price,
        want: usize,
        floor: usize,
    ) -> Grant {
        let units = self.affordable(phase, price).min(want).max(floor);
        let grant = Grant {
            phase,
            units,
            unit_bytes: price.bytes,
        };
        let slot = &mut self.granted[phase as usize];
        *slot = slot.saturating_add(grant.bytes());
        grant
    }
    /// Return a grant once what it priced is no longer resident.
    pub(super) fn release(&mut self, grant: Grant) {
        let slot = &mut self.granted[grant.phase as usize];
        *slot = slot.saturating_sub(grant.bytes());
    }
    /// The pass-2 compute window (#494, #716): up to `requested` input
    /// batches, each priced across every level it fans out to, fitted into
    /// the window's share and charged. `speed` takes what it asked for.
    pub(super) fn compute_window(
        &mut self,
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
        self.fit(
            MemoryPhase::Compute,
            Price::model(per_batch),
            requested.max(1),
            1,
        )
        .units
    }
    pub(super) fn require(&self, phase: MemoryPhase, needed: u64) -> Result<(), ConvertError> {
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

    /// Data that must be resident (the retained input stays resident across
    /// every later phase): charge it all or fail. Charged before any
    /// allowance is calculated from this budget.
    pub(super) fn reserve(&mut self, phase: MemoryPhase, bytes: u64) -> Result<(), ConvertError> {
        self.require(phase, bytes)?;
        let slot = &mut self.reserved[phase as usize];
        *slot = slot.saturating_add(bytes);
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

    /// `fit` grants what the phase's share affords, capped by what it wants,
    /// never below its floor, and charges it: the next fit sees less.
    #[test]
    fn fit_grants_within_the_share_and_charges_the_budget() {
        let mut budget = MemoryBudget::with_available(Some(10_000));
        // Allowance 6,000; the read share is 10% = 600 → 6 units of 100.
        let price = Price::model(100);
        assert_eq!(budget.affordable(MemoryPhase::Read, price), 6);
        assert_eq!(budget.available(), Some(10_000), "a query charges nothing");
        let grant = budget.fit(MemoryPhase::Read, price, 4, 1);
        assert_eq!((grant.units, grant.bytes()), (4, 400), "capped by want");
        assert_eq!(budget.available(), Some(9_600));
        // (6,000 − 400) × 10% = 560 → 5 units now.
        assert_eq!(budget.fit(MemoryPhase::Compute, price, 100, 1).units, 5);
        assert_eq!(budget.available(), Some(9_100));
        // The floor wins over the share: a phase cannot run below it.
        assert_eq!(
            budget
                .fit(MemoryPhase::Read, Price::model(1 << 40), 8, 2)
                .units,
            2
        );
        // A zero price cannot divide by zero, and want 0 with floor 0 grants
        // nothing.
        assert_eq!(
            budget.fit(MemoryPhase::Read, Price::model(0), 0, 0).units,
            0
        );
    }

    /// An unprobed budget falls back to a fixed allowance and never rejects a
    /// hard reservation.
    #[test]
    fn an_unprobed_budget_grants_from_the_fallback_allowance() {
        let mut budget = MemoryBudget::with_available(None);
        let unit = (FALLBACK_ALLOWANCE_BYTES as f64 * READ_SHARE) as u64 / 4;
        assert_eq!(budget.affordable(MemoryPhase::Read, Price::model(unit)), 4);
        budget.reserve(MemoryPhase::EagerInput, u64::MAX).unwrap();
        assert_eq!(budget.remaining(), FALLBACK_ALLOWANCE_BYTES);
        assert_eq!(budget.available(), None);
    }

    #[test]
    fn memory_estimates_saturate_and_unknown_limits_do_not_reject() {
        let budget = MemoryBudget::with_available(None);
        let estimate = budget.estimate(MemoryPhase::Scan, u64::MAX, None);
        assert_eq!(estimate, u64::MAX);
        budget.require(MemoryPhase::Scan, estimate).unwrap();
    }
}
