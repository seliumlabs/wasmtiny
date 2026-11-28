# Spec Delta

## MODIFIED Requirements

### Requirement: Self-contained test execution
The repository's test suites SHALL run via `cargo test` on a fresh clone with no C toolchain, no external binaries, no git submodules, and no network access. Coverage-guided fuzzing SHALL live in a separate workspace excluded from the main workspace, and SHALL NOT be required by `cargo test` or `cargo build`; an optional nightly lane may use libFuzzer.

#### Scenario: Fresh clone test run
- **WHEN** a developer clones the repository and runs `cargo test` without initialising submodules or installing tools beyond a Rust toolchain
- **THEN** all test suites SHALL compile and pass

#### Scenario: No C-era test assets
- **WHEN** the repository tree is inspected
- **THEN** no `cargo test` suite SHALL require `iwasm`, `wamrc`, wasi-sdk, googletest, CMake, or libFuzzer to build or run

#### Scenario: Fuzz workspace stays out of the default build
- **WHEN** `cargo build` or `cargo test` runs at the repository root
- **THEN** the `fuzz/` workspace SHALL NOT be compiled and libFuzzer SHALL NOT be required

## ADDED Requirements

### Requirement: Coverage-guided fuzz workspace
The repository SHALL provide an excluded `fuzz/` cargo-fuzz workspace with targets for module load, load-and-instantiate, interpreter dispatch, AOT artifact load, AOT dispatch, and the shared-region API. The checked-in seed corpus SHALL be minimised, and the working corpus SHALL NOT be committed.

#### Scenario: Every target is listed
- **WHEN** `cargo +nightly fuzz list` runs in the repository
- **THEN** it SHALL list every target named above

#### Scenario: Hostile input never panics
- **WHEN** a target receives arbitrary bytes
- **THEN** it SHALL return an error or complete without panicking, hanging, or aborting the process

#### Scenario: Seed replay
- **WHEN** the smoke lane replays the checked-in corpus
- **THEN** every target SHALL complete cleanly

#### Scenario: Nightly lane
- **WHEN** the nightly workflow is dispatched with a budget
- **THEN** each target SHALL run for that budget and upload crash and corpus artifacts
