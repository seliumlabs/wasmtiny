## REMOVED Requirements

### Requirement: Accurately named core engine
**Reason**: This requirement constrained the engine to interpreter-backed naming and forbade any ahead-of-time compilation pipeline ("No public item SHALL use `Aot`/`aot_runtime` naming, and no ahead-of-time compilation pipeline SHALL exist"). The `aot-compilation` change introduces a real AOT pipeline exposed through a new `aot` module whose public API uses `Aot*`/`aot` naming, so the constraint is obsolete.
**Migration**: The AOT execution path is exposed through a new `aot` module (`AotLoader`, `AotInstance`, `AotModule`, `AotStore`, ...). The interpreter-backed `engine` module and its `Engine`/`EngineLoader` types are unchanged, and the interpreter remains available (see the `wasm-interpreter` delta for this change).

## ADDED Requirements

### Requirement: Execution build configurations
The crate SHALL build with the AOT execution path enabled by default (via the `aot` feature). The classic interpreter and `.wasm` parsing SHALL remain compiled and available across builds, and an `interpreter` feature SHALL exist to select the interpreter path for interpreter-only and differential AOT+interpreter builds.

#### Scenario: Default build executes AOT artifacts
- **WHEN** the crate is built with default features
- **THEN** it loads, verifies, and executes `.aot` artifacts through the AOT path

#### Scenario: AOT plus interpreter build
- **WHEN** the crate is built with both the AOT and `interpreter` features enabled
- **THEN** both execution paths are available for differential testing