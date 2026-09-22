# Spec Delta

## Purpose

Per-instance metering of executed WebAssembly instructions and memory usage, with configurable execution and memory budgets that surface exhaustion distinctly so embaders can enforce and bill against them.

## ADDED Requirements

### Requirement: Per-Instance Instruction Accounting

The runtime SHALL maintain a per-instance counter of executed WebAssembly instructions, charged on the interpreter execution path used by the embedder. The counter SHALL count guest instructions only — calling an imported host function SHALL NOT increase it. The counter SHALL be queryable by the embedder at any time.

#### Scenario: Counter queryable

- **WHEN** an embedder queries an instance's metering data after executing a function
- **THEN** the returned executed-instruction count SHALL be greater than zero and SHALL equal the instructions executed since instantiation

#### Scenario: Host calls excluded

- **WHEN** a guest calls an imported host function
- **THEN** the host function's own execution SHALL NOT be charged to the instance's instruction count

### Requirement: Monotonic Instruction Count

The executed-instruction counter SHALL NOT decrease during an instance's lifetime.

#### Scenario: Repeated samples monotonic

- **WHEN** an embedder samples the instruction counter multiple times while an instance executes
- **THEN** each sample SHALL be greater than or equal to the previous sample

### Requirement: Memory Usage Reporting

The runtime SHALL report an instance's committed linear-memory usage as its owned pages (excluding shared-region pages), observable through the metering interface.

#### Scenario: Growth reflected

- **WHEN** an instance grows its linear memory
- **THEN** the instance's reported memory usage SHALL reflect the additional committed pages

### Requirement: Configurable Execution Budget

The runtime SHALL allow an embedder to set and reset a per-instance execution budget (a maximum instruction count). When execution reaches the budget, the runtime SHALL stop execution and surface a budget-exhausted outcome with a trap code distinct from other faults. With no budget set, execution SHALL be unbounded.

#### Scenario: Budget exhaustion traps distinctly

- **WHEN** an instance executes more instructions than its configured execution budget
- **THEN** execution SHALL stop with the budget-exhausted trap code

#### Scenario: Budget resettable

- **WHEN** an embedder resets an instance's execution budget between invocations
- **THEN** the new budget SHALL be enforced from the reset onward

#### Scenario: Unset budget unbounded

- **WHEN** an instance has no execution budget configured
- **THEN** execution SHALL proceed without an instruction ceiling

### Requirement: Configurable Memory Budget

The runtime SHALL allow an embedder to set a per-instance memory budget (a maximum committed page count). A memory growth that would exceed the budget SHALL fail with a memory-limit outcome distinct from other faults.

#### Scenario: Growth beyond budget fails

- **WHEN** an instance attempts to grow memory beyond its configured memory budget
- **THEN** the growth SHALL fail with the memory-limit trap code
