//! The run's memory budget (#779 F1, #716): one probe, one allowance, and
//! the grants every phase draws from it.
//!
//! A phase that must hold some data resident whatever it costs (the eager
//! input, the pass-1 feature table) [`reserve`](MemoryBudget::reserve)s it
//! and fails with [`ConvertError::BudgetExceeded`] when it cannot fit. A phase
//! whose footprint is a choice (how many read-ahead batches, how wide the
//! pass-2 window, how many row groups the drain encodes at once) asks
//! [`fit`](MemoryBudget::fit) how many units of its [`Price`] it may hold.
//! Both charge the budget, so every phase sized afterwards sees only what is
//! left.
//!
//! The [`MemoryProfile`] decides what `fit` grants, the same way for every
//! phase:
//!
//! - `bounded` keeps main's caps per phase; the allowance can only shrink
//!   them ([`MemoryPhase::bounded_cap`], #788).
//! - `auto` takes what the phase's share of the allowance affords, clamped
//!   to the phase's floor and what it wants.
//! - `speed` takes what it wants.
//!
//! Every profile gets at least the phase's floor.

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

/// Share of the allowance the writer's concurrent row-group encodes may hold
/// (#545). They are resident beside the sink they drain, so they take a
/// quarter and leave the rest to it.
const ENCODE_SHARE: f64 = 0.25;

/// Share of the available memory the Python-facing loss list may hold
/// before exact reports stream from spill instead.
const PYTHON_LOSS_SHARE: f64 = 0.10;

/// Fewest row groups a non-`speed` writer encodes at once, pool permitting:
/// #545's A/B kept 4 as the point below which the drain serializes.
const ENCODE_FLOOR: usize = 4;

/// Fraction of the probed memory export's waves, level stores and tile
/// encodes may hold (#303, #311, #788 E1), as export's own budget always
/// was (`EXPORT_WAVE_RAM_FRACTION`): below the allowance, because the waves
/// sit on top of export state no estimate models (scan accumulators, decode
/// buffers, the writer's directory).
const EXPORT_FRACTION: f64 = 0.5;

/// [`EXPORT_FRACTION`] as a share of the allowance, which is itself
/// [`ALLOWANCE_FRACTION`] of the probed figure: with nothing granted, the
/// export phases afford exactly half of the probed memory, the slots
/// export always sized its waves to.
const EXPORT_SHARE: f64 = EXPORT_FRACTION / ALLOWANCE_FRACTION;

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
    Encode,
    /// The pass-2 output sink (it takes [`MemoryBudget::remaining`]).
    Write,
    /// Spill queues (track C1), and under a cgroup limit the page cache a
    /// spilling pass 2 reads its spill back through (#716).
    Spill,
    /// Export's tile collection (track E1).
    ExportCollect,
    /// Export's concurrent tile encodes (track E1).
    ExportEncode,
    /// Coverage fill (track F).
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

    /// Main's `bounded` cap for this phase, out of `want` units with `floor`
    /// at the least: what a `bounded` grant may never exceed (#788). The
    /// read-ahead and the compute window wanted what the caller asked for
    /// and were cut by the allowance, as now, so their cap is `want` (the
    /// read shape applies main's per-worker caps itself, `read.rs`). The
    /// writer encoded at most [`ENCODE_FLOOR`] row groups, which is the
    /// encode floor. Export and fill held no store under `bounded`: their
    /// floor is main's shape. Remote staging held at most main's staging
    /// byte budget in flight ([`StageAdmission::mem_budget`], 256 MiB or
    /// the env override): `stage_units` is how many units that holds.
    ///
    /// [`StageAdmission::mem_budget`]: crate::input::StageAdmission::mem_budget
    fn bounded_cap(self, want: usize, floor: usize, stage_units: usize) -> usize {
        match self {
            Self::Encode | Self::ExportCollect | Self::ExportEncode | Self::Fill => floor,
            Self::Stage => stage_units.min(want),
            Self::EagerInput
            | Self::Scan
            | Self::Assign
            | Self::Read
            | Self::Compute
            | Self::Write
            | Self::Spill => want,
        }
    }

    /// The share of the allowance an elastic phase may be granted.
    fn share(self) -> f64 {
        match self {
            Self::Read => READ_SHARE,
            Self::Compute => COMPUTE_SHARE,
            Self::Encode => ENCODE_SHARE,
            Self::ExportCollect | Self::ExportEncode => EXPORT_SHARE,
            Self::EagerInput
            | Self::Scan
            | Self::Assign
            | Self::Stage
            | Self::Write
            | Self::Spill
            | Self::Fill => 1.0,
        }
    }
}

