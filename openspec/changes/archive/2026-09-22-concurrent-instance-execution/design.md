# Design

## Context

See proposal.md — Why. Today `WasmApplication::call_function` and the instance `invoke` take `&mut self`; the store, memories, tables, and mutable globals are `Arc<Mutex<...>>` (AOT `exec.rs`, interpreter store). Guest memory is mmap-backed with pre-reserved address space, and the AOT path executes guest native loads/stores directly against those pages; per-thread native stacks and stack limits already exist (`traps.rs`). Atomics, `memory.atomic.wait`/`notify`, and the waiter map already exist (`shared_memory.rs`, `memory.rs`) and route to platform primitives.

## Goals / Non-Goals

**Goals:**

- SMP execution: N host threads executing one instance's code concurrently over shared linear memory, each with its own stack.
- Coherent shared memory, tables, and globals; atomics/wait/notify observable across concurrent invocations; no instance-wide serialisation.
- Keep the interpreter's serialised semantics untouched.

**Non-Goals:**

- The WebAssembly threads *proposal* (thread spawning from guest code, `shared` type validation, guest TLS) — concurrency is host-initiated.
- Changing the interpreter or the module/proposal validation surface.
- Selium's worker pool itself (that is the arch3 change).

## Decisions

**1. AOT-only concurrency.**
The interpreter keeps `Arc<Mutex<Store>>` serialisation and is the spec-corpus/fallback path; the AOT backend becomes concurrent. Rationale: the AOT already runs native code with per-thread stacks and direct memory access, so it is the plausible substrate; the interpreter's value stack is inherently one-context.

**2. Per-invocation execution context.**
Each concurrent invocation holds an independent logical stack and a shared `vmctx` referencing the instance's shared Memory/Table/Global handles. Trap recovery stacks are already per host thread (`traps.rs`), so the context plumbing extends rather than replaces them. The shared context's stack limit is disabled (indirect callees read it, and one thread's bound is not valid for another), so indirect-callee stack overflow on the concurrent path is recovered by the thread's guard page + signal alt-stack and classified `StackOverflow` by the handler (faulting SP at or below the thread's recorded stack base).

**3. Concurrent shared state.**
Memory stays direct-access mmap and needs synchronisation only on `grow` (a rare, coordinated mutation). Tables (`call_indirect`) and mutable globals move from `Arc<Mutex<...>>` to concurrency-safe storage (lock-free or fine-grained locks) because they are read on the hot path of every indirect call and global access. The instance meter's memory-budget check (added by `instance-metering`) moves inside the growth's critical section: today the grow path checks the page budget under the instance lock and then grows under the memory lock — a check-then-act that concurrent invocations could exploit to exceed the budget — so the check and the commit must become one atomic step. The budget counts committed pages across the whole instance (matching the interpreter's grow path and `stats`), so the growth critical section is the instance's full memory-lock set, taken in ascending index order.

**4. Invocation API rejects `&mut self`.**
Concurrent entry becomes `&self`-style over a shared instance (wasmtime's `Store` model), so the owning `Arc<Instance>` can be invoked from any thread.

**5. Waiter park without lock.**
Verify and preserve the existing `wasm-threads` contract — a thread parked in `memory.atomic.wait` must not hold the memory/instance lock, so others can notify it.

**6. Pre-build spike.**
Before the API/state refactors, spike with two host threads entering one instance over shared memory to isolate what actually breaks (table, global, growth, host-callback re-entrancy).

## Risks / Trade-offs

- **[Table/global lock granularity becomes the hot path]** → lock-free where feasible, fine-grained otherwise; measured in the concurrency stress tests.
- **[Memory grow racing concurrent loads]** → grow is already grant/commit based; make it synchronise against readers (e.g. an rwlock or epoch), a bounded rarity.
- **[Memory-budget check-then-act]** the `instance-metering` grow path checks the page budget under the instance lock and grows under the memory lock; under concurrent invocations this TOCTOU lets concurrent grows exceed the budget → fold the budget check into the grow critical section (decision 3).
- **[Host function re-entrancy]** → the existing "Callback-safe lock discipline" requirement already forbids holding store/memory locks across host callbacks; preserve it.
- **[Interpreter divergence]** → AOT-only concurrency means behaviour differences between backends under concurrency are by design, not bugs; spec-corpus still runs serialised.

## Migration Plan

1. Land this change ahead of the arch3 `multithreaded-guest-execution` change (its task 1.1 depends on it).
2. Introduce the concurrent invocation API alongside the existing `&mut self` API; deprecate the old shape after arch3 migrates.
3. No rollback of artifacts needed; concurrency is opt-in at the API level (old single-invocation API keeps working).

## Open Questions

- Whether tables/globals go lock-free (CAS on resize) versus fine-grained reads — typically resolves empirically from the spike.
- Whether memory growth needs an epoch or a plain rwlock — deferred to the spike's growth-vs-load findings.
