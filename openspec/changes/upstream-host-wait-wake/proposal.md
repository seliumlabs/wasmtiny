# Proposal: Host wait/wake primitives

## Why

The engine emits the host platform wake primitive on a guest
`memory.atomic.notify` against a shared region, but embedders that park on the
region's host mapping have no matching public wait primitive. Selium therefore
carries its own `futex`/`WaitOnAddress`/`_umtx_op` wait backend — a second
platform syscall surface that must stay in lockstep with the engine's emission.

## What Changes

- Expose `os_wake::{SUPPORTED, wait, wake}` alongside the existing emission.
- `wait` returns `WaitOutcome::{Woken, TimedOut, Recheck}`; `Duration::MAX`
  blocks indefinitely.
- `emit_wake` delegates to `wake`, so emission and embedder wakes use one
  backend. Correct the FreeBSD wake opcode to `UMTX_OP_WAKE_PRIVATE` (16).
- Add a paired cross-thread conformance test on an `mmap`'d shared page,
  skipped where `SUPPORTED` is false.

## Impact

- `wasmtiny` core: `runtime::os_wake` gains a wait backend and a public
  wake entry point. No ABI or artifact change.