/// Where a [`Price`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PriceBasis {
    /// Measured on data this run already holds (a built batch, a sample).
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
    /// The staging knobs [`stage_requests`](Self::stage_requests) and
    /// `bounded`'s staging cap read (#788).
    stage: crate::input::StageAdmission,
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
            stage: crate::input::stage_admission(),
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
        self.available()
            .map_or(cap, |b| ((b as f64 * PYTHON_LOSS_SHARE) as u64).min(cap)) as usize
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
    /// Units of `price` that `phase` may hold under `profile`, at least 1. A
    /// query: it charges nothing. `speed` affords everything; `auto` and
    /// `bounded` what `phase`'s share of the current allowance holds
    /// (`bounded` then applies main's caps on top, in [`fit`](Self::fit) and
    /// the read shape).
    pub(super) fn affordable(
        &self,
        profile: MemoryProfile,
        phase: MemoryPhase,
        price: Price,
    ) -> usize {
        match profile {
            MemoryProfile::Speed => usize::MAX,
            MemoryProfile::Auto | MemoryProfile::Bounded => {
                // Rounded up: a share that is exactly a whole number of
                // units (export's half of a round probed figure) must not
                // lose one to the allowance's own rounding down.
                let ceiling = (self.remaining() as f64 * phase.share()).ceil() as u64;
                usize::try_from(ceiling / price.bytes.max(1))
                    .unwrap_or(usize::MAX)
                    .max(1)
            }
        }
    }
    /// Grant `phase` units of `price` under `profile`, never fewer than
    /// `floor` (a phase that cannot run below it): `auto` what its share of
    /// the allowance affords within `[floor, want]`; `bounded` the same, cut
    /// to main's cap for the phase ([`MemoryPhase::bounded_cap`]); `speed`
    /// `want`. The grant is charged to this budget whatever the
    /// profile, so every later phase sees it.
    pub(super) fn fit(
        &mut self,
        profile: MemoryProfile,
        phase: MemoryPhase,
        price: Price,
        want: usize,
        floor: usize,
    ) -> Grant {
        let units = match profile {
            MemoryProfile::Bounded => self
                .affordable(profile, phase, price)
                .min(want)
                .min(phase.bounded_cap(want, floor, self.stage_units(price)))
                .max(floor),
            MemoryProfile::Speed => want.max(floor),
            MemoryProfile::Auto => self.affordable(profile, phase, price).min(want).max(floor),
        };
        let grant = Grant {
            phase,
            units,
            unit_bytes: price.bytes,
        };
        let slot = &mut self.granted[phase as usize];
        *slot = slot.saturating_add(grant.bytes());
        grant
    }
    /// Units of `price` main's staging byte budget holds, at least 1.
    fn stage_units(&self, price: Price) -> usize {
        usize::try_from(self.stage.mem_budget / price.bytes.max(1))
            .unwrap_or(usize::MAX)
            .max(1)
    }
    /// Return a grant once what it priced is no longer resident.
    pub(super) fn release(&mut self, grant: Grant) {
        let slot = &mut self.granted[grant.phase as usize];
        *slot = slot.saturating_sub(grant.bytes());
    }
    /// The price of one pass-2 window batch (#716): the input batch plus
    /// what it fans out to across the levels being built, `fanout` output
    /// rows per input row (the plan's per-level winner counts over the input
    /// rows). From the footers' bytes per row when known; an output row is
    /// priced as a whole input row, an upper bound, since every level copies
    /// the properties and only simplifies the geometry. Without footer bytes,
    /// the per-row model at every level, as before.
    pub(super) fn window_price(
        &self,
        batch_rows: usize,
        row_bytes: Option<u64>,
        fanout: f64,
        levels: usize,
        geometry: Option<u64>,
    ) -> Price {
        match row_bytes {
            Some(row) => {
                let rows = batch_rows as f64 * (1.0 + fanout.max(0.0));
                Price::new(((rows * row as f64) as u64).max(1), PriceBasis::Footer)
            }
            None => Price::model(
                self.estimate(MemoryPhase::Compute, batch_rows as u64, geometry)
                    .saturating_mul(levels.max(1) as u64)
                    .max(1),
            ),
        }
    }
    /// The pass-2 compute window (#494, #716): `requested` batches of
    /// `price` wanted, one at the floor, granted per [`fit`](Self::fit).
    pub(super) fn compute_window(
        &mut self,
        profile: MemoryProfile,
        requested: usize,
        price: Price,
    ) -> Grant {
        self.fit(profile, MemoryPhase::Compute, price, requested.max(1), 1)
    }
    /// Remote staging requests in flight at once (#727, #788), per
    /// [`fit`](Self::fit): the concurrency ceiling wanted, or what
    /// `TYLERTOO_STAGING_MEM_BUDGET_BYTES` holds when it is set, one at the
    /// floor (a request larger than the grant is still admitted alone),
    /// priced at the plan's largest request. `bounded` keeps main's staging
    /// byte budget ([`MemoryPhase::bounded_cap`]).
    pub(super) fn stage_requests(&mut self, profile: MemoryProfile, largest_request: u64) -> Grant {
        let price = Price::new(largest_request.max(1), PriceBasis::Footer);
        let ceiling = self.stage.max_concurrency.max(1);
        let want = if self.stage.mem_budget_set {
            self.stage_units(price).min(ceiling)
        } else {
            ceiling
        };
        self.fit(profile, MemoryPhase::Stage, price, want, 1)
    }
    /// Row groups the writer may encode at once (#296, #545), per
    /// [`fit`](Self::fit): the pool's threads wanted, [`ENCODE_FLOOR`]
    /// (#545's measured floor, main's `bounded` cap) at the floor, priced at
    /// one row group held as Arrow plus its encoded chunks. The grant stays
    /// charged for the run: the writer encodes until it finishes.
    pub(super) fn encode_concurrency(
        &mut self,
        profile: MemoryProfile,
        threads: usize,
        price: Price,
    ) -> Grant {
        let threads = threads.max(1);
        self.fit(
            profile,
            MemoryPhase::Encode,
            price,
            threads,
            threads.min(ENCODE_FLOOR),
        )
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

    /// Reserve up to `bytes` for `phase`, as much as is available, and never
    /// fail: a working set the run is better off holding than not (the
    /// spill's page cache under a cgroup limit). Nothing when unprobed.
    /// Returns what was reserved.
    pub(super) fn reserve_up_to(&mut self, phase: MemoryPhase, bytes: u64) -> u64 {
        let taken = self.available().map_or(0, |available| available.min(bytes));
        let slot = &mut self.reserved[phase as usize];
        *slot = slot.saturating_add(taken);
        taken
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
            // Only the pass-1 scan's floor has the preflight escape hatch.
            assert_eq!(
                error.to_string().contains("TYLERTOO_SKIP_MEMORY_PREFLIGHT"),
                matches!(phase, MemoryPhase::Scan),
                "{error}"
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

    /// A window of `requested` batches priced by the per-row model.
    fn grant_window(
        budget: &mut MemoryBudget,
        requested: usize,
        batch_rows: usize,
        levels: usize,
    ) -> usize {
        let price = budget.window_price(batch_rows, None, 0.0, levels, Some(600));
        budget
            .compute_window(MemoryProfile::Auto, requested, price)
            .units
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
        assert_eq!(
            budget.affordable(MemoryProfile::Auto, MemoryPhase::Read, price),
            6
        );
        assert_eq!(budget.available(), Some(10_000), "a query charges nothing");
        let grant = budget.fit(MemoryProfile::Auto, MemoryPhase::Read, price, 4, 1);
        assert_eq!((grant.units, grant.bytes()), (4, 400), "capped by want");
        assert_eq!(budget.available(), Some(9_600));
        // (6,000 − 400) × 10% = 560 → 5 units now.
        assert_eq!(
            budget
                .fit(MemoryProfile::Auto, MemoryPhase::Compute, price, 100, 1)
                .units,
            5
        );
        assert_eq!(budget.available(), Some(9_100));
        // The floor wins over the share: a phase cannot run below it.
        assert_eq!(
            budget
                .fit(
                    MemoryProfile::Auto,
                    MemoryPhase::Read,
                    Price::model(1 << 40),
                    8,
                    2
                )
                .units,
            2
        );
        // A zero price cannot divide by zero, and want 0 with floor 0 grants
        // nothing.
        assert_eq!(
            budget
                .fit(
                    MemoryProfile::Auto,
                    MemoryPhase::Read,
                    Price::model(0),
                    0,
                    0
                )
                .units,
            0
        );
    }

    /// An unprobed budget falls back to a fixed allowance and never rejects a
    /// hard reservation.
    #[test]
    fn an_unprobed_budget_grants_from_the_fallback_allowance() {
        let mut budget = MemoryBudget::with_available(None);
        let unit = (FALLBACK_ALLOWANCE_BYTES as f64 * READ_SHARE) as u64 / 4;
        assert_eq!(
            budget.affordable(MemoryProfile::Auto, MemoryPhase::Read, Price::model(unit)),
            4
        );
        budget.reserve(MemoryPhase::EagerInput, u64::MAX).unwrap();
        assert_eq!(budget.remaining(), FALLBACK_ALLOWANCE_BYTES);
        assert_eq!(budget.available(), None);
    }

    /// #545: under `auto` the writer's encode concurrency comes from the
    /// budget, not a fixed figure. On main, `bounded` encoded at most 4 row
    /// groups at once and `auto` the whole pool, whatever the budget.
    /// Profiled (`.788/PROFILE-LOG.md`, 64 threads, glibc): the drain is 38%
    /// of pass 2's wall on Moldova and 39% on NZ parcels at 4 encodes,
    /// against 14% at the full pool on the same Moldova output, while the
    /// drain thread itself holds 1.1% of the CPU samples: it waits on the
    /// capped encodes. #545's own A/B kept 4 as the floor and asked for the
    /// count to come from the memory probe. `bounded` keeps main's 4
    /// (`every_profile_grants_by_one_rule`).
    #[test]
    fn bounded_encode_concurrency_comes_from_the_budget() {
        let resolve = |mut budget: MemoryBudget, threads: usize, row_group_bytes: u64| {
            budget
                .encode_concurrency(
                    MemoryProfile::Auto,
                    threads,
                    Price::new(row_group_bytes, PriceBasis::Footer),
                )
                .units
        };
        let row_group = 64 << 20;
        // Ample memory: the pool, not the constant, is the limit.
        assert_eq!(
            resolve(MemoryBudget::with_available(Some(64 << 30)), 16, row_group),
            16
        );
        // Tight memory: never below #545's measured floor of 4 ...
        assert_eq!(
            resolve(MemoryBudget::with_available(Some(256 << 20)), 16, row_group),
            4
        );
        // ... and between the two, what the budget affords.
        let mid = resolve(MemoryBudget::with_available(Some(4 << 30)), 16, row_group);
        assert!((4..16).contains(&mid), "4 GiB, 64 MiB row groups: {mid}");
        // Never more than the pool.
        assert_eq!(
            resolve(MemoryBudget::with_available(Some(64 << 30)), 2, row_group),
            2
        );
    }

    /// The encodes are charged for the run, so the pass-2 read sizes against
    /// what they left; `speed` takes the pool and is still charged.
    #[test]
    fn encode_grants_charge_the_budget_and_speed_takes_the_pool() {
        let mut budget = MemoryBudget::with_available(Some(10_000));
        let grant = budget.encode_concurrency(MemoryProfile::Auto, 8, Price::model(100));
        // 6,000 × 25% = 1,500 → 15 affordable, capped at the pool.
        assert_eq!(grant.units, 8);
        assert_eq!(budget.remaining(), 6_000 - 800);
        let speed = budget.encode_concurrency(MemoryProfile::Speed, 3, Price::model(1 << 40));
        assert_eq!(speed.units, 3);
        budget.release(speed);
        budget.release(grant);
        assert_eq!(budget.remaining(), 6_000);
    }

    /// A soft reservation takes what is there, never fails, and shrinks
    /// every later grant.
    #[test]
    fn reserve_up_to_takes_what_is_available() {
        let mut budget = MemoryBudget::with_available(Some(10_000));
        assert_eq!(budget.reserve_up_to(MemoryPhase::Spill, 4_000), 4_000);
        assert_eq!(budget.remaining(), 3_600);
        assert_eq!(budget.reserve_up_to(MemoryPhase::Spill, u64::MAX), 6_000);
        assert_eq!((budget.available(), budget.remaining()), (Some(0), 0));
        let floor = budget.fit(
            MemoryProfile::Auto,
            MemoryPhase::Read,
            Price::model(10),
            8,
            2,
        );
        assert_eq!(floor.units, 2, "grants fall to their floors");
        assert_eq!(
            MemoryBudget::with_available(None).reserve_up_to(MemoryPhase::Spill, 9),
            0
        );
    }

    #[test]
    fn memory_estimates_saturate_and_unknown_limits_do_not_reject() {
        let budget = MemoryBudget::with_available(None);
        let estimate = budget.estimate(MemoryPhase::Scan, u64::MAX, None);
        assert_eq!(estimate, u64::MAX);
        budget.require(MemoryPhase::Scan, estimate).unwrap();
    }

    /// #788: remote staging requests in flight per profile, for 64 MiB
    /// requests against main's 256 MiB staging budget and a ceiling of 8.
    /// `bounded` keeps main's budget (4 in flight, not 8); `auto` is decided
    /// by the allowance; `speed` takes the ceiling; an env-set budget sizes
    /// every profile alike.
    #[test]
    fn bounded_staging_keeps_main_byte_budget() {
        use crate::input::StageAdmission;
        const MIB: u64 = 1 << 20;
        let largest = 64 * MIB;
        let units = |profile, available: u64, set_budget: Option<u64>| {
            let mut budget = MemoryBudget::with_available(Some(available));
            budget.stage = StageAdmission {
                max_concurrency: 8,
                mem_budget: set_budget.unwrap_or(256 * MIB),
                mem_budget_set: set_budget.is_some(),
            };
            let before = budget.remaining();
            let grant = budget.stage_requests(profile, largest);
            let n = grant.bytes() / largest;
            budget.release(grant);
            assert_eq!(budget.remaining(), before, "released");
            n
        };
        let roomy = 1 << 40;
        assert_eq!(units(MemoryProfile::Bounded, roomy, None), 4);
        assert_eq!(units(MemoryProfile::Auto, roomy, None), 8);
        assert_eq!(units(MemoryProfile::Speed, roomy, None), 8);
        // A tight machine: the allowance (60% of 400 MiB) holds 3.
        assert_eq!(units(MemoryProfile::Auto, 400 * MIB, None), 3);
        assert_eq!(units(MemoryProfile::Bounded, 400 * MIB, None), 3);
        // Never below one request, whatever the budget.
        assert_eq!(units(MemoryProfile::Bounded, MIB, None), 1);
        // TYLERTOO_STAGING_MEM_BUDGET_BYTES=128 MiB: two for every profile.
        for profile in [
            MemoryProfile::Bounded,
            MemoryProfile::Auto,
            MemoryProfile::Speed,
        ] {
            assert_eq!(units(profile, roomy, Some(128 * MIB)), 2);
        }
    }

    /// #788: one rule for every phase. `auto` never grants more than the
    /// phase's share of the allowance unless its floor needs it; `bounded`
    /// the same, never above main's cap for the phase (the floor for the
    /// encodes, export and fill); `speed` what it wants. Each charges the
    /// budget.
    #[test]
    fn every_profile_grants_by_one_rule() {
        let phases = [
            MemoryPhase::EagerInput,
            MemoryPhase::Scan,
            MemoryPhase::Assign,
            MemoryPhase::Stage,
            MemoryPhase::Read,
            MemoryPhase::Compute,
            MemoryPhase::Encode,
            MemoryPhase::Write,
            MemoryPhase::Spill,
            MemoryPhase::ExportCollect,
            MemoryPhase::ExportEncode,
            MemoryPhase::Fill,
        ];
        assert_eq!(phases.len(), MemoryPhase::COUNT);
        let price = Price::model(1_000);
        for available in [10_000u64, 1 << 20, 1 << 30, 64 << 30] {
            for phase in phases {
                for (want, floor) in [(64usize, 4usize), (8, 1), (1, 1), (100_000_000, 2)] {
                    let fresh = MemoryBudget::with_available(Some(available));
                    let mut budget = fresh;
                    let bounded = budget.fit(MemoryProfile::Bounded, phase, price, want, floor);
                    let share = (fresh.remaining() as f64 * phase.share()) as u64;
                    assert!(bounded.units >= floor, "{phase:?} bounded");
                    assert!(
                        bounded.units
                            <= phase
                                .bounded_cap(want, floor, fresh.stage_units(price))
                                .max(floor),
                        "{phase:?} bounded above main's cap"
                    );
                    assert!(
                        bounded.units == floor || bounded.bytes() <= share,
                        "{phase:?} bounded over its share"
                    );
                    if matches!(
                        phase,
                        MemoryPhase::Encode
                            | MemoryPhase::ExportCollect
                            | MemoryPhase::ExportEncode
                            | MemoryPhase::Fill
                    ) {
                        assert_eq!(bounded.units, floor, "{phase:?}: main's shape is its floor");
                    }
                    assert_eq!(budget.remaining(), fresh.remaining() - bounded.bytes());

                    let mut budget = fresh;
                    let auto = budget.fit(MemoryProfile::Auto, phase, price, want, floor);
                    assert!(bounded.units <= auto.units, "{phase:?}: bounded above auto");
                    assert!(auto.units >= floor && auto.units <= want, "{phase:?} auto");
                    assert!(
                        auto.units == floor || auto.bytes() <= share,
                        "{phase:?} auto: {} B over a {share} B share",
                        auto.bytes()
                    );
                    assert!(auto.bytes() <= fresh.remaining() || auto.units == floor);

                    let mut budget = fresh;
                    let speed = budget.fit(MemoryProfile::Speed, phase, price, want, floor);
                    assert_eq!(speed.units, want, "{phase:?} speed");
                    assert_eq!(
                        budget.remaining(),
                        fresh.remaining().saturating_sub(speed.bytes())
                    );
                }
            }
        }
        // The named grants follow the same rule.
        let mut budget = MemoryBudget::with_available(Some(64 << 30));
        let encode = Price::model(1 << 20);
        assert_eq!(
            budget
                .encode_concurrency(MemoryProfile::Bounded, 64, encode)
                .units,
            ENCODE_FLOOR
        );
        assert_eq!(
            budget
                .encode_concurrency(MemoryProfile::Speed, 64, encode)
                .units,
            64
        );
        // Main's bounded window: what was asked, cut by the allowance.
        assert_eq!(
            budget
                .compute_window(MemoryProfile::Bounded, 8, encode)
                .units,
            8
        );
        let mut tight = MemoryBudget::with_available(Some(40 << 20));
        assert_eq!(
            tight
                .compute_window(MemoryProfile::Bounded, 8, encode)
                .units,
            2
        );
        assert_eq!(
            budget.compute_window(MemoryProfile::Speed, 8, encode).units,
            8
        );
    }

    /// #716: with footer bytes, a window batch is priced as its input rows
    /// plus `fanout` output rows per input row, each a whole input row.
    #[test]
    fn a_footer_priced_window_charges_input_and_fanout_rows() {
        let budget = MemoryBudget::with_available(Some(1 << 30));
        // 10 input rows fanning out to 1.5 output rows each: 25 rows of 100 B.
        assert_eq!(
            budget.window_price(10, Some(100), 1.5, 3, None),
            Price::new(2_500, PriceBasis::Footer)
        );
        // A negative fanout counts as none: the input batch alone.
        assert_eq!(
            budget.window_price(10, Some(100), -2.0, 3, None),
            Price::new(1_000, PriceBasis::Footer)
        );
    }
}
