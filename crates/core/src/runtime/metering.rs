use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::{Result, TrapCode, WasmError};

use crate::memory::PAGE_SIZE_BYTES;

/// Sentinel for "no execution budget": an unbounded execution ceiling.
///
/// Stored in the meter's `execution_budget` cell; the inline AOT check
/// `next > budget` is never true against it (short of counter saturation),
/// so an unbounded budget never traps.
pub const UNBOUNDED_BUDGET: u64 = u64::MAX;
/// Sentinel for "no memory budget": an unbounded committed-page ceiling.
pub const UNBOUNDED_MEMORY_BUDGET: u32 = u32::MAX;

/// Snapshot of a per-instance meter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstanceStats {
    /// Executed guest instructions since the instance was created.
    pub executed_instructions: u64,
    /// Committed owned linear-memory pages (shared-region pages excluded).
    pub memory_pages: u32,
    /// Committed owned memory bytes (`memory_pages * PAGE_SIZE_BYTES`).
    pub memory_bytes: u64,
}

/// The lock-free cells compiled AOT code charges directly, reached through the
/// `vmctx.meter` field.
///
/// This is a `#[repr(C)]` view over the first two fields of [`InstanceMeter`]:
/// the layout below is *the* ABI contract with `wasmtiny-aotc` (see
/// `environment::MeterCellsOffsets` there), duplicated by design because the
/// runtime never links the compiler.
///
/// ```text
///   0: executed — AtomicU64: monotonically increasing fuel consumed
///   8: budget   — AtomicU64: the execution budget; `u64::MAX` means unbounded
/// ```
///
/// Compiled code loads the pointer once from `vmctx.meter` and, at each charge
/// point, does an atomic add on `executed` and traps inline when the new value
/// exceeds `budget`. The runtime decides what that pointer names: the instance
/// meter's own cells (address-stable for the instance's lifetime behind an
/// `Arc`) on the shared context, or an invocation-local cell on the
/// per-invocation context copy, which the runtime drains back here at flush
/// points (see `InstanceMeter::invocation_cells` and
/// `InstanceMeter::drain_invocation_cells`). Either way the cells outlive
/// every compiled charge that reaches them.
#[repr(C)]
pub struct MeterCells {
    /// Monotonically increasing fuel consumed (saturating at `u64::MAX`).
    pub executed: AtomicU64,
    /// The allowance the current charge is checked against; `u64::MAX` means
    /// unbounded.
    ///
    /// On the instance meter's own cells this is the instance budget itself; on
    /// an invocation-local cell it is the allowance remaining at the
    /// invocation's most recent flush point.
    pub budget: AtomicU64,
}

/// Per-instance instruction accounting and configurable budgets.
///
/// The meter is lock-free: every field is an atomic, so a stable pointer into
/// it (`*const MeterCells`) can be handed to compiled AOT code, which charges
/// with a single atomic add — no per-charge lock or function call.
///
/// The executed count is monotonic for the instance's lifetime:
/// [`charge`](Self::charge) only ever adds units (saturating at `u64::MAX`),
/// and setting or resetting budgets never rewinds the count, so consecutive
/// [`snapshot`](Self::snapshot) samples are non-decreasing.
///
/// # Layout
///
/// `#[repr(C)]`, with [`MeterCells`] overlaying the first two fields:
/// `executed` at offset 0 and `execution_budget` at offset 8 (see
/// [`cells`](Self::cells)). The field order must not change without bumping
/// the artifact ABI version.
#[repr(C)]
#[derive(Debug)]
pub struct InstanceMeter {
    /// Executed fuel (guest work only), saturating at `u64::MAX`.
    executed: AtomicU64,
    /// Execution budget; `UNBOUNDED_BUDGET` (`u64::MAX`) means unbounded.
    execution_budget: AtomicU64,
    /// Memory budget in committed pages; `UNBOUNDED_MEMORY_BUDGET` means
    /// unbounded.
    memory_budget: AtomicU32,
    /// Whether the saturation report has already been emitted for this
    /// meter (one-shot guard, not part of the compiled-code ABI).
    saturation_logged: AtomicBool,
}

