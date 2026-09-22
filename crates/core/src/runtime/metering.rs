use super::{Result, TrapCode, WasmError};
use parking_lot::RwLock;

use crate::memory::PAGE_SIZE_BYTES;

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

/// Per-instance instruction accounting and configurable budgets.
///
/// The executed-instruction count is monotonic for the instance's lifetime:
/// [`charge`](Self::charge) only ever adds units, and setting or resetting
/// budgets never rewinds the count, so consecutive
/// [`snapshot`](Self::snapshot) samples are non-decreasing.
#[derive(Debug, Default)]
pub struct InstanceMeter {
    state: RwLock<InstanceMeterState>,
}

#[derive(Debug, Clone, Copy, Default)]
struct InstanceMeterState {
    executed_instructions: u64,
    execution_budget: Option<u64>,
    memory_budget: Option<u32>,
}

impl InstanceMeter {
    /// Creates a new meter with no budgets configured (unbounded).
    pub fn new() -> Self {
        Self::default()
    }

    /// Charges `units` executed guest instructions against the meter.
    ///
    /// The units are always added to the count. When the resulting count
    /// exceeds the configured execution budget, execution is stopped with
    /// [`TrapCode::ExecutionBudgetExceeded`].
    pub fn charge(&self, units: u64) -> Result<()> {
        if units == 0 {
            return Ok(());
        }

        let mut state = self.state.write();
        let Some(next) = state.executed_instructions.checked_add(units) else {
            state.executed_instructions = u64::MAX;
            if state.execution_budget.is_some() {
                return Err(WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
            }
            return Err(WasmError::Runtime(
                "execution instruction count overflowed".to_string(),
            ));
        };

        state.executed_instructions = next;
        if let Some(budget) = state.execution_budget
            && next > budget
        {
            return Err(WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
        }

        Ok(())
    }

    /// Returns a snapshot of the executed-instruction count and the memory
    /// usage computed from the provided committed owned-page count.
    pub fn snapshot(&self, memory_pages: u32) -> InstanceStats {
        let state = *self.state.read();
        InstanceStats {
            executed_instructions: state.executed_instructions,
            memory_pages,
            memory_bytes: memory_pages as u64 * PAGE_SIZE_BYTES as u64,
        }
    }

    /// Returns the authoritative executed-instruction count and execution
    /// budget (crate-internal). The interpreter caches this locally to bound
    /// budget overshoot between flushes; the meter itself remains
    /// authoritative at every flush.
    pub(crate) fn execution_state(&self) -> (u64, Option<u64>) {
        let state = *self.state.read();
        (state.executed_instructions, state.execution_budget)
    }

    /// Sets the execution budget; `None` resets it (unbounded execution).
    pub fn set_execution_budget(&self, budget: Option<u64>) -> Result<()> {
        self.state.write().execution_budget = budget;
        Ok(())
    }

    /// Sets the memory budget in committed pages; `None` resets it (unbounded).
    pub fn set_memory_budget(&self, budget: Option<u32>) -> Result<()> {
        self.state.write().memory_budget = budget;
        Ok(())
    }

    /// Ensures a grow to `new_total_pages` does not exceed the memory budget.
    pub(crate) fn ensure_memory_pages(&self, new_total_pages: u32) -> Result<()> {
        let state = self.state.read();
        if let Some(budget) = state.memory_budget
            && new_total_pages > budget
        {
            return Err(WasmError::Trap(TrapCode::MemoryLimitExceeded));
        }

        Ok(())
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
}
