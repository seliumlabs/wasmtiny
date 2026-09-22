# Spec Delta

## MODIFIED Requirements

### Requirement: Thread safety

The runtime SHALL support `Send + Sync` on types where it is safe to share across threads. An instance SHALL support concurrent invocation of its exported functions from multiple host threads, each invocation carrying its own execution context (stack) and sharing the instance's linear memory, tables, and globals coherently, without serialising the whole instance behind a single instance-wide lock. Atomic instructions and `memory.atomic.wait`/`memory.atomic.notify` SHALL remain observable across concurrent invocations, and a thread parked in `memory.atomic.wait` SHALL NOT serialise the instance or prevent other threads from executing it.

#### Scenario: Shared instance invoked from multiple threads

- **WHEN** an instance wrapped in an `Arc` has its exported functions invoked concurrently from multiple threads
- **THEN** all invocations complete correctly without data races, panics, or lock poisoning

#### Scenario: Concurrent invocations share state coherently

- **WHEN** two threads invoke functions of the same instance concurrently and each reads or writes shared memory and globals
- **THEN** writes made by one invocation SHALL be observable to the other, with ordering governed by the module's atomics and memory model, and no instance-wide lock SHALL serialise the executions

#### Scenario: One invocation does not block another

- **WHEN** one thread is executing a long-running invocation of an instance
- **THEN** another thread MAY execute a different invocation of that same instance concurrently without waiting for the first to finish

#### Scenario: Waiter parks without blocking other invocations

- **WHEN** one thread is parked in `memory.atomic.wait` on a shared address
- **THEN** another thread SHALL still be able to invoke the instance and SHALL be able to notify the parked waiter

#### Scenario: Memory budget holds under concurrent growth

- **WHEN** multiple invocations grow an instance's memory concurrently while the instance has a memory-page budget configured (see `instance-metering`)
- **THEN** the budget check and the growth commit SHALL be atomic with respect to each other, so the instance's committed pages SHALL never exceed the configured budget
