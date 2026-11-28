# Proposal: Generic AOT worker pool

## Why

Embedders that run one shared AOT instance on many OS threads reimplement the
same generic mechanics: spawning workers with a per-worker argument, capturing
the trap site, joining through a monitor, retrying stop delivery, and
detaching a wedged pool instead of hanging teardown. Those mechanics are engine
knowledge; only the stop signal, worker count, fault presentation, and
completion policy belong to the embedder.

## What Changes

- Add `aot::{WorkerPool, WorkerPoolConfig, StopSignal, PoolReport,
  WorkerFault, StopOutcome}`.
- The pool spawns N workers entering one export with a per-worker argument,
  captures the first trap site, runs a monitor that joins and reports once,
  retries an embedder-supplied stop signal, and detaches a wedged pool after a
  configurable budget.
- No engine API carries embedder ABI, process, or activity types.

## Impact

- `wasmtiny` core: new `aot::pool` module. No ABI, artifact, or execution
  change.
