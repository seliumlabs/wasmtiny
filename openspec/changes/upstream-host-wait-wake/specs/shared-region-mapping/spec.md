# Spec Delta

## ADDED Requirements

### Requirement: Host wait/wake primitive
The runtime SHALL expose the platform wait-word primitive that its wake
emission targets, so embedders can park on a shared region's host mapping.
`os_wake::SUPPORTED` SHALL report whether a backend exists for the target.
`wait` SHALL park until the word differs from the expected value or the
timeout elapses and SHALL report `Woken`, `TimedOut`, or `Recheck`. `wake`
SHALL wake up to a caller-specified count on the same address, and emission
SHALL use the same backend.

#### Scenario: Paired park and wake
- **WHEN** one thread waits on a shared page word and another stores the new value and calls wake
- **THEN** the waiter SHALL report `Woken` or `Recheck`

#### Scenario: Timeout
- **WHEN** no wake is delivered before the timeout
- **THEN** the waiter SHALL report `TimedOut`

#### Scenario: Value mismatch
- **WHEN** the word already differs from the expected value
- **THEN** the waiter SHALL report `Recheck` without parking until the timeout

#### Scenario: Unsupported platform
- **WHEN** the target has no wired wait-word primitive
- **THEN** `SUPPORTED` SHALL be false and the primitive SHALL NOT be activated

#### Scenario: Shared-page conformance
- **WHEN** the conformance test runs on a supported platform
- **THEN** a waiter parked on an `mmap`'d shared page SHALL be released by wake, and the test SHALL be skipped where `SUPPORTED` is false
