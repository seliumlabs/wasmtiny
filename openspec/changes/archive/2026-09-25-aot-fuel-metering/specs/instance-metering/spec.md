# Spec Delta

## MODIFIED Requirements

### Requirement: Per-Instance Instruction Accounting

The runtime SHALL maintain a per-instance counter of metering units charged on
both execution paths (interpreter and AOT). The counter SHALL count guest work
only — calling an imported host function SHALL NOT increase it. On the
interpreter path a unit is one executed instruction; on the AOT path units are
size-weighted fuel — a function's static instruction count charged at entry, and
a loop body's static instruction count charged at each loop back-edge — so the
AOT count approximates rather than exactly equals executed instructions. The
counter SHALL be queryable by the embedder at any time.

#### Scenario: Counter queryable

- **WHEN** an embedder queries an instance's metering data after executing a function
- **THEN** the returned metering-unit count SHALL be greater than zero and SHALL equal the units charged since instantiation

#### Scenario: Host calls excluded

- **WHEN** a guest calls an imported host function
- **THEN** the host function's own execution SHALL NOT be charged to the instance's counter

#### Scenario: AOT execution charges the counter

- **WHEN** an exported function is invoked through the AOT path
- **THEN** the instance's counter SHALL increase by at least the invoked function's fuel charge

### Requirement: Monotonic Instruction Count

The metering counter SHALL NOT decrease during an instance's lifetime. If the
counter reaches its maximum value, it SHALL saturate there (staying pinned)
rather than wrap, on both execution paths.

#### Scenario: Repeated samples monotonic

- **WHEN** an embedder samples the metering counter multiple times while an instance executes
- **THEN** each sample SHALL be greater than or equal to the previous sample

#### Scenario: Counter saturates, never wraps

- **WHEN** the metering counter reaches its maximum value
- **THEN** subsequent charges keep it pinned at that value rather than wrapping it to a smaller value, and the runtime reports the saturation once at error level

### Requirement: Configurable Execution Budget

The runtime SHALL allow an embedder to set and reset a per-instance execution
budget (a maximum metering-unit count), on both the interpreter and AOT paths.
When the counter reaches the budget, the runtime SHALL stop execution and
surface a budget-exhausted outcome with a trap code distinct from other faults.
On the AOT path a charge is compared against the allowance the invocation was
granted at its most recent flush point (invocation start, or a host-call
boundary within the invocation), and the trap is raised at a charge point
(function entry or loop back-edge), so the counter may overshoot the budget by
at most one charge plus any units other concurrent invocations have not yet
flushed. With no budget set, execution SHALL be unbounded.

#### Scenario: Budget exhaustion traps distinctly

- **WHEN** an instance executes more metering units than its configured execution budget
- **THEN** execution SHALL stop with the budget-exhausted trap code

#### Scenario: Budget resettable

- **WHEN** an embedder resets an instance's execution budget between invocations
- **THEN** the new budget SHALL be enforced from the reset onward

#### Scenario: Unset budget unbounded

- **WHEN** an instance has no execution budget configured
- **THEN** execution SHALL proceed without a metering-unit ceiling

#### Scenario: AOT budget exhaustion traps distinctly

- **WHEN** an AOT instance executes more metering units than its configured execution budget
- **THEN** execution SHALL stop with the budget-exhausted trap code

#### Scenario: Budget reset reaches a running invocation at its next host call

- **WHEN** an embedder raises or lowers an AOT instance's execution budget while an invocation is running
- **THEN** the remainder of that invocation SHALL be charged against the new budget from its next host-call boundary onward

#### Scenario: Charges are committed when an invocation traps

- **WHEN** an AOT invocation ends in a trap
- **THEN** the metering units it charged before the trap SHALL be reflected in the instance counter
