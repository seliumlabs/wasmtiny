# Design

## Context

See `proposal.md` - Why for motivation.

Current state (verified in-tree):

- `AotInstance` (`crates/core/src/aot/exec.rs`) carries an `Arc<InstanceMeter>`
  in its dispatch state, but uses it only for the memory-page budget (checked
  inside the `memory.grow` libcall) and for `stats()`, whose
  `executed_instructions` is documented and tested as always zero. There is no
  `set_execution_budget` on the AOT path.
- The interpreter (`crates/core/src/interpreter/exec.rs`) charges one unit per
  executed operator via a local accumulator, flushing to `InstanceMeter` at
  control-flow boundaries; the budget is enforced at flush points.
- `InstanceMeter` (`crates/core/src/runtime/metering.rs`) is a
  `RwLock<InstanceMeterState>`.
- The vmctx ABI is duplicated by design between the compiler
  (`crates/aotc/src/environment.rs`, `VmCtxOffsets`) and the runtime
  (`crates/core/src/aot/context.rs`, `VmCtx`); `ABI_VERSION` is 2.
- Traps from compiled code are `trapnz` instructions whose byte offsets are
  recorded in each function's trap table, surfaced as a typed `TrapCode` by the
  signal handler; the artifact carries a trap-code byte per trap site.

## Goals / Non-Goals

**Goals:**

- Charge size-weighted fuel on the AOT path and expose a real count through
  `stats()`.
- Enforce a configurable execution budget on the AOT path with the existing
  distinct `TrapCode::ExecutionBudgetExceeded`.
- Keep charging concurrency-safe under `invoke_shared` and cheap enough to run
  on every function entry and loop back-edge.
- Keep one authoritative per-instance meter shared by both execution paths,
  while keeping concurrent invocations of one instance off a single contended
  cache line.

**Non-Goals:**

- Exact instruction parity with the interpreter on the AOT path.
- Deterministic, cross-compiler-version-stable fuel numbers.
- Per-instruction charging, safepoints, or suspension.
- Any change to the interpreter's charging granularity.
- Weighting by instruction class (memory vs arithmetic); see Open Questions.

## Decisions

### 1. Fuel model: size-weighted, charged at function entry and loop back-edge

Charge a function's static instruction count when it is entered, and a loop
body's static instruction count at the loop header (so it is charged once on
entry and once per iteration).

