## Purpose

Defines the `.aot` artifact binary format used by the compiler and the runtime: versioned, self-describing, finish-linked sections, with a mandatory scheme-tagged integrity section and room for future signature schemes.

## ADDED Requirements

### Requirement: Versioned and target-bound header
Every `.aot` artifact SHALL begin with a header carrying the format version, ABI version, target ISA, and endianness. A loader SHALL reject any artifact whose header it cannot satisfy.

#### Scenario: Mismatched ABI version rejected
- **WHEN** an artifact declares an ABI version newer than the loader supports
- **THEN** loading is refused with an explicit version error

#### Scenario: Wrong target ISA rejected
- **WHEN** an artifact declares a target ISA different from the host
- **THEN** loading is refused with an explicit target error

#### Scenario: Wrong endianness rejected
- **WHEN** an artifact declares endianness opposite to the host
- **THEN** loading is refused with an explicit format error

### Requirement: Self-describing module metadata
An artifact SHALL carry the type, import, export, memory, table, and global metadata, including initialisation values, plus function mapping and trap tables, sufficient to instantiate and execute the module without parsing WebAssembly.

#### Scenario: Instantiation does not parse WebAssembly
- **WHEN** an artifact is loaded by the runtime
- **THEN** memories, tables, globals, and initialisation segments are applied and exports are resolvable without parsing the original WebAssembly

### Requirement: Finish-linked code
Artifact code SHALL be fully linked by the compiler; the loader SHALL NOT perform relocation or mutation of code after verification.

#### Scenario: Loader is read-only after verification
- **WHEN** a verified artifact is loaded
- **THEN** no relocation step occurs and code bytes are mapped executable without modification

### Requirement: Mandatory scheme-tagged integrity section
Every artifact SHALL contain an integrity section that carries a scheme identifier and a payload covering all bytes preceding the section.

#### Scenario: Scheme-tagged payload
- **WHEN** an artifact's integrity section is inspected
- **THEN** it declares a scheme identifier and a payload covering all preceding bytes

#### Scenario: Unknown scheme refused
- **WHEN** an artifact declares an integrity scheme the loader does not implement
- **THEN** loading is refused with an explicit unsupported-scheme error

### Requirement: Extensible signature schemes
The format SHALL reserve scheme identifiers and a key identifier field so that future signature-based schemes can be added without a format version change.

#### Scenario: Format unchanged for new scheme
- **WHEN** a new signature scheme is introduced later
- **THEN** it SHALL use a reserved scheme identifier and the same section shape, requiring no format version bump