# Spec Delta

## ADDED Requirements

### Requirement: AOT worker pool execution
The AOT runtime SHALL provide a worker pool that runs one shared instance on a
configurable number of OS threads. Each worker SHALL enter a caller-supplied
function with a caller-supplied per-worker argument. The pool SHALL join every
worker through a monitor and invoke a completion callback exactly once. The
pool API SHALL NOT reference embedder ABI, process, or activity types.

#### Scenario: Clean exit
- **WHEN** every worker returns normally
- **THEN** the completion callback SHALL receive a report with no fault

#### Scenario: Per-worker entry argument
- **WHEN** the pool starts N workers
- **THEN** each worker SHALL receive the argument produced for its worker id

### Requirement: AOT worker pool fault and stop handling
The pool SHALL capture the first worker fault with its trap code and trap site.
Stop delivery SHALL be supplied by the embedder through a `StopSignal` and
SHALL be retried while the pool joins. A pool that does not join within its
stop budget SHALL be detached rather than joined, and the outcome SHALL be
reported to the caller.

#### Scenario: Worker trap
- **WHEN** a worker traps
- **THEN** the pool SHALL stop the remaining workers and the report SHALL carry the trap code and trap site

#### Scenario: Stop joins the pool
- **WHEN** the caller stops a running pool
- **THEN** the stop signal SHALL be delivered and the pool SHALL report `Joined` once every worker has exited

#### Scenario: Wedged pool
- **WHEN** a worker does not exit within the stop budget
- **THEN** the pool SHALL be detached and the caller SHALL receive `Wedged` rather than a hung join
