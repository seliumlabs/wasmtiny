# Tasks

## 1. Module probe

- [x] 1.1 Retain custom sections on `Module` (`CustomSection { name, payload }`)
  and capture atomic-notify presence from the code section during parsing;
  verify with unit tests over a WAT module with shared memory, imports,
  exports, and a custom section.
- [x] 1.2 Add `loader::probe::{probe, ModuleProbe}` and re-export
  `wasmtiny::probe`; verify WAT fixtures with and without
  `memory.atomic.notify`.
- [x] 1.3 Make the probe total on malformed input; verify every committed
  malformed module returns `Err` without panic.
- [x] 1.4 Add the `wasm-module-loader` spec delta; verify
  `openspec validate --strict` passes.
