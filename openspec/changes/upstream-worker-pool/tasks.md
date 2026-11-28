# Tasks

## 1. Worker pool

- [x] 1.1 Add `aot::{WorkerPool, WorkerPoolConfig, StopSignal, PoolReport,
  WorkerFault, StopOutcome}` with thread spawn, per-worker arguments, trap-site
  capture, monitor/join, retrying stop, and detach-on-wedge.
- [x] 1.2 Unit tests: clean exit, per-worker argument, trap, stop, wedged
  timeout.
- [x] 1.3 Add the `wasm-runtime-core` spec delta; verify
  `openspec validate --strict`.
- [x] 1.4 Verify no Selium ABI, process, or activity type enters the pool API
  (the fallback in the parent change did not trigger).
