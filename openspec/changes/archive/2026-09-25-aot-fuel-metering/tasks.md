# Tasks

## 1. Meter core (runtime)

- [x] 1.1 Refactor `InstanceMeter` in `crates/core/src/runtime/metering.rs` to lock-free atomics (`AtomicU64` executed, `AtomicU64` execution budget, `AtomicU32` memory budget) preserving the existing public API; verify `cargo test -p wasmtiny runtime::metering` still passes.
- [x] 1.2 Add a `#[repr(C)]` `MeterCells` view over the meter (documented layout: `executed` at 0, `budget` at 8, `u64::MAX` = unbounded) with a stable address for the vmctx; verify a unit test charges through a raw `*const MeterCells` and observes it via `snapshot`.
- [x] 1.3 Handle counter overflow (saturate) and document that an unbounded budget never traps; verify a unit test drives the counter near `u64::MAX` and asserts monotonic saturation without a spurious trap.
- [x] 1.4 Detect wrap-around on both paths and report saturation once per meter at `error` level through the `log` facade (interpreter: saturating `charge`; AOT: the inline charge clamps the cell back to `u64::MAX` with an atomic `umax` and the runtime reports the pinned counter through `snapshot`); verify unit tests for the one-shot guard and an error-level log-capture test.

## 2. vmctx ABI and version

- [x] 2.1 Add the `METER` offset to `VmCtxOffsets` (`crates/aotc/src/environment.rs`) and the `meter` field to `VmCtx` (`crates/core/src/aot/context.rs`), updating `VmCtx::empty()` and both layout doc comments; verify `cargo build -p wasmtiny-aotc -p wasmtiny` succeeds.
- [x] 2.2 Bump `ABI_VERSION` to 3 in `crates/aotc/src/artifact.rs` and `crates/core/src/aot/format.rs`; verify the loader-rejects-mismatched-ABI test still passes and a v2 header is refused.

## 3. Compiler fuel instrumentation (aotc)

- [x] 3.1 Add `USER_TRAP_BUDGET` (`crates/aotc/src/environment.rs`) and map it to a new trap byte 13 in `crates/aotc/src/artifact.rs`; add byte 13 to `crates/core/src/aot/format.rs` mapped to `TrapCode::ExecutionBudgetExceeded`; verify a `trap_code_byte` unit test and a loader mapping test.
- [x] 3.2 Add a static-sizing pre-pass in `crates/aotc/src/translate.rs` that computes each function's total instruction count and each loop header's body count; verify a unit test over a module with a nested loop asserts the expected sizes.
- [x] 3.3 Emit the inline charge (atomic add + budget compare + `trapnz`) at function entry and each loop header, in wasm function bodies only; verify an `aotc` integration test where a loop runs N times and the counter rises by roughly `entry + N * body`.
- [x] 3.4 Confirm the emitted charge produces a recorded trap site and that emission is deterministic; verify the repeated-compilation byte-identical artifact test still passes.
- [x] 3.5 Emit the wrap clamp in the inline charge (unsigned compare against the previous value, cold-block atomic `umax` pinning the cell at `u64::MAX`, budget check on the clamped total); verify a CLIF-level unit test asserts the `umax` clamp and the recorded trap site, and update the golden artifact.

## 4. Runtime wiring (core)

- [x] 4.1 Point `vmctx.meter` at the instance's `MeterCells` during instantiation in `crates/core/src/aot/exec.rs`; verify an AOT test asserts `stats().executed_instructions > 0` after invoking an exported function.
- [x] 4.2 Add execution-budget set/reset to the AOT instance API and remove the "does not charge instructions / always zero" doc comments; verify an AOT test that a configured budget traps with `TrapCode::ExecutionBudgetExceeded` and that a reset budget is honoured on the next invocation.
- [x] 4.3 Verify the memory budget path is unaffected by the meter refactor; verify `cargo test -p wasmtiny` (including the concurrent memory-budget test) passes.

## 5. Documentation

- [x] 5.1 Update `README.md` (execution modes / metering note) and any `docs/` page to describe AOT fuel metering and the ABI v3 regeneration requirement; verify the documented statements match the implemented behaviour.
- [x] 5.2 Record the artifact-regeneration requirement for the owning repo; verify the loader's ABI error message names the version and the remedy.

## 6. Integration verification

- [x] 6.1 End-to-end: AOT budget exhaustion traps distinctly, the counter is monotonic across invocations, host-function calls are excluded, and concurrent `invoke_shared` charging is safe; verify with dedicated `aotc`/`core` tests.
- [x] 6.2 Unbudgeted AOT execution remains behaviourally identical to the interpreter (existing parity tests); verify the parity suite passes unchanged.
- [x] 6.3 Run `cargo fmt --all`, `cargo clippy -- -D warnings`, and `cargo test` clean before the change is considered done.

## 7. Invocation-local fuel cells (concurrency contention fix)

- [x] 7.1 Add invocation-local cell support to `InstanceMeter` in `crates/core/src/runtime/metering.rs` (`invocation_cells` seeded with the remaining allowance, `drain_invocation_cells` committing and refreshing it, never trapping); verify unit tests for seeding, commit+refresh, and over-budget drains.
- [x] 7.2 Move both AOT entry points (`invoke`, `invoke_shared`) onto a shared `invoke_with_local_meter` wrapper in `crates/core/src/aot/exec.rs` that points the invocation's context copy at its own cell, publishes it on the calling thread, and drains it on return (success and trap paths); verify the existing fuel, budget, and concurrent-charging tests still pass unchanged.
- [x] 7.3 Drain and refresh at the host-call boundary inside `host_call`, using a thread-local invocation scope (restored on return for host-initiated re-entry) whose owner address rejects cross-instance drains; verify tests that a budget raised and a budget lowered mid-invocation both take effect from the next host call.
- [x] 7.4 Pin the regression: add `two_workers_beat_serial_wall_time_on_a_hot_loop` to `crates/aotc/tests/fuel_metering.rs` (guarded on `available_parallelism >= 2`, min-of-3 rounds); verify it fails with the charge pointed back at the shared cells (~1.7x slower than serial) and passes with the local cell.
- [x] 7.5 Add scenario coverage for the new guarantees (charges land on a trapping invocation, counter monotonic while concurrent invocations are in flight, budget reset visible at the next host call); verify with `crates/aotc/tests/fuel_metering.rs`.
- [x] 7.6 Update the contract documentation (`VmCtx` and its per-invocation section, `aot/context.rs`; `MeterCellsOffsets`, `VmCtxOffsets::METER`, and `emit_fuel_charge` in `crates/aotc/src/environment.rs`; the README metering section; `docs/threat-model.md` TM-05) to describe the invocation-local cell, the flush points, and the allowance model.
- [x] 7.7 Run `cargo fmt --all`, `cargo clippy --all-targets --features security-test -- -D warnings`, and the full `cargo test` suite clean.
