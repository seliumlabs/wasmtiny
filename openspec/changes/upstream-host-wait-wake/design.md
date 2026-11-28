# Design

## Decisions

### One platform matrix, two entry points

`wait` and `wake` live in `runtime::os_wake` next to `emit_wake`, so the
platform table, the macOS exclusion, and the cross-mapping caveats are stated
once. `emit_wake` now delegates to `wake(ptr, u32::MAX)`: emission and embedder
wakes cannot drift.

`SUPPORTED` is a platform fact (`linux | windows | freebsd`), distinct from
`COMPILED_IN`, which additionally requires the `platform-wake-emission`
feature. Embedders gate Stage 2 on the registry's `host_wait_support()`, which
reports `RegistryAndOsWake` only when emission is compiled in.

### WaitOutcome instead of a bool

A bool conflates "woken" with "the word already changed", but a waiter that
must re-check the word does not need to distinguish them. It does need to
distinguish a real timeout, which a bool encodes only by convention. The enum
makes the timeout the only terminal, non-wake outcome.

### FreeBSD wake opcode

The previous emission used `15` (`UMTX_OP_WAIT_UINT_PRIVATE`) for the wake
operation. The header defines `UMTX_OP_WAKE_PRIVATE` as `16`; both the new
`wake` and emission use the correct value so the pair agrees.

## Risks

- **Wake-all on Windows** (`WakeByAddressAll`) replaces the previous single
  wake for emission. Waking more waiters than the notify count is safe: every
  waiter re-checks the word.
- **Cross-mapping limits** on Windows/FreeBSD remain: wakes are keyed by
  virtual address, so only waiters on the engine's own mapping are reached.
  The conformance test uses that mapping.
