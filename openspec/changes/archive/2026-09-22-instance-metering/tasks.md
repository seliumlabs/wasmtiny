# Tasks

## 1. Meter type and instance API

- [x] 1.1 Add an `InstanceMeter` holding an executed-instruction count and an optional execution budget, with `charge`, `snapshot`, and budget set/reset functions that never expose a non-monotonic count; verify unit tests for charge, snapshot, and monotonicity
- [x] 1.2 Attach the meter to `Instance` so the cached instance reused across `invoke_function` calls accumulates one lifetime count, and expose query/set APIs (`stats`, `set_execution_budget`, `set_memory_budget`); verify an instance test asserting counts persist across repeated invocations of the same loaded module

## 2. Interpreter charging and execution budget

- [x] 2.1 Charge each executed instruction in the interpreter's run loop, amortising the shared-meter write by accumulating locally and flushing at control-flow and function-return boundaries; verify a test executing a small module repeatedly asserts a nonzero, strictly cumulative count
- [x] 2.2 Charge only decoded guest instructions, not host-function execution, by flushing around host-call dispatch rather than charging calls; verify a test with a host import asserting the import's run is not counted
- [x] 2.3 Enforce the execution budget by stopping execution with `TrapCode::ExecutionBudgetExceeded` once the count reaches the budget; verify tests for overrun (traps distinctly) and under-budget (completes normally)
- [x] 2.4 Bound budget overshoot per instruction with an interpreter-local snapshot of the meter's count and budget (seeded per invocation, refreshed at flushes, shared meter authoritative at flush); verify a test asserting an over-budget store in a flush-free straight-line block traps before its effects execute

## 3. Memory gauge and budget

- [x] 3.1 Report committed owned pages per instance through the metering query using `Memory::size()`, excluding shared-region pages; verify a test asserting `memory.grow` reflects in the gauge while attached shared ranges do not
- [x] 3.2 Enforce the memory budget in the grow path before `mprotect` extends the accessible range, failing with `TrapCode::MemoryLimitExceeded`; verify a test asserting an over-budget grow fails with the memory-limit code
- [x] 3.3 Make both budgets settable and resettable between invocations with `None` meaning unbounded; verify tests for reset-mid-life and unbounded-default behaviour

## 4. Public API and repo hygiene

- [x] 4.1 Re-export the metering types from `lib.rs` and confirm each has an embedder (Selium) caller, consistent with the consumer-driven public API requirement; verify `cargo build` and a usage smoke test
- [x] 4.2 Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and the full test suite; verify clean output and no regressions in `wasm-interpreter` and `wasm-runtime-core` tests
