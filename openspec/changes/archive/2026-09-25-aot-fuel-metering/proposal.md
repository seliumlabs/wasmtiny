# Proposal

## Why

The runtime's primary execution path is AOT, but CPU metering exists only on the
interpreter path. On the AOT path the instance meter's instruction count is
always zero and there is no execution ceiling, so embedders cannot enforce or
bill CPU usage where it actually runs.

## What Changes

- Instrument AOT-compiled code with **size-weighted fuel**: charge a function's
  static instruction count at function entry, and a loop body's static
  instruction count at each loop back-edge.
- Add an inline, lock-free **meter cell** reachable from compiled code through a
  new `vmctx` field. Compiled code charges it with an atomic add and traps when
  a configured budget is exceeded — no per-charge function call.
- Charge an **invocation-local** cell (not the shared one) on the per-invocation
  `vmctx` copy that `invoke_shared` already builds, draining it into the
  authoritative instance meter at each host-call boundary and at the end of the
  invocation. The emitted charge is unchanged; the local cell keeps concurrent
  invocations from ping-ponging one shared cache line, and each invocation's
  budget field holds the allowance remaining at its last flush point.
- **BREAKING**: bump the artifact ABI version (2 -> 3) for the new `vmctx`
  field. Existing `.aot` artifacts are rejected by the loader and must be
  regenerated.
- Add execution-budget set/reset to the AOT instance API; `stats()` returns real
  metering counts on the AOT path.
- Add a distinct budget-exhausted trap code carried through the artifact trap
  table, mapping to the existing `TrapCode::ExecutionBudgetExceeded`.
- Saturate the fuel counter at `u64::MAX`: the inline charge detects a wrapped
  add, clamps the cell back with an atomic `umax` (so the observable counter
  never decreases and a finite budget still traps), and the runtime reports the
  saturation once per meter at `error` level through the `log` facade.
- Reword the metering semantics so the counter is **metering units (fuel)**
  rather than exact instruction parity, and explicitly cover the AOT path;
  carve metering out of the AOT execution-parity requirement.

## Capabilities

### New Capabilities

<!-- None: this change extends existing capabilities. -->

### Modified Capabilities

- `instance-metering`: the instruction counter is redefined as size-weighted
  metering units and is charged on the AOT path as well as the interpreter; the
  execution budget is enforced on the AOT path with a distinct
  budget-exhausted trap, from an allowance refreshed at the invocation's flush
  points.
- `aot-execution`: a new requirement for fuel charging and budget-exhausted
  trapping, and a requirement that concurrent invocations charge without
  serializing on shared metering state; the native execution-parity requirement
  is scoped to exclude metering so a fuel trap is not a parity violation.

## Impact

- `crates/aotc`: `environment.rs` (`VmCtxOffsets::METER`, `USER_TRAP_BUDGET`,
  the inline charge emission with wrap clamp), `translate.rs` (static sizing
  pre-pass, entry/loop charge emission), `artifact.rs` (new trap byte,
  `ABI_VERSION = 3`). `compile.rs` needed no changes: charges are emitted during
  translation, and the artifact format is unchanged apart from the trap byte.
  The invocation-local cell requires no compiler change at all — only the
  contract documentation of what `vmctx.meter` may point at.
- `crates/core`: `runtime/metering.rs` (lock-free atomic meter, saturation
  clamp/report, invocation-local cell seeding and draining), `aot/context.rs`
  (`VmCtx` field and per-invocation contract), `aot/exec.rs` (wire the meter,
  budget API, real `stats()`, invocation-local cells on both entry points, the
  host-call boundary drain and its thread-local scope), `aot/format.rs` (trap
  byte), `aot/loader.rs`.
- Existing `.aot` artifacts must be regenerated; ABI version 2 artifacts are
  refused. One new external dependency: the `log` facade in `crates/core`,
  used solely for the (practically unreachable) counter-saturation report.
