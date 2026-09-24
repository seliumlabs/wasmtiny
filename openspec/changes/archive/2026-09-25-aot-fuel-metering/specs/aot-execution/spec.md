# Spec Delta

## ADDED Requirements

### Requirement: Fuel metering and execution budget

AOT execution SHALL charge size-weighted fuel against the instance meter at
function entry and at each loop back-edge, and SHALL trap with the
budget-exhausted trap code when a configured execution budget is exceeded.
Charging SHALL count guest work only — executing an imported host function SHALL
NOT be charged — and SHALL be safe under concurrent invocation of the same
instance.

#### Scenario: Fuel charged at entry and loops

- **WHEN** an exported function is invoked through the AOT path
- **THEN** the instance's metering counter increases by at least the function's fuel charge

#### Scenario: Budget exhaustion traps distinctly

- **WHEN** an AOT instance executes more metering units than its configured execution budget
- **THEN** execution stops with the budget-exhausted trap code

#### Scenario: Host calls excluded

- **WHEN** a compiled module calls an imported host function
- **THEN** the host function's own execution is not charged to the instance's counter

#### Scenario: Concurrent charging is safe

- **WHEN** several host threads invoke the same instance concurrently
- **THEN** the metering counter remains monotonic and no invocation corrupts the meter

### Requirement: Concurrent charging does not serialize invocations

AOT execution SHALL charge an invocation's fuel to invocation-local state and
commit it to the instance's authoritative counter at flush points (a host-call
boundary and the end of the invocation), so that independent CPU-bound
invocations of one instance running on different host threads do not contend on
the same memory on every charge. Every charged unit SHALL still be committed
exactly once, and the authoritative counter SHALL remain monotonic.

#### Scenario: Independent CPU-bound invocations scale with cores

- **WHEN** two CPU-bound tasks run on two host threads against one instance and the same two tasks run back to back on one thread
- **THEN** the two-thread wall time SHALL be less than the serial wall time

#### Scenario: Every unit is committed exactly once

- **WHEN** several host threads each complete many invocations of one instance
- **THEN** the instance counter SHALL equal the sum of the units those invocations charged

#### Scenario: An indirect callee's fuel is attributed to its instance

- **WHEN** a compiled invocation calls a function reached through `call_indirect`
- **THEN** that callee's charges SHALL land in the counter of the instance that owns the callee

## MODIFIED Requirements

### Requirement: Native execution parity

AOT execution SHALL produce the same observable behaviour as interpreter execution: identical results, memory/global/table side effects, imported-function calls, and traps for the same module and inputs. Metering is excluded from this parity: with an execution budget configured, the budget-exhausted trap point may differ between the two paths.

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
