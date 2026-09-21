## ADDED Requirements

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