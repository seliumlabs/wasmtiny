# Design

## Decisions

### Fact-only, validated probe

`probe(bytes) -> Result<ModuleProbe>` parses with the same `Parser` and
`Validator` the engine loader uses, then moves the fact-bearing fields into a
`ModuleProbe`. Running the validator keeps the probe total on hostile input
and guarantees that facts are reported only for modules the engine would
accept. Allocation stays bounded by the loader's existing untrusted-count
limits.

`ModuleProbe` owns its data (no borrow from a temporary parse). The design
intent — facts without policy, and no second parser downstream — is unchanged.

### Two facts on `Module`

Custom sections and atomic-notify presence are captured during parsing, where
the section bytes and code-section payload are already in hand. Embedders read
them from the probe; the engine does not act on either.

Atomic-notify detection scans the code-section payload for `0xFE 0x00`. Like
the downstream scan it replaces, it can produce a false positive on an
unrelated immediate and never a false negative. Embedders combine it with the
shared-memory declaration, which the validator already requires for atomics.

## Risks

- **Double validation for embedders** that already loaded the module. The probe
  is a spawn-time classification step, not a hot path.
- **False-positive notify** keeps the existing mitigation: the classifier also
  requires shared memory, and the fallback is portable-path latency only.
