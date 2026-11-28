# Tasks

## 1. Host wait/wake

- [x] 1.1 Expose `os_wake::{SUPPORTED, wait, wake}` and `WaitOutcome`; keep
  `COMPILED_IN`/`active` as the conformance-gated emission signal.
- [x] 1.2 Delegate `emit_wake` to `wake` and correct the FreeBSD wake opcode.
- [x] 1.3 Add unit tests for timeout, value mismatch, and wake paths.
- [x] 1.4 Add a paired cross-thread park/wake conformance test on an
  `mmap`'d shared page, skipped where `SUPPORTED` is false.
- [x] 1.5 Add the `shared-region-mapping` spec delta; verify
  `openspec validate --strict`.