- *Alternative - constant per point (Wasmtime's model):* cheapest, but the count
  becomes "entries + iterations", a poor proxy for work and a meaningless
  billing unit.
- *Alternative - per operator:* exact, but one atomic add per wasm instruction
  is far too costly and inflates code size.
- *Alternative - per basic block:* closer to exact, but requires precise
  block-size accounting through every control-flow split; entry + loop captures
  the dominant cost (loops) with far less machinery.

Rationale: the stated purpose is to *enforce and bill*. Size-weighting keeps the
number roughly proportional to executed instructions while charging at only the
two points that dominate dynamic cost. `wasm-interpreter` remains exact; the
AOT count is a documented approximation.

### 2. Mechanism: inline atomic meter cell reached through a new vmctx field

Add one pointer field to `VmCtx` pointing at a runtime-owned `MeterCells`
struct. Compiled code, at each charge point, does an atomic add on the counter
and traps inline if the result exceeds the budget. No function call per charge.

- *Alternative - a `charge` libcall (like `memory.grow`):* no ABI change, but a
  real indirect call on every entry and loop iteration, which defeats the point
  of fuel metering.

`MeterCells` (layout duplicated in `environment.rs` and `context.rs`):

```text
  0: executed : AtomicU64   monotonically increasing fuel consumed
  8: budget   : AtomicU64   u64::MAX means unbounded
```

Charge sequence emitted at each point (units is a compile-time constant):

```text
  meter  = load [vmctx + METER]
  prev   = atomic_rmw add [meter + 0], units
  next   = prev + units
  over   = next > load [meter + 8]      ; the allowance, see decision 8
  trapnz over, USER_TRAP_BUDGET
```

plus a wrap check (see decision 4): `next < prev` (unsigned) branches to a
cold block that clamps the cell back to `u64::MAX` with an atomic `umax`, and
the budget compare uses the clamped total.

The `trapnz` is an ordinary registered trap site, so surfacing needs no new
machinery: a new `USER_TRAP_BUDGET` maps to a new artifact trap byte that the
loader maps to `TrapCode::ExecutionBudgetExceeded`.

### 3. Refactor `InstanceMeter` to lock-free atomics

Make `executed`, `execution_budget`, and `memory_budget` atomics so a stable
`*const MeterCells` (or a stable interior layout) exists for the vmctx to point
at. `charge` becomes an atomic add; `ensure_memory_pages` and `snapshot` read
atomics.

- *Alternative - a separate atomic cell for AOT alongside the locked
  `InstanceMeter`:* two sources of truth for one instance; rejected.

The `Arc<InstanceMeter>` allocation is stable for the instance's lifetime, so a
raw pointer into it is valid for the vmctx. This keeps a single authoritative
meter for both paths.

### 4. Budget encoding and counter wrap

`budget = u64::MAX` means unbounded; the inline check `next > budget` is then
never true, so unbudgeted execution never traps. On the AOT path the compared
`budget` is the invocation-local allowance rather than the instance budget
itself (decision 8); the check is structurally the same, and an unbounded
instance still yields `u64::MAX` in that field.

The inline atomic add *wraps* rather than saturating, so the emitted charge
detects the wrap explicitly: `next < prev` (unsigned compare, true exactly when
the add wrapped) branches to a cold, predicted-not-taken block that clamps the
cell back to `u64::MAX` with an atomic `umax`, and the budget compare uses the
clamped total. Consequences:

- The observable counter never decreases (the cell stays pinned at `u64::MAX`),
  matching the "counter SHALL NOT decrease" requirement on both paths.
- A finite budget still traps at the wrap point, exactly like the runtime
  helper's saturating `charge`; an unbounded budget never traps.
- Per-charge cost is one extra compare and a predicted-not-taken branch —
  negligible against the atomic add itself.

The runtime helper used by the interpreter saturates directly
(`checked_add` -> `u64::MAX`). Both paths report the saturation once per meter
at `error` level through the `log` facade (the runtime observes the pinned
counter in `charge`/`snapshot`), so the condition is detectable in production.
The whole scenario requires ~2^64 metering units — unreachable in practice
(centuries of charging at billions of charges per second) — but the report
makes the one meter state where the counter stops being a reliable
billing/enforcement signal visible rather than silent.

### 5. ABI version bump

Adding the `vmctx` field changes the compiler/runtime contract, so
`ABI_VERSION` moves 2 -> 3 in both `crates/aotc/src/artifact.rs` and
`crates/core/src/aot/format.rs`. The loader already refuses mismatched ABI
versions, so this is the migration gate. No artifact *format* version change and
no new section: the meter is runtime-provided, and the new trap byte lives in
the existing function-map trap records.

### 6. Static sizing via a pre-pass

The function's static instruction count is needed at the prologue, and each
loop body's count at its header, before translation reaches them. The translator
therefore does a lightweight pre-pass over each function body's operator stream
(mirroring the existing control-stack handling) to compute the function total
and a per-loop-header body size, then emits charges during the real translation
pass. Charges are emitted only into wasm function bodies - never into host-call
stubs or entry trampolines - so host-function work is not charged.

### 7. Spec reconciliation

`instance-metering`'s counter is reworded as metering units: exact instructions
on the interpreter, size-weighted fuel on AOT. `aot-execution`'s native
execution-parity requirement gains a metering carve-out, since a budget trap
fires at a different point than the interpreter's per-instruction count.

### 8. Charge an invocation-local cell; drain it at flush points

The emitted charge is unchanged (decision 2): an atomic add on `vmctx.meter`
plus a budget compare. What changes is *which cells that pointer names*. Every
Rust-level invocation already runs on a copy of the instance context
(`invoke_shared`'s per-invocation copy carries the thread-relative stack bound
and, for a module with a shadow stack, the private stack pointer); it now also
carries a stack-local `MeterCells` whose `budget` field is the **remaining
allowance** — `budget - executed`, saturating, or `u64::MAX` when unbounded.
Compiled code charges that cell, and the runtime drains it into the
authoritative instance meter at flush points: a host-call boundary and the end
of the invocation (trap path included, so charges made before a trap still
land).

Why: pointing the charge at the single shared cell makes every loop iteration
of every invocation write one contended cache line. Two workers running
independent CPU-bound tasks then ping-pong it — measured on a two-worker hot
loop (one instance, two `invoke_shared` callers): serial ~5.0ms vs parallel
~8.3ms, i.e. 1.7x *slower* than serial. With the local cell the per-iteration
atomic stays on a line only that invocation touches, and the shared counter is
written once per host call and once per invocation, so two workers scale with
cores instead of degrading (same loop after the change: parallel < serial).

- *Alternative - batch inside compiled code* (accumulate in a register and
  flush only every Nth back-edge): removes the per-iteration atomic entirely,
  but needs a hidden accumulator threaded through every loop header and every
  branch targeting one (`br`, `br_if`, `br_table`), and leaves unflushed
  residue on trap paths. More translator surface for a smaller win.
- *Alternative - per-thread cells registered with the instance and summed at
  `snapshot`*: no exit flush, but adds per-thread registration, lifetime, and
  reclamation bookkeeping, and turns every `snapshot` into a reduction over
  live threads.
- *Alternative - relax the atomic ordering*: does not help; the cost is the
  shared cache line, not the ordering (Cranelift's atomics are sequentially
  consistent regardless).

Semantics this changes, all deliberate:

- Budget enforcement keeps the interpreter's shape: a cached allowance,
  refreshed at flush points. A reset between invocations applies to the next
  invocation; a reset *during* one applies at its next host call. The
  invocation-local check still bounds the instance's total to within one charge
  of the allowance it was granted, and the advisory-across-threads stance
  (below) is unchanged.
- `stats()` sampled mid-invocation lags by the invoking thread's unflushed
  charges (at most a host-call interval). The count stays monotonic and is
  exact once invocations end; the interpreter has the same property within a
  straight-line block.
- The budget field is per-invocation, so the emitted code must keep loading it
  non-readonly (it already does).
- `call_indirect` and imported-function calls load the callee's context from
  its `FuncDesc` — the *shared* context for this instance's own functions — so
  frames entered that way charge the shared meter directly. That is correct
  accounting (atomic add, right instance, monotonic) and preserves the
  "cross-instance native call charges the callee's instance" contract; it only
  forgoes the private line for indirect-heavy code. Making the per-invocation
  context survive indirect calls needs a new ABI (same-instance callee
  detection) and is out of scope.
- The host-call boundary is reachable from inside `host_call` even though
  imported callees hold the *shared* context, because the runtime tracks the
  current invocation's cell in a thread-local scope (set on entry, restored on
  return so host-initiated re-entry nests). The scope carries the owning
  meter's address too, so a native cross-instance call never drains one
  instance's fuel into another's meter.

