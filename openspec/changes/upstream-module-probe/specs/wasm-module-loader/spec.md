# Spec Delta

## ADDED Requirements

### Requirement: Module probe
The loader SHALL expose a fact-only `probe(bytes)` entry point reporting the
module's declared memories, imports, exports, retained custom sections, and
whether the code section contains the `memory.atomic.notify` opcode sequence.
The probe SHALL NOT apply embedder classification policy. It SHALL parse and
validate with the engine loader and SHALL return an error, without panicking,
for any malformed module.

#### Scenario: Facts reported for a valid module
- **WHEN** a valid module declares shared memory, an import, an export, and a custom section
- **THEN** the probe SHALL report each of those facts

#### Scenario: Atomic notify detected
- **WHEN** a module's code section contains `memory.atomic.notify`
- **THEN** the probe SHALL report atomic-notify presence

#### Scenario: Atomic notify absent
- **WHEN** a module's code section contains no `memory.atomic.notify`
- **THEN** the probe SHALL report its absence

#### Scenario: Malformed module rejected
- **WHEN** the module bytes are truncated or otherwise malformed
- **THEN** the probe SHALL return an error and SHALL NOT panic

### Requirement: Custom section retention
The parser SHALL retain custom sections on the parsed module in binary order,
each with its UTF-8 name and payload bytes. A custom section with an invalid
UTF-8 name SHALL be rejected.

#### Scenario: Custom section retained
- **WHEN** a module contains a custom section
- **THEN** the parsed module SHALL expose its name and payload unchanged
