# Proposal

## Why

wasmtiny serialises execution of a single instance: invocation is `&mut self` and the store, memory, and tables are wrapped in `Arc<Mutex<...>>`. The `wasm-runtime-core` "Thread safety" capability claims cross-thread use, but a given instance can only be executed by one host thread at a time — WebAssembly guest code has no way to use more than one host CPU core. Selium's `multithreaded-guest-execution` change requires the engine to permit N host threads to execute one instance's code concurrently over its shared linear memory, so a single guest can achieve true CPU parallelism. This change adds genuine concurrent (SMP) instance execution.

## What Changes

- Permit multiple concurrent invocations of a single instance, each carrying its own execution context (stack), sharing the instance's linear memory, tables, and globals coherently.
- Replace instance-wide serialisation — the `&mut self` invocation shape and `Arc<Mutex<Store>>`/`Arc<Mutex<Memory>>`/`Arc<Mutex<Table>>`-serialised data paths — with per-invocation contexts plus synchronised shared state.
- Ensure `memory.atomic.*` and `memory.atomic.wait`/`notify` remain observable across concurrent invocations, and that a thread parked in `memory.atomic.wait` does not serialise the instance or block other threads from executing it.
- Keep the interpreter's single-instance, lock-serialised semantics; concurrent execution is delivered on the AOT path, which already runs native code with per-thread stacks and atomic opcodes.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `wasm-runtime-core`: the "Thread safety" requirement is sharpened from "`Send`/`Sync` glue" to genuine SMP — one instance, many concurrent invocations with independent stacks, coherent shared memory/tables/globals, and observable atomics/wait/notify, free of instance-wide serialisation.

## Impact

- `crates/core/aot` (`exec.rs`, `traps.rs`, `application.rs`): concurrent invocation API (`&self` entry over a shared instance), per-invocation execution context; stack exhaustion on the concurrent path is recovered by the signal handler and reported as `StackOverflow`.
- `crates/core/memory` (`memory.rs`): synchronised memory growth against concurrent loads/stores, with the instance meter's memory-page budget check (see `instance-metering`) made atomic with the growth commit so concurrent grows cannot exceed a configured budget.
- `crates/core/aot` tables and globals: replace `Arc<Mutex<...>>` with concurrency-safe structures for `call_indirect` and mutable globals.
- `crates/core/runtime`: waiter map and wait/notify verified to park without holding the memory lock.
- Interpreter backend: execution stays single-instance (serialised semantics untouched), but its `memory.atomic.wait` park path is fixed to drop the memory and instance locks before parking, so a parked waiter never blocks a notifier or another invocation of the same instance (design decision 5).
