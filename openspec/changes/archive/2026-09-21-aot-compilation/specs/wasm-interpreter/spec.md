## MODIFIED Requirements

### Requirement: Classic interpreter execution
The interpreter SHALL execute WebAssembly bytecode using a stack-based virtual machine with operand and control stacks. It SHALL be available in every build alongside the AOT execution path (no longer the only execution mode). It SHALL NOT contain safepoint, suspension, or per-instruction metering hooks (those subsystems are removed), and it SHALL NOT dispatch host calls through a pending/outcome protocol — host functions return results or errors synchronously.

#### Scenario: Bytecode executes via operand and control stacks
- **WHEN** a function is invoked through the interpreter
- **THEN** its bytecode executes sequentially via the operand and control stacks and its results are returned

#### Scenario: Default build includes the interpreter
- **WHEN** the crate is built with default features
- **THEN** the interpreter is present and `.wasm` input loads and executes through it alongside the AOT path

#### Scenario: Wasm modules execute via the interpreter
- **WHEN** a `.wasm` module is loaded and its function invoked through the interpreter
- **THEN** it executes and host calls complete synchronously