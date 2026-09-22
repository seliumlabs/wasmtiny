## Purpose

The interpreter execution mode for WebAssembly bytecode, using a stack-based virtual machine with operand and control stacks and deterministic, trap-safe semantics.

## Requirements

### Requirement: Classic interpreter execution
The interpreter SHALL execute WebAssembly bytecode using a stack-based virtual machine with operand and control stacks. It SHALL be available in every build alongside the AOT execution path (no longer the only execution mode). It SHALL charge each executed instruction against the executing instance's instruction meter (see `instance-metering`), and SHALL NOT contain safepoint or suspension hooks. It SHALL NOT dispatch host calls through a pending/outcome protocol — host functions return results or errors synchronously.

#### Scenario: Bytecode executes via operand and control stacks
- **WHEN** a function is invoked through the interpreter
- **THEN** its bytecode executes sequentially via the operand and control stacks and its results are returned

#### Scenario: Default build includes the interpreter
- **WHEN** the crate is built with default features
- **THEN** the interpreter is present and `.wasm` input loads and executes through it alongside the AOT path

#### Scenario: Wasm modules execute via the interpreter
- **WHEN** a `.wasm` module is loaded and its function invoked through the interpreter
- **THEN** it executes and host calls complete synchronously

#### Scenario: Instructions charged to the instance meter
- **WHEN** an instance executes a function through the interpreter
- **THEN** each executed instruction SHALL be charged to that instance's instruction meter

### Requirement: Instruction coverage
The interpreter SHALL implement all WebAssembly MVP instructions including control flow, memory, numeric, and parametric operations.

#### Scenario: Control flow instructions execute
- **WHEN** a module uses `block`, `loop`, `if`, `br`, `br_table`, and `return` instructions
- **THEN** control flow follows the specified semantics

#### Scenario: Numeric and parametric instructions execute
- **WHEN** a module uses numeric (I32, I64, F32, F64 arithmetic and conversion) and parametric (`drop`, `select`) instructions
- **THEN** results match the WebAssembly specification

### Requirement: Host function imports
The interpreter SHALL support calling imported host functions with proper parameter passing.

#### Scenario: Imported host function receives arguments
- **WHEN** a module calls an imported host function with arguments
- **THEN** the host function is invoked with the arguments in order and its return value is delivered back to the guest

### Requirement: Branch table support
The interpreter SHALL efficiently handle `br_table` instructions with arbitrary branch table sizes.

#### Scenario: Large branch table with default target
- **WHEN** a module executes `br_table` with many targets and an index beyond the target range
- **THEN** in-range indices select their target and out-of-range indices take the default target

### Requirement: Cross-module funcref dispatch
The interpreter SHALL execute `call_indirect` through funcrefs stored in imported or shared tables, including functions defined in other modules.

#### Scenario: Funcref from another module dispatched indirectly
- **WHEN** a table entry holds a funcref defined by a different module and is invoked via `call_indirect`
- **THEN** the other module's function executes with the expected type checks and result

### Requirement: Stack overflow detection
The interpreter SHALL detect and trap on operand stack overflow. The interpreter's stack/call-depth limits SHALL be consistent with the validator's static guarantees, so that no module passing validation fails at runtime for exceeding a limit the validator did not check.

#### Scenario: Validator and interpreter limits agree
- **WHEN** a module's maximum operand-stack depth exceeds the interpreter's operand stack capacity
- **THEN** the module SHALL be rejected at validation time with a clear error rather than failing mid-execution

### Requirement: Deterministic execution
The interpreter SHALL produce identical results for the same module input regardless of execution order.

#### Scenario: Execute add instruction
- **WHEN** a module containing `(func (result i32) (i32.add (i32.const 1) (i32.const 2)))` is executed
- **THEN** the result is 3

#### Scenario: Execute memory load
- **WHEN** a module loads an i32 from memory offset 0
- **THEN** the correct value is returned from the instance memory

#### Scenario: Execute br_table
- **WHEN** a module with a branch table of 10 entries executes with index 5
- **THEN** execution branches to the 6th target

#### Scenario: Stack overflow
- **WHEN** a module executes instructions that overflow the operand stack
- **THEN** a trap with `TrapCode::StackOverflow` is returned

#### Scenario: Host function call
- **WHEN** a module calls an imported host function
- **THEN** the host function is invoked with correct arguments and result is returned

#### Scenario: Host function call completes synchronously
- **WHEN** a guest calls an imported host function
- **THEN** the host function's `call` method runs to completion and its results or error are delivered directly to the interpreter

#### Scenario: Indirect call through shared imported table
- **WHEN** a module calls `call_indirect` through a table entry populated by another module
- **THEN** the referenced function is invoked with the expected type checks and result

### Requirement: Untrusted operand hardening
The interpreter SHALL NOT size heap allocations directly from guest-controlled counts or lengths, and SHALL bounds-check memory regions before copying or filling them.

#### Scenario: memory.copy bounds-checked before copying
- **WHEN** a guest executes `memory.copy` with a length that exceeds source or destination bounds
- **THEN** execution SHALL trap with `MemoryOutOfBounds` without allocating a length-sized staging buffer

#### Scenario: memory.fill bounds-checked before filling
- **WHEN** a guest executes `memory.fill` with a length that exceeds destination bounds
- **THEN** execution SHALL trap with `MemoryOutOfBounds` without allocating a length-sized staging buffer

#### Scenario: br_table with huge count does not exhaust memory
- **WHEN** a module executes a `br_table` whose declared label count is near u32::MAX
- **THEN** the interpreter SHALL process the instruction without pre-allocating count-sized memory (validation already bounded the count)

### Requirement: Robust immediate decoding
The interpreter SHALL reject LEB128 immediates whose final byte carries bits beyond the decoded type's width, and SHALL reject unmapped atomic subopcodes with an error rather than ignoring them.

#### Scenario: Overlong LEB immediate rejected
- **WHEN** a function body contains a u32 immediate encoded with set bits beyond bit 31
- **THEN** loading or execution SHALL fail with an explicit error

#### Scenario: Unknown atomic subopcode errors
- **WHEN** execution encounters an unmapped 0xFE subopcode
- **THEN** execution SHALL fail with an explicit unsupported-instruction error rather than a no-op