# Tasks

## 1. Feasibility Spike

- [x] 1.1 Spike: run two host threads entering one AOT instance over shared linear memory and verify (by test result, ≥10 consecutive runs) where it breaks — memory, tables, globals, host-callback re-entrancy
- [x] 1.2 Confirm `memory.atomic.wait` parks a waiter without holding the memory or instance lock and verify a notifier on another thread wakes it (existing wasm-threads scenario)

## 2. Concurrent Invocation API

- [x] 2.1 Add a concurrent invocation entry on the shared instance taking `&self` (or equivalent interior-mutability), and verify two threads can invoke the same instance without an `&mut self` borrow conflict
- [x] 2.2 Give each invocation an independent execution context (stack) sharing one `vmctx`, and verify concurrent invocations preserve independent locals and control flow

## 3. Concurrent Shared State

- [x] 3.1 Make memory growth synchronised against concurrent loads/stores and verify a grow during concurrent reads does not tear or race
- [x] 3.2 Replace `Arc<Mutex<Table>>` with concurrency-safe table storage for `call_indirect` and verify concurrent `call_indirect` is correct
- [x] 3.3 Replace `Arc<Mutex<Global>>` for mutable globals with concurrency-safe storage and verify concurrent global access is correct
- [x] 3.4 Fold the instance meter's memory-page budget check (see `instance-metering`) into the growth critical section — the current grow path is a check-then-act across a dropped lock — and verify concurrent grows cannot push an instance past its configured memory budget

## 4. Correctness and Integration

- [x] 4.1 Add a concurrency stress test (two threads, shared instance, atomics + wait/notify) and verify it passes repeatedly (≥10 consecutive runs)
- [x] 4.2 Run the full `cargo test` (spec corpus, malformed corpus, spine regressions) and verify it is green
- [x] 4.3 Verify against `openspec validate` that the change is valid and all artifacts are complete