impl InstanceMeter {
    /// Creates a new meter with no budgets configured (unbounded).
    pub fn new() -> Self {
        Self::default()
    }

    /// A stable `*const MeterCells` view over this meter, for the `vmctx`.
    ///
    /// The pointer is valid for the meter's lifetime; the vmctx field is set
    /// once at instantiation and read by compiled code at every charge point.
    pub fn cells(&self) -> *const MeterCells {
        self as *const InstanceMeter as *const MeterCells
    }

    /// Charges `units` executed guest instructions against the meter.
    ///
    /// The units are always added to the count. When the resulting count
    /// exceeds the configured execution budget, execution is stopped with
    /// [`TrapCode::ExecutionBudgetExceeded`]. The count saturates at
    /// `u64::MAX` rather than wrapping; an unbounded budget
    /// ([`UNBOUNDED_BUDGET`]) therefore never traps, even on saturation.
    /// Saturation — only reachable after ~2^64 metering units — is reported
    /// once per meter at `error` level (see [`Self::note_saturation`]).
    pub fn charge(&self, units: u64) -> Result<()> {
        if units == 0 {
            return Ok(());
        }

        // Saturating add: the counter never wraps. `fetch_update` never fails
        // here (the closure always returns `Some`), so the previous value is
        // always available.
        let previous = self
            .executed
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                match value.checked_add(units) {
                    Some(next) => Some(next),
                    None => {
                        self.note_saturation();
                        Some(u64::MAX)
                    }
                }
            })
            .unwrap_or(u64::MAX);
        let next = previous.saturating_add(units);

        let budget = self.execution_budget.load(Ordering::SeqCst);
        if next > budget {
            return Err(WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
        }

        Ok(())
    }

    /// Returns a snapshot of the executed-instruction count and the memory
    /// usage computed from the provided committed owned-page count.
    ///
    /// This is also the runtime's observation point for saturation caused by
    /// the AOT inline charge (whose atomic add wraps; the emitted code clamps
    /// the cell back to `u64::MAX`, so a pinned counter is reported here once
    /// at `error` level).
    pub fn snapshot(&self, memory_pages: u32) -> InstanceStats {
        let executed = self.executed.load(Ordering::SeqCst);
        if executed == u64::MAX {
            self.note_saturation();
        }
        InstanceStats {
            executed_instructions: executed,
            memory_pages,
            memory_bytes: memory_pages as u64 * PAGE_SIZE_BYTES as u64,
        }
    }

    /// Returns the authoritative executed-instruction count and execution
    /// budget (crate-internal). The interpreter caches this locally to bound
    /// budget overshoot between flushes; the meter itself remains
    /// authoritative at every flush.
    pub(crate) fn execution_state(&self) -> (u64, Option<u64>) {
        let executed = self.executed.load(Ordering::SeqCst);
        let budget = self.execution_budget.load(Ordering::SeqCst);
        (
            executed,
            if budget == UNBOUNDED_BUDGET {
                None
            } else {
                Some(budget)
            },
        )
    }

    /// The execution allowance still available before the budget is
    /// exhausted: the budget minus the units already executed (saturating at
    /// zero), or [`UNBOUNDED_BUDGET`] when no budget is configured.
    ///
    /// The AOT path stamps this into an invocation-local cell's `budget`
    /// field, so a single invocation traps when it *alone* exhausts what the
    /// instance had left — the same bound the interpreter derives from its
    /// cached `(count, budget)` snapshot between flushes.
    fn remaining_allowance(&self) -> u64 {
        match self.execution_state() {
            (executed, Some(budget)) => budget.saturating_sub(executed),
            (_, None) => UNBOUNDED_BUDGET,
        }
    }

    /// Creates an invocation-local fuel cell seeded with the current remaining
    /// allowance (crate-internal).
    ///
    /// The AOT path charges this cell rather than the shared `executed` cell,
    /// so invocations running on different threads never contend on one cache
    /// line: compiled code's atomic add stays on a line that only the invoking
    /// thread touches. The runtime drains the cell into the authoritative
    /// counter and refreshes it at flush points (see
    /// [`drain_invocation_cells`](Self::drain_invocation_cells)); the cell is
    /// single-threaded by construction, discovered by compiled code through
    /// the per-invocation `vmctx` copy.
    pub(crate) fn invocation_cells(&self) -> MeterCells {
        MeterCells {
            executed: AtomicU64::new(0),
            budget: AtomicU64::new(self.remaining_allowance()),
        }
    }

    /// Drains an invocation-local fuel cell into this meter — the
    /// authoritative counter — and refreshes the cell's allowance, returning
    /// the refreshed remaining allowance (crate-internal).
    ///
    /// The charges always land: the shared counter is the billing authority,
    /// and a flush must not rewrite history. The budget check in
    /// [`charge`](Self::charge) is therefore deliberately discarded — the
    /// inline allowance check at each charge point owns trapping — which also
    /// means a flush can never fail an invocation that has already finished.
    ///
    /// Refreshing the allowance (rather than recomputing it only at entry) is
    /// what makes a budget raise or reset visible to the remainder of a
    /// running invocation at its next flush point, matching the interpreter's
    /// refreshed `(count, budget)` snapshot.
    pub(crate) fn drain_invocation_cells(&self, local: &MeterCells) -> u64 {
        let units = local.executed.swap(0, Ordering::SeqCst);
        if units != 0 {
            let _ = self.charge(units);
        }
        let remaining = self.remaining_allowance();
        local.budget.store(remaining, Ordering::SeqCst);
        remaining
    }

    /// Sets the execution budget; `None` resets it (unbounded execution).
    pub fn set_execution_budget(&self, budget: Option<u64>) -> Result<()> {
        self.execution_budget
            .store(budget.unwrap_or(UNBOUNDED_BUDGET), Ordering::SeqCst);
        Ok(())
    }

    /// Sets the memory budget in committed pages; `None` resets it (unbounded).
    pub fn set_memory_budget(&self, budget: Option<u32>) -> Result<()> {
        self.memory_budget
            .store(budget.unwrap_or(UNBOUNDED_MEMORY_BUDGET), Ordering::SeqCst);
        Ok(())
    }

    /// Ensures a grow to `new_total_pages` does not exceed the memory budget.
    pub(crate) fn ensure_memory_pages(&self, new_total_pages: u32) -> Result<()> {
        let budget = self.memory_budget.load(Ordering::SeqCst);
        if budget != UNBOUNDED_MEMORY_BUDGET && new_total_pages > budget {
            return Err(WasmError::Trap(TrapCode::MemoryLimitExceeded));
        }

        Ok(())
    }

    /// Reports counter saturation once per meter at `error` level.
    ///
    /// Saturation is only reachable after ~2^64 metering units — practically
    /// impossible — but it is the one condition under which the meter stops
    /// being a reliable billing/enforcement signal (the counter stays pinned
    /// at `u64::MAX`, and on the AOT path a budget configured *after*
    /// saturation still traps because the emitted check compares against the
    /// clamped total). Emitting an error-level record through the `log`
    /// facade lets embedders detect it in production.
    fn note_saturation(&self) {
        if !self.saturation_logged.swap(true, Ordering::Relaxed) {
            log::error!(
                "instance meter saturated: the fuel counter reached u64::MAX \
                 (~2^64 metering units); further charges no longer increase it"
            );
        }
    }

    /// Whether the saturation report has already fired for this meter
    /// (test-only observation of the one-shot guard).
    #[cfg(test)]
    fn saturation_logged(&self) -> bool {
        self.saturation_logged.load(Ordering::Relaxed)
    }
}

