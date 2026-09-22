# Design

## Context

See proposal.md — Why. The cull removed `runtime/metering.rs` (an `InstanceMeter` with instruction and memory limits shared across interpreter and JIT) and left the interpreter spec forbidding metering hooks. Today the interpreter is the execution path the embedder reaches (`WasmApplication` → `Engine` → `LoadedModule::invoke_function` → `Interpreter`); `TrapCode::ExecutionBudgetExceeded` still exists but nothing produces it; `Memory::size()` already reports owned pages excluding shared ranges.

## Goals / Non-Goals

**Goals:**

- Per-instance, monotonic, queryable instruction count.
- Per-instance committed-page gauge.
- Settable/resettable execution and memory budgets with distinct exhaustion outcomes.
- Keep the engine boring: no new execution modes, no suspension.

**Non-Goals:**

- Safepoints, suspension, or resumable preemption (quantum scheduling).
- Snapshotting or migration (rejected by the sibling project's design intent).
- Instrumenting the AOT (`.aot`) native execution path — a follow-up; the embedder executes `.wasm` through the interpreter today.

## Decisions

### D1 — The meter hangs off the instance, not the thread

`LoadedModule` caches an `Instance` reused across `invoke_function` calls, so an `Arc<InstanceMeter>` on the instance yields cumulative counts across reactor polls automatically, and attribution stays correct when a guest is polled inline on another thread or nested inside a host call. This mirrors the pre-cull design and satisfies `wasm-runtime-core`'s callback-safe lock discipline (the meter lock is never held across a host-function call).

### D2 — Charge per instruction in the interpreter loop, amortised

`run()` gains a charge between fetching each opcode and dispatching it. To avoid a lock per instruction, the counter increments into an interpreter-local accumulator and flushes to the shared meter at control-flow and host-call boundaries, flushing at function return. The budget check uses an interpreter-local snapshot of the meter's count and budget — seeded at the start of each invocation and refreshed at every flush — compared per instruction, so execution stops at the budget boundary even inside a long straight-line block with no flush point; the shared meter stays authoritative at every flush (a concurrently raised budget is picked up on the next local overrun, a lowered one enforced at the next flush).

### D3 — Exhaustion is a distinct, first-class outcome (the seam)

The charge step returns a distinct budget-exhausted result mapped to `TrapCode::ExecutionBudgetExceeded` (already present). Nothing else is coupled to teardown: a future suspension path can replace the trap mapping at a single point without touching the counter, budget, or caller.

### D4 — Budgets are settable and resettable values

`InstanceMeter` carries a budget that the embedder can set and reset between invocations, not a constructor-time parameter. Per-minute ceiling republication (Selium) and a future throttle-to-zero both reuse this; a budget of `None` is unbounded.

### D5 — Memory gauge is owned pages; memory budget is enforced at grow

The gauge reads the same owned-page count `Memory::size()` already exposes (shared ranges are excluded by the existing `len` semantics). The memory budget is checked in the grow path before `mprotect` extends the accessible range, failing with `TrapCode::MemoryLimitExceeded` — distinct from the module's own declared-maximum failure.

## Risks / Trade-offs

- **[Per-instruction overhead]** → amortised flush (D2); the hot loop stays a branch, an addition, and a cached-budget comparison, not a lock.
- **[Lock discipline / re-entrancy]** A host function can re-enter the engine → never hold the meter lock across a host call; charge before/after the dispatch, and flush on frames, not on host calls.
- **[AOT divergence]** The interpreter is instrumented and the AOT path is not → recorded as a follow-up; a `.aot`-executing build would under-meter until then.
- **[Budget overshoot during a host call]** A long host function is not preempted mid-call → overshoot is bounded by the length of one host call; acceptable and consistent with the interpreter's synchronous host-call model.

## Migration Plan

1. Add `InstanceMeter` and the stats/budget API (additive; `ExecutionBudgetExceeded` already exists).
2. Wire charging into the interpreter loop and the grow-time memory budget.
3. Update `wasm-interpreter`'s "no metering hooks" clause (done in the spec delta) and re-export the metering types.

No breaking change on the Wasmtiny side: the new API is additive and the trap code already exists.

## Open Questions

- Whether to also instrument the AOT path once `.aot` artifacts are on the embedder's execution route.
- Whether the memory budget should also be exposed as a combined cap with the module's declared maximum (it currently only lowers it).
