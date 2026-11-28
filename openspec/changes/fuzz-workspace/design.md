# Design

## Decisions

### Excluded workspace, not a feature

`fuzz/` is a standalone cargo-fuzz workspace (empty `[workspace]` plus a root
`exclude`). libFuzzer needs a nightly toolchain and a C++-era build
environment; neither may leak into `cargo test` or the default build. The
existing deterministic `wasmtiny-fuzz` binary remains the `cargo test` lane.

### Six targets, bounded budgets

- `module_load` — parse, validate, and the embedder probe.
- `load_and_instantiate` — the embedding API end to end.
- `interpreter_dispatch` — first function with zeroed typed arguments under an
  execution and memory budget.
- `aot_artifact_load` — artifact verification plus instantiation.
- `aot_dispatch` — compile, load, instantiate, and invoke a defined function
  under a budget, with a small input cap to bound compile time.
- `shared_region` — allocation, raw writes, waiter registration, notify, and
  destroy.

Every target treats hostile input as an error, never a panic, and bounds work
with byte caps and execution/memory budgets so a hang is a fuzz finding rather
than a stuck runner.

### Corpus from the downstream import

The imported corpus is minimised with `cmin` before commit; the working corpus
lives in the Actions cache and in run artifacts, never in the tree.

## Risks

- **AOT dispatch cost** — compilation is far slower than parsing, so the
  target caps input at 32 KiB and sets an execution budget.
- **Corpus size** — `cmin` keeps only coverage-adding inputs; the checked-in
  set stays reviewable and the nightly cache absorbs growth.