impl Default for InstanceMeter {
    fn default() -> Self {
        Self {
            executed: AtomicU64::new(0),
            execution_budget: AtomicU64::new(UNBOUNDED_BUDGET),
            memory_budget: AtomicU32::new(UNBOUNDED_MEMORY_BUDGET),
            saturation_logged: AtomicBool::new(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_charge_accumulates_into_snapshot() {
        let meter = InstanceMeter::new();
        meter.charge(3).unwrap();
        meter.charge(4).unwrap();

        let stats = meter.snapshot(2);
        assert_eq!(stats.executed_instructions, 7);
        assert_eq!(stats.memory_pages, 2);
        assert_eq!(stats.memory_bytes, 2 * PAGE_SIZE_BYTES as u64);
    }

    #[test]
    fn test_snapshot_is_monotonic() {
        let meter = InstanceMeter::new();
        let mut previous = meter.snapshot(0).executed_instructions;
        for units in [1, 2, 5, 100] {
            meter.charge(units).unwrap();
            let current = meter.snapshot(0).executed_instructions;
            assert!(current >= previous, "count must not decrease");
            previous = current;
        }

        // Setting and resetting budgets must never rewind the count.
        meter.set_execution_budget(Some(10_000)).unwrap();
        meter.set_execution_budget(None).unwrap();
        meter.set_memory_budget(Some(16)).unwrap();
        meter.set_memory_budget(None).unwrap();
        let after = meter.snapshot(0).executed_instructions;
        assert_eq!(after, previous);
    }

    #[test]
    fn test_budget_exceeded_traps_distinctly() {
        let meter = InstanceMeter::new();
        meter.set_execution_budget(Some(5)).unwrap();
        meter.charge(5).unwrap();

        let error = meter.charge(1).unwrap_err();
        assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));

        // The over-budget charge still lands in the count.
        assert_eq!(meter.snapshot(0).executed_instructions, 6);
    }

    #[test]
    fn test_unset_budget_is_unbounded() {
        let meter = InstanceMeter::new();
        for _ in 0..10_000 {
            meter.charge(1).unwrap();
        }
        assert_eq!(meter.snapshot(0).executed_instructions, 10_000);
    }

    #[test]
    fn test_memory_budget_enforcement_and_reset() {
        let meter = InstanceMeter::new();
        meter.set_memory_budget(Some(4)).unwrap();
        meter.ensure_memory_pages(4).unwrap();

        let error = meter.ensure_memory_pages(5).unwrap_err();
        assert_eq!(error, WasmError::Trap(TrapCode::MemoryLimitExceeded));

        // Resetting to `None` lifts the ceiling.
        meter.set_memory_budget(None).unwrap();
        meter.ensure_memory_pages(100).unwrap();
    }

    #[test]
    fn invocation_cells_seed_the_remaining_allowance() {
        let meter = InstanceMeter::new();
        // Unbounded: the allowance is the unbounded sentinel, so an
        // invocation's inline check can never trap.
        let unbounded = meter.invocation_cells();
        assert_eq!(unbounded.executed.load(Ordering::SeqCst), 0);
        assert_eq!(unbounded.budget.load(Ordering::SeqCst), UNBOUNDED_BUDGET);

        // Finite budget: the allowance is what is *left*, not the budget.
        meter.set_execution_budget(Some(100)).unwrap();
        meter.charge(30).unwrap();
        let bounded = meter.invocation_cells();
        assert_eq!(bounded.executed.load(Ordering::SeqCst), 0);
        assert_eq!(bounded.budget.load(Ordering::SeqCst), 70);

        // Already over budget: nothing left to grant.
        meter.set_execution_budget(Some(50)).unwrap();
        // The over-budget charge still lands (it reports the trap, but the
        // count remains the authority) and leaves no allowance.
        let error = meter.charge(30).unwrap_err();
        assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
        assert_eq!(meter.snapshot(0).executed_instructions, 60);
        assert_eq!(
            meter.invocation_cells().budget.load(Ordering::SeqCst),
            0,
            "an exhausted budget grants no allowance"
        );
    }

    #[test]
    fn draining_an_invocation_cell_commits_and_refreshes() {
        let meter = InstanceMeter::new();
        meter.set_execution_budget(Some(100)).unwrap();

        let local = meter.invocation_cells();
        local.executed.store(25, Ordering::SeqCst);
        let remaining = meter.drain_invocation_cells(&local);

        // The units landed in the authoritative counter and the local cell
        // was reset and re-seeded with what is left.
        assert_eq!(meter.snapshot(0).executed_instructions, 25);
        assert_eq!(remaining, 75);
        assert_eq!(local.executed.load(Ordering::SeqCst), 0);
        assert_eq!(local.budget.load(Ordering::SeqCst), 75);

        // Draining an empty cell changes nothing.
        assert_eq!(meter.drain_invocation_cells(&local), 75);
        assert_eq!(meter.snapshot(0).executed_instructions, 25);
    }

    #[test]
    fn draining_never_traps_even_when_over_budget() {
        let meter = InstanceMeter::new();
        meter.set_execution_budget(Some(10)).unwrap();

        // The invocation was granted an allowance of 10 but accumulated 50
        // (the interpreter's overshoot window, widened to a whole flush
        // interval). Committing must record the work — a finished invocation
        // cannot be failed retroactively — and must leave no allowance.
        let local = meter.invocation_cells();
        local.executed.store(50, Ordering::SeqCst);
        assert_eq!(meter.drain_invocation_cells(&local), 0);
        assert_eq!(meter.snapshot(0).executed_instructions, 50);
    }

    #[test]
    fn a_raised_budget_is_visible_at_the_next_refresh() {
        let meter = InstanceMeter::new();
        meter.set_execution_budget(Some(10)).unwrap();
        let local = meter.invocation_cells();
        assert_eq!(local.budget.load(Ordering::SeqCst), 10);

        // An embedder raises the ceiling mid-invocation: the refresh at the
        // flush point grants the new headroom.
        meter.set_execution_budget(Some(500)).unwrap();
        assert_eq!(meter.drain_invocation_cells(&local), 500);
        assert_eq!(local.budget.load(Ordering::SeqCst), 500);

        // Resetting to unbounded restores the unbounded sentinel.
        meter.set_execution_budget(None).unwrap();
        assert_eq!(meter.drain_invocation_cells(&local), UNBOUNDED_BUDGET);
    }

    #[test]
    fn meter_cells_layout_matches_the_documented_offsets() {
        // The compiler's `MeterCellsOffsets` (executed at 0, budget at 8) must
        // match the runtime view exactly.
        assert_eq!(std::mem::offset_of!(InstanceMeter, executed), 0);
        assert_eq!(std::mem::offset_of!(InstanceMeter, execution_budget), 8);
        assert_eq!(std::mem::offset_of!(MeterCells, executed), 0);
        assert_eq!(std::mem::offset_of!(MeterCells, budget), 8);
    }

    #[test]
    fn charging_through_a_raw_meter_cells_view_is_observed_by_snapshot() {
        let meter = InstanceMeter::new();
        let cells: *const MeterCells = meter.cells();
        assert_eq!(
            cells as *const InstanceMeter,
            &meter as *const InstanceMeter
        );

        // Charge exactly as compiled code does: an atomic add on `executed`.
        // SAFETY: `cells` points into the live `meter` above.
        let previous = unsafe { (*cells).executed.fetch_add(5, Ordering::SeqCst) };
        assert_eq!(previous, 0);

        // The runtime observes the compiled-code charge through `snapshot`.
        assert_eq!(meter.snapshot(0).executed_instructions, 5);

        // And a budget written through the runtime is visible to the compiled
        // path's budget cell.
        meter.set_execution_budget(Some(4)).unwrap();
        // SAFETY: `cells` still points into the live `meter`.
        let budget = unsafe { (*cells).budget.load(Ordering::SeqCst) };
        assert_eq!(budget, 4);
    }

    #[test]
    fn overflow_saturates_and_never_traps_without_a_budget() {
        let meter = InstanceMeter::new();
        // Drive the counter right up to the saturation point.
        meter.charge(u64::MAX - 1).unwrap();
        assert_eq!(meter.snapshot(0).executed_instructions, u64::MAX - 1);

        // The next charge overflows; it saturates rather than wrapping and
        // does not trap while the budget is unbounded.
        meter.charge(5).unwrap();
        assert_eq!(meter.snapshot(0).executed_instructions, u64::MAX);

        // Saturation is monotonic and further charges stay pinned.
        meter.charge(1).unwrap();
        assert_eq!(meter.snapshot(0).executed_instructions, u64::MAX);
    }

    #[test]
    fn overflow_traps_under_a_finite_budget() {
        let meter = InstanceMeter::new();
        // Drive the counter up while unbounded, then impose a finite budget.
        meter.charge(u64::MAX - 1).unwrap();
        meter.set_execution_budget(Some(10)).unwrap();
        // The saturated counter exceeds any finite budget, so the charge traps.
        let error = meter.charge(1).unwrap_err();
        assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
        assert_eq!(meter.snapshot(0).executed_instructions, u64::MAX);
    }

    #[test]
    fn saturation_report_fires_once_per_meter() {
        // The interpreter-side saturation path (`charge`).
        let meter = InstanceMeter::new();
        assert!(!meter.saturation_logged());
        meter.charge(u64::MAX - 1).unwrap();
        assert!(!meter.saturation_logged(), "landing below the cap is fine");
        meter.charge(5).unwrap();
        assert!(meter.saturation_logged(), "the overflowing charge reports");
        // Further saturated charges must not re-report.
        meter.charge(1).unwrap();
        assert!(meter.saturation_logged());

        // The AOT-side observation path (`snapshot`): compiled code clamps
        // the cell to u64::MAX after a wrapping add, so a pinned counter
        // observed through `snapshot` reports exactly once.
        let pinned = InstanceMeter::new();
        pinned.executed.store(u64::MAX, Ordering::SeqCst);
        assert!(pinned.snapshot(0).executed_instructions == u64::MAX);
        assert!(pinned.saturation_logged(), "a pinned counter reports");
        pinned.snapshot(0);
        assert!(pinned.saturation_logged());
    }

    /// A minimal `log` implementation that counts `error!` records, so the
    /// saturation report is observable in tests. Only
    /// `saturation_report_is_emitted_at_error_level` installs it.
    static SATURATION_ERRORS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    struct CountingLogger;

    impl log::Log for CountingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if record.level() == log::Level::Error {
                SATURATION_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn flush(&self) {}
    }

    #[test]
    fn saturation_report_is_emitted_at_error_level() {
        // May already be set if another test installed a logger first; the
        // assertions below tolerate records from other meters.
        static COUNTING_LOGGER: CountingLogger = CountingLogger;
        let _ = log::set_logger(&COUNTING_LOGGER);
        log::set_max_level(log::LevelFilter::Error);

        // Other tests saturate their own meters concurrently, so only assert
        // that driving this fresh meter to saturation emits at least one
        // error-level record (the once-per-meter semantics are covered above).
        let before = SATURATION_ERRORS.load(Ordering::Relaxed);
        let meter = InstanceMeter::new();
        meter.charge(u64::MAX - 1).unwrap();
        meter.charge(5).unwrap();
        let after = SATURATION_ERRORS.load(Ordering::Relaxed);
        assert!(after > before, "saturation must be reported at error level");
    }
}
