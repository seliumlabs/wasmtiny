# Proposal: Coverage-guided fuzz workspace

## Why

Coverage-guided fuzzing of wasm parsing, instantiation, interpretation, AOT
artifact loading, AOT dispatch, and the shared-region API belongs in the
repository that owns those surfaces. Keeping the harness downstream means the
reproducer, fix, regression test, and seed all live in the wrong place.

## What Changes

- Add an excluded `fuzz/` cargo-fuzz workspace with six targets: module load,
  load-and-instantiate, interpreter dispatch, AOT artifact load, AOT dispatch,
  and shared-region API.
- Import the engine seed corpus (previously carried downstream) and commit a
  `cmin`-reduced copy.
- Add `scripts/fuzz-smoke.sh` and a nightly `fuzz-nightly.yml` workflow with a
  manual dispatch input.
- Amend `wasm-test-suites` so `cargo test` stays libFuzzer-free while the
  optional nightly lane may use libFuzzer.

The deterministic in-repo security fuzz lane is unchanged.

## Impact

- New `fuzz/` workspace, excluded from the main workspace.
- New workflow and script. `cargo test` and `cargo build` are unaffected.