## Risks / Trade-offs

- [Fuel is an approximation; equal static sizes can differ in dynamic cost] ->
  accepted and documented; the interpreter remains the exact reference.
- [Budget trap point differs from the interpreter under a configured budget] ->
  explicit parity carve-out in `aot-execution`; unbudgeted behaviour is
  unchanged and remains parity-tested.
- [Concurrent budget check is racy; two threads may both pass] -> advisory
  ceiling, consistent with the interpreter's per-flush check; the counter itself
  is exact and monotonic. With invocation-local cells each thread checks the
  allowance it was granted at its last flush point, so the instance total can
  overshoot by one charge plus whatever the other threads have not yet flushed.
- [Two workers contend on one shared fuel cell] -> each invocation charges a
  stack-local cell and the runtime drains it once per host call and once per
  invocation, so independent CPU-bound invocations scale with cores instead of
  ping-ponging one cache line (pinned by a two-worker vs serial wall-time test).
- [Snapshot lag while an invocation is in flight] -> the counter is charged to
  the shared meter at flush points, so `stats()` lags by the invoking thread's
  unflushed fuel; the count stays monotonic and is exact once invocations end,
  matching the interpreter's per-flush granularity.
- [Indirect-callee charges bypass the local cell] -> documented: those frames
  run on the shared context (their `FuncDesc` owns it), so their fuel lands in
  the shared meter directly — correct and monotonic, just not cache-private.
- [A drain could fail an invocation retroactively] -> drains never trap; they
  only commit units into the authoritative counter and refresh the allowance.
  Enforcement stays at the inline charge site, so a finished invocation cannot
  be failed after the fact.
- [ABI v3 invalidates existing artifacts] -> loader refuses v2 with a clear
  error; the owning repo regenerates artifacts.
- [Emitted charge could miss a trap record] -> the charge uses `trapnz`, which
  the compiler already records; add a trap-mapping test.
- [Inline counter add wraps at u64::MAX] -> detected inline (unsigned compare
  against the previous value), the cell is clamped back with an atomic `umax`,
  and the budget check uses the clamped total, so a finite budget still traps
  and the observable counter stays monotonic; the runtime additionally reports
  saturation once per meter at error level. Needs ~2^64 units — unreachable,
  but detectable rather than silent.
- [Deterministic emission requirement] -> sizing and charge emission are purely
  static, so repeated compilation stays byte-identical.
- [Added prologue/loop work] -> one atomic add and compare per entry and loop
  iteration; the add targets an invocation-local line, so it does not serialize
  concurrent invocations.

## Migration Plan

1. Land the meter refactor, vmctx field, and `ABI_VERSION = 3` together so the
   compiler and runtime never disagree.
2. Regenerate `.aot` artifacts in the owning repo; the loader rejects v2
   artifacts until then.
3. Rollback: revert to ABI 2 and the previous meter; previously generated v2
   artifacts remain loadable.

## Open Questions

- Whether fuel should eventually weight by instruction class (e.g. charge memory
  operations more than arithmetic). Deferrable: the default is unweighted static
  counts, and changing the weighting changes no requirement, the approach, or
  the task breakdown.
