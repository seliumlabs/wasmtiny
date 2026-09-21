# wasm-test-suites

## Purpose

TBD

## Requirements

### Requirement: Self-contained test execution
The repository's test suites SHALL run via `cargo test` on a fresh clone with no C toolchain, no external binaries, no git submodules, and no network access.

#### Scenario: Fresh clone test run
- **WHEN** a developer clones the repository and runs `cargo test` without initialising submodules or installing tools beyond a Rust toolchain
- **THEN** all test suites SHALL compile and pass

#### Scenario: No C-era test assets
- **WHEN** the repository tree is inspected
- **THEN** no test suite SHALL require `iwasm`, `wamrc`, wasi-sdk, googletest, CMake, or libFuzzer to build or run

### Requirement: Vendored core spec suite
The WebAssembly core spec test corpus (`*.wast`) SHALL be vendored into the repository and exercised by a Rust directive runner supporting module/register/invoke/assert_return/assert_trap/assert_invalid/assert_malformed/assert_unlinkable directives.

#### Scenario: Spec suite runs from vendored files
- **WHEN** `cargo test --test spec` executes
- **THEN** it SHALL load `.wast` files from an in-repo directory and evaluate every applicable directive, failing on any unmet assertion

#### Scenario: No submodule dependency
- **WHEN** the repository is cloned with `--recurse-submodules` never invoked
- **THEN** the spec suite SHALL still pass because the corpus is committed to the repo

### Requirement: Malformed module corpus
The runtime SHALL be regression-tested against a corpus of malformed/crasher modules, each of which MUST fail to load with an error (never panic, hang, or crash the host).

#### Scenario: Malformed corpus rejected cleanly
- **WHEN** `cargo test --test malformed` executes over every `.wasm` file in the malformed corpus directory
- **THEN** each file SHALL fail module loading with an `Err` and the test process SHALL remain alive

### Requirement: Targeted regression tests
Fixed bugs SHALL be covered by targeted Rust regression tests (inline WAT or committed fixtures) named after the defect they cover, rather than by parked placeholders.

#### Scenario: No parked tests
- **WHEN** the test suites execute
- **THEN** there SHALL be no test that prints a "parked"/"skipped-pending-feature" message and passes without asserting behaviour

#### Scenario: Regression test per critical fix
- **WHEN** a change fixes a correctness bug classed as critical or major
- **THEN** the change SHALL add a failing-before/passing-after regression test referencing the defect

### Requirement: AOT-path spec corpus
The vendored WebAssembly spec corpus SHALL be exercised through the AOT execution path — compiling each module with the AOT compiler library and asserting the same directives — in addition to the existing interpreter-path execution.

#### Scenario: Corpus directives pass in AOT mode
- **WHEN** `cargo test` runs the `.wast` corpus through the AOT path
- **THEN** every applicable directive (module/register/invoke/assert_*) is evaluated with the same pass/fail outcomes that wasm semantics require

#### Scenario: AOT tests run with no external tools
- **WHEN** the AOT corpus tests execute on a fresh clone
- **THEN** they run via `cargo test` with no C toolchain, no external binaries, no git submodules, and no network access

### Requirement: Artifact rejection testing
The test suites SHALL verify that the runtime refuses artifacts that are tampered, unsigned, version-mismatched, or ISA-mismatched, and that rejection never panics, hangs, or crashes the host.

#### Scenario: Tampered artifact rejected
- **WHEN** a compiled artifact is modified in any byte and offered to the loader
- **THEN** loading fails with an integrity error and the test process remains alive

#### Scenario: Unsigned artifact rejected
- **WHEN** an artifact without an integrity section is offered to the loader
- **THEN** loading fails with an integrity error

#### Scenario: Version-mismatched artifact rejected
- **WHEN** an artifact with an unsupported format or ABI version is offered to the loader
- **THEN** loading fails with an explicit version error
