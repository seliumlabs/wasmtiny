## Why

Interpreter throughput is the bottleneck for executing guest modules in Selium. JIT compilation is rejected because it increases the trusted computing base and cold-start latency. Selium's client-server architecture with a private WASM repository makes ahead-of-time compilation the natural fit: modules are compiled to native code once (repo/server side) into `.aot` artifacts, which the runtime loads and executes directly — with no compiler linked into the runtime.

## What Changes

- New `wasmtiny-aotc` crate: a standalone CLI plus library that compiles `.wasm` → `.aot` using Cranelift (`cranelift-wasm`, `cranelift-codegen`, `wasmparser`). The compiler is never linked into the runtime crate.
- New `.aot` artifact format: header (magic, format version, ABI version, ISA triple, endianness, feature flags), type/import/export metadata, memory/table/global metadata with initialisers, data/element segments, function map, finish-linked machine code blobs, per-function trap tables, and a mandatory integrity section.
- Mandatory integrity verification: the runtime SHALL refuse to load any artifact without a verifiable integrity section — **fail-closed from v1**. The section is scheme-tagged (v1: SHA512) and covers all bytes preceding it, leaving room for PKI signing later without a format change.
- Real AOT execution path in the runtime: artifact loader, verifier dispatch, calling convention with trampolines, explicit bounds checks and entry-time stack-overflow guards, a trap protocol mapping to `WasmError`/`TrapCode`, and function-table dispatch from `invoke_function`.
- Feature rework of the crate: `aot` becomes the default feature (artifact loading, integrity verification, native execution). An `interpreter` feature selects the interpreter path for interpreter-only and differential AOT+interpreter builds; the classic interpreter and `.wasm` parsing are retained rather than gated out. **BREAKING**: the default feature set changes to `default = ["aot"]`.
- A new `aot` module in the runtime exposes the AOT public API (`AotLoader`, `AotInstance`, `AotModule`, ...) alongside the existing interpreter-backed `engine` module, which is unchanged. **BREAKING**: new public module and types.
- The interpreter is retained as the test/reference oracle and for differential testing; it is no longer the only execution mode but remains available in all builds.
- v1 feature coverage matches the interpreter's current coverage: core spec, atomics, threads, bulk memory, reference types, shared regions, and `os_wake` routing. SIMD and other proposals are excluded until separately enabled.
- Spec-parity testing: the vendored `.wast` corpus is exercised through the AOT path, plus rejection tests for malformed/tampered artifacts in the security corpus.
- Trap model: explicit bounds checks (`heap_addr` with Spectre guards) and stack-overflow guards emitted into compiled code, recovered by a process-global signal handler on a dedicated alt-stack that maps the faulting PC to a typed `TrapCode` and returns control to the faulting call's trampoline on the faulting thread — each tenant's trap is confined to its own call, never tearing down the process. Guard pages cover growth, the reserved-range gap, and read-only shared mappings.
- `.aot` artifacts are fully linked by the compiler; the runtime loader performs no relocation. Format version + ABI version + ISA triple are checked strictly; artifacts are regenerated on compiler/ISA upgrades (version skew is an accepted operational policy owned by the repo build).

## Capabilities

### New Capabilities
- `aot-compilation`: the `wasmtiny-aotc` crate — Cranelift-based `.wasm` → `.aot` compiler, feature coverage matching the interpreter, deterministic artifact emission.
- `aot-artifact-format`: the `.aot` binary format — header, sections, finish-linked code, trap tables, mandatory scheme-tagged integrity section, strict loader rejection rules, extensibility for future PKI signing.
- `aot-execution`: the runtime AOT path — artifact loading and verification (fail-closed), calling convention, trampolines, traps, native execution, dispatch integration with `invoke_function`, shared-memory/atomics coverage.

### Modified Capabilities
- `wasm-runtime-core`: the "Accurately named core engine" requirement currently states that no ahead-of-time compilation pipeline SHALL exist and bans `Aot*` naming — this change inverts that by adding a real AOT pipeline exposed through a new `aot` module with `Aot*` types; the interpreter-backed `engine` module itself is unchanged.
- `wasm-interpreter`: the "Classic interpreter execution" requirement currently states the interpreter is the only execution mode — it becomes the reference mode alongside the AOT execution path, retained in all builds.
- `wasm-test-suites`: test suites gain AOT-path execution (spec corpus through the compiled path, artifact verification/rejection tests).

## Impact

- **Code**: new `wasmtiny-aotc` crate (library + CLI binary); runtime gains an `aot` module (artifact loading, verification, exec glue) alongside the existing interpreter-backed `engine/`; `interpreter/`, `loader/`, `parser`, and `validator` are retained and not feature-gated; Cargo feature matrix reworked (`default = ["aot"]`).
- **Public API**: breaking — new `aot` module and `Aot*` types are added; `default` feature set changes; engine/loader module names are unchanged.
- **Dependencies**: runtime gains `sha2` (pure-Rust integrity verification); the runtime must NOT link Cranelift. The compiler crate gains `cranelift-codegen`, `cranelift-wasm`, `wasmparser`.
- **Tests**: spec corpus runs against the AOT path; differential tests run both modes when both features are enabled; security corpus covers integrity-rejection of tampered/unsigned artifacts.
- **Systems**: artifact versioning and regeneration policy on compiler/ISA upgrades; fail-closed integrity verification as the v1 trust model (channel trust + corruption detection; PKI origin authentication later).