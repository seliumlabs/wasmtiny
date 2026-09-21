## Purpose

Executes verified `.aot` artifacts natively in the runtime, providing loading, typed traps, host calls, and shared-memory and atomics behaviour with no compiler present in the process.

## ADDED Requirements

### Requirement: Fail-closed artifact verification
The runtime SHALL refuse to load any artifact that lacks a verifiable integrity section or whose integrity verification fails; there SHALL be no policy or build configuration that loads unverified artifacts.

#### Scenario: Tampered artifact refused
- **WHEN** any byte of a compiled artifact is modified after compilation
- **THEN** loading is refused with an integrity error

#### Scenario: Unsigned artifact refused
- **WHEN** an artifact without an integrity section is offered to the loader
- **THEN** loading is refused with an integrity error rather than executing

### Requirement: Malformed artifact rejection
The runtime SHALL reject malformed, truncated, or header-mismatched artifacts with structured errors, and SHALL NOT panic, hang, or crash the host while doing so.

#### Scenario: Truncated artifact errors
- **WHEN** an artifact is truncated mid-section
- **THEN** loading fails with an explicit error and the host process remains healthy

### Requirement: No compiler in the runtime
The runtime SHALL execute artifacts without linking the AOT compiler or any code generator; the runtime crate SHALL NOT declare the compiler's code-generation dependency (Cranelift).

#### Scenario: Default runtime runs without a code generator
- **WHEN** the runtime is built with default features
- **THEN** it loads and executes `.aot` artifacts with no code generator linked

### Requirement: Native execution parity
AOT execution SHALL produce the same observable behaviour as interpreter execution: identical results, memory/global/table side effects, imported-function calls, and traps for the same module and inputs.

#### Scenario: Arithmetic parity
- **WHEN** an exported arithmetic function is invoked via the AOT path
- **THEN** the result matches interpreter execution

#### Scenario: Branch table parity
- **WHEN** a module with a branch table of 10 entries executes with index 5 via the AOT path
- **THEN** execution branches to the 6th target

#### Scenario: State persists across invocations
- **WHEN** an artifact's exported function mutates memory or globals and is called again
- **THEN** the second call observes the first call's mutations

#### Scenario: Cross-module import aliasing preserved
- **WHEN** a compiled module imports a table, memory, global, or function exported by another module
- **THEN** mutations and calls behave identically to interpreter execution, sharing state by reference

### Requirement: Typed traps without host crashes
AOT execution SHALL detect out-of-bounds accesses, stack overflow, unreachable code, and other trap conditions and return typed `TrapCode` errors; it SHALL never crash or corrupt the host.

#### Scenario: Out-of-bounds read traps
- **WHEN** a compiled module reads memory beyond allocation
- **THEN** a `TrapCode::MemoryOutOfBounds` error is returned

#### Scenario: Stack overflow traps
- **WHEN** a compiled module recurses beyond the configured depth
- **THEN** a `TrapCode::StackOverflow` error is returned and the host stack is not exhausted

#### Scenario: Unreachable traps
- **WHEN** a compiled module executes `unreachable`
- **THEN** a `TrapCode::Unreachable` error is returned

#### Scenario: Read-only shared region write traps
- **WHEN** a compiled module writes to a shared region page mapped read-only
- **THEN** a `TrapCode::MemoryOutOfBounds` error is returned

### Requirement: Host function imports
Compiled code SHALL call imported host functions with correct argument passing and synchronous result or error delivery.

#### Scenario: Host function called from compiled code
- **WHEN** a compiled module calls an imported host function
- **THEN** the host function receives the correct arguments and its result is returned, or its error is propagated as a trap

### Requirement: Shared memory and atomics
The AOT path SHALL support atomic operations on shared memories, including `memory.atomic.notify` and `memory.atomic.wait`, routing to the platform wake primitives consistently with interpreter behaviour.

#### Scenario: Atomic read-modify-write
- **WHEN** a compiled module performs an atomic operation on a shared memory
- **THEN** the operation is applied atomically and observably consistent with interpreter execution

#### Scenario: Notify/wait wake
- **WHEN** one compiled module waits on a shared address and another notifies it
- **THEN** the waiter wakes and returns consistent results