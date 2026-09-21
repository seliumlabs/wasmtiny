## Purpose

Compiles WebAssembly modules to native `.aot` artifacts ahead of time, so the runtime executes verified native code with no compiler linked into it.

## Requirements

### Requirement: Compilation of valid modules
The AOT compiler SHALL accept a valid `.wasm` binary and emit a loadable `.aot` artifact whose execution produces the same observable behaviour as interpreter execution of the same module.

#### Scenario: Simple function compiles and executes
- **WHEN** a module containing `(func (result i32) (i32.add (i32.const 1) (i32.const 2)))` is compiled and its exported function invoked through the AOT path
- **THEN** the result is 3

### Requirement: Supported feature coverage
The compiler SHALL support the core specification plus bulk memory, reference types, atomics, threads, and shared-memory instructions, matching the coverage of the interpreter execution mode.

#### Scenario: Parity corpus compiles
- **WHEN** the vendored spec corpus modules within the supported feature set are compiled with the AOT compiler
- **THEN** compilation SHALL succeed for every such module

### Requirement: Explicit rejection of unsupported features
The compiler SHALL reject modules containing instructions or types outside the supported feature set with an explicit unsupported-feature error, and SHALL NOT emit a partial artifact or an artifact with fallback semantics.

#### Scenario: SIMD module rejected
- **WHEN** a module uses SIMD (`v128`) instructions
- **THEN** compilation fails with an explicit unsupported-feature error and no artifact is produced

#### Scenario: Unsupported proposal never executes
- **WHEN** a module requires a proposal outside the supported set
- **THEN** compilation SHALL fail and the module SHALL NOT be executable in any form

### Requirement: Deterministic artifact emission
Compiling the same `.wasm` input with the same configuration SHALL produce byte-identical `.aot` output on every run.

#### Scenario: Repeated compilation is byte-identical
- **WHEN** the same module is compiled twice with identical configuration
- **THEN** both artifacts are byte-for-byte identical

### Requirement: Integrity section always emitted
The compiler SHALL emit an integrity section in every artifact it produces, covering all bytes of the artifact that precede the section.

#### Scenario: Compiled artifact carries verification data
- **WHEN** any artifact produced by the compiler is inspected
- **THEN** it contains an integrity section whose digest matches the preceding bytes
