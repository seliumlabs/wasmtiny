# Tasks

## 1. Fuzz workspace

- [x] 1.1 Add the excluded `fuzz/` workspace with all six targets; verify
  `cargo +nightly fuzz list` and that the default workspace build is unaffected.
- [x] 1.2 Import the engine `module_probing` seeds, minimise with `cmin`, and
  commit the reduced corpus; verify a short smoke run.
- [x] 1.3 Add `scripts/fuzz-smoke.sh` and the nightly `fuzz-nightly.yml`
  workflow with a `workflow_dispatch` budget input.
- [x] 1.4 Amend `wasm-test-suites`; verify `openspec validate --strict` and
  `cargo test`.
