# Proposal

## Why

Instance metering and resource limits were removed from Wasmtiny in the cull-unused-runtime-features change, and the interpreter spec now explicitly forbids metering hooks. Wasmtiny's known embedder, Selium, needs per-instance instruction counting and memory reporting to bill tenant CPU and memory honestly, and needs a way to halt an instance that exhausts its budget. Restore engine-side metering without regressing the engine's fenced, boring scope.

## What Changes

- Add a per-instance, monotonic counter of executed WebAssembly instructions, charged on the interpreter execution path (the path the embedder executes).
- Add a per-instance committed linear-memory page gauge (owned pages, excluding shared-region pages).
- Add a settable/resettable per-instance execution budget that surfaces a distinct budget-exhausted outcome (a trap) rather than silently overflowing.
- Add a settable per-instance memory-page budget, enforced at growth.
- Reverse the interpreter's "no per-instruction metering hooks" clause.
- Keep safepoints, suspension, and snapshotting/migration out of scope, and leave the AOT execution path unchanged for now.

## Capabilities

### New Capabilities

- `instance-metering`: per-instance instruction accounting, memory usage reporting, and configurable execution and memory budgets with distinct exhaustion signals.

### Modified Capabilities

- `wasm-interpreter`: classic interpreter execution now charges each executed instruction against the instance's meter.

## Impact

- **Code**: `src/runtime/metering.rs` (new `InstanceMeter`), `src/interpreter` (instruction charging), `src/runtime/instance.rs` (stats and budget API, grow-path budget enforcement), `src/engine/runtime.rs` (embedder-facing stats/budget API, host-side grow enforcement), `src/lib.rs` (re-exports). `src/runtime/error.rs` and `src/memory.rs` are unchanged: the budget-exhausted trap codes already exist, and enforcement sits in the grow paths before `Memory::grow`'s `mprotect`.
- **Embedder**: Selium reads instance stats and sets budgets through the public API.
- **AOT path**: unchanged; instrumenting `.aot` native execution is a documented follow-up.
