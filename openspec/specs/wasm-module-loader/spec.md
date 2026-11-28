## Requirements

### Requirement: Binary format parsing
The loader SHALL parse WebAssembly binary format (.wasm) into an intermediate representation.

### Requirement: Validation
The loader SHALL validate WASM modules according to the WebAssembly specification, rejecting invalid modules. This SHALL include rejecting an `if` without `else` whose parameter and result arities differ, rejecting non-zero memory indices on bulk-memory instructions (multi-memory is unsupported), validating memarg alignment exponents, and enforcing the `ref.func` declared-function rule.

#### Scenario: if without else and mismatched arities rejected
- **WHEN** a function contains an `if` block whose parameter count differs from its result count and which has no `else` arm
- **THEN** validation SHALL fail

#### Scenario: Bulk op with non-zero memory index rejected
- **WHEN** a function body contains `memory.copy`, `memory.fill`, or `memory.init` with a non-zero memory index immediate
- **THEN** validation SHALL fail

#### Scenario: Excessive memarg alignment rejected
- **WHEN** a memory instruction's alignment immediate encodes an alignment greater than its natural access size
- **THEN** validation SHALL fail

### Requirement: Type checking
The loader SHALL verify that function signatures, local types, and global types are consistent throughout the module.

### Requirement: Section ordering
The loader SHALL enforce proper section ordering per the WASM binary specification for the sections it supports. Tag sections (exception handling) are not supported and SHALL be rejected.

#### Scenario: Unknown or unsupported section rejected
- **WHEN** a module contains a tag section or an unknown section id
- **THEN** parsing SHALL fail with an explicit unsupported-section error

### Requirement: Reference type encoding support
The loader SHALL parse and validate the basic reference-type encodings needed for funcref tables (`funcref`, `externref`, `ref.null`, `ref.func`, non-null table initialisers). Exception-handling tag encodings and GC heap types are not supported and SHALL be rejected explicitly.

#### Scenario: Non-null table initializer rejects ref.null
- **WHEN** a module declares a non-null table type but uses `ref.null` as the declared initializer
- **THEN** parsing or validation SHALL fail instead of accepting the initializer

#### Scenario: Tag imports and exports are rejected
- **WHEN** a module declares tag imports, tag exports, or a tag section
- **THEN** parsing or validation SHALL fail with an explicit unsupported-feature error

#### Scenario: GC heap types are rejected
- **WHEN** a module references GC heap types (e.g. `any`, `eq`, `struct`, `array` encodings)
- **THEN** parsing or validation SHALL fail with an explicit unsupported-feature error

#### Scenario: Valid WASM module loading
- **WHEN** a valid WASM binary is loaded
- **THEN** a `Module` is returned with all sections parsed correctly

#### Scenario: Invalid magic number
- **WHEN** a file with invalid WASM magic bytes is loaded
- **THEN** `Err(WasmError::Load("invalid magic number"))` is returned

#### Scenario: Type mismatch in function body
- **WHEN** a function body references local variables of wrong type
- **THEN** `Err(WasmError::Validation("type mismatch"))` is returned

#### Scenario: Missing required section
- **WHEN** a module is missing the Type section
- **THEN** `Err(WasmError::Validation("type section required"))` is returned

### Requirement: DataCount section support
The loader SHALL accept the DataCount section (id 12, ordered between the element and code sections) and use it to validate data segment indices referenced by `memory.init`/`data.drop`.

#### Scenario: Bulk-memory binary with DataCount loads
- **WHEN** a module produced by a standard toolchain (LLVM/Binaryen) containing `memory.init` or `data.drop` and a DataCount section is loaded
- **THEN** parsing and validation SHALL succeed

#### Scenario: Data segment index checked against DataCount
- **WHEN** a function body references a data segment index ≥ the DataCount value
- **THEN** validation SHALL fail

### Requirement: Untrusted declaration limits
The loader SHALL NOT size heap allocations directly from untrusted section counts or declared minimums, and SHALL cap function-local counts and table minimums at defined maxima.

#### Scenario: Huge type-section count does not pre-allocate
- **WHEN** a malformed module declares a section vector count near u32::MAX
- **THEN** parsing SHALL fail with an unexpected-end or count error without attempting a count-sized allocation

#### Scenario: Huge locals count rejected
- **WHEN** a function body declares locals whose expanded total exceeds the implementation cap
- **THEN** validation SHALL fail with an explicit limits error

#### Scenario: Huge table minimum rejected
- **WHEN** a table declaration's minimum exceeds the implementation cap
- **THEN** loading or instantiation SHALL fail with an explicit limits error rather than allocating minimum-sized storage

### Requirement: Name encoding strictness
The loader SHALL require names (module, function, import/export fields) to be valid UTF-8 and reject malformed encodings.

#### Scenario: Invalid UTF-8 name rejected
- **WHEN** a module contains an import/export name that is not valid UTF-8
- **THEN** parsing SHALL fail with an explicit encoding error
