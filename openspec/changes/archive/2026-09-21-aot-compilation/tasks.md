## 1. Workspace and Crate Scaffold

- [x] 1.1 Create the `wasmtiny-aotc` workspace crate (library + thin CLI binary) and register it in the workspace; verify `cargo build -p wasmtiny-aotc` succeeds
- [x] 1.2 Add compiler dependencies (`cranelift-codegen`, `cranelift-wasm`, `wasmparser`) to `wasmtiny-aotc` only; verify `cargo tree -p wasmtiny` shows no Cranelift or wasmparser crate in the runtime's tree
- [x] 1.3 Add `aot` (default) and `interpreter` features to the runtime crate; verify `aot`-only and `aot`+`interpreter` combinations both compile cleanly

## 2. Compiler: Minimal Pipeline (seam de-risking)

- [x] 2.1 Implement a stub `FuncEnvironment`/`ModuleEnvironment` that binds a hidden context argument and memory layout to wasmtiny's structures; verify a trivial module (`i32.add`) translates to CLIF without error
- [x] 2.2 Wire the compile path: wasmparser validation → `cranelift-wasm` `FuncTranslator` → `cranelift-codegen` `Context::compile`; verify a machine-code buffer is emitted for the trivial module
- [x] 2.3 Add compiler CLI config with explicit target-ISA default (baseline host triple) and `--target`/feature flags (resolves design open question); verify CLI compiles a `.wasm` to `.aot` and `--help` documents all flags
- [x] 2.4 Implement finish-linking of intra-module direct calls against the final code layout; verify the emitted code has no unresolved relocation records

## 3. Artifact Format

- [x] 3.1 Implement the `.aot` writer: header (magic, format version, ABI version, endianness, feature flags), types, imports/exports, memory/table/global metadata with initialisers, data/elem segments, function map, code, trap tables; verify golden-byte comparison for a fixture artifact
- [x] 3.2 Implement integrity-section emission (scheme `0x01` = SHA512 over all preceding bytes); verify every produced artifact contains the section and the digest matches its bytes
- [x] 3.3 Add the determinism test: compile the same module twice and assert byte-identical output; verify it passes
- [x] 3.4 Implement the unsupported-feature gate (SIMD/v128, GC, exception handling rejected with explicit errors); verify rejection unit tests for each unsupported proposal

## 4. Runtime AOT Loading and Verification

- [x] 4.1 Implement the artifact loader with strict header/section validation (magic, format/ABI versions, ISA, endianness, bounded counts); verify truncated and malformed fixtures produce structured errors without panicking
- [x] 4.2 Implement the verifier dispatch (scheme table; `sha2` for `0x01`; unknown scheme → refusal); verify fail-closed tests: tampered artifact refused and artifact without integrity section refused
- [x] 4.3 Implement executable code mapping (mmap RW→RX flip, W^X discipline for code pages); verify code executes from RX pages and pages are non-writable after the flip
- [x] 4.4 Instantiate artifacts against `Instance` (apply memory/table/global metadata and initialisation segments, resolve imports and exports including host functions); verify instantiation tests cover data/elem segment replay and export resolution without wasm parsing

## 5. Calling Convention and Execution

- [x] 5.1 Define the hidden-context calling convention and per-function entry trampoline from `invoke_function` (host → wasm); verify invoking the compiled `i32.add` export returns 3
- [x] 5.2 Implement typed host-import trampolines marshalling into `HostCaller::call(&[WasmValue])`; verify a host function is invoked with correct arguments and its error propagates synchronously as a trap
- [x] 5.3 Implement `call` and `call_indirect` including funcrefs through imported/shared tables and cross-module dispatch; verify parity tests for shared imported tables and cross-module funcref calls
- [x] 5.4 Emit explicit bounds checks (`heap_addr` with Spectre guards) and trap sites; verify OOB read/write trap with `TrapCode::MemoryOutOfBounds` and `unreachable` traps with `TrapCode::Unreachable` against the OOB corpus fixtures
- [x] 5.5 Emit entry-time stack-overflow checks for non-leaf functions; verify deep recursion traps with `TrapCode::StackOverflow` without exhausting the host stack (exhaust-deep-recursion fixture passes in AOT mode)
- [x] 5.6 Route atomics and `memory.atomic.notify`/`wait` through the `FuncEnvironment` to the existing shared-memory and `os_wake` machinery; verify atomic RMW and notify/wait wake parity tests

## 6. Coverage Parity and Spec Corpus

- [x] 6.1 Add the AOT-path `.wast` runner (compile via `wasmtiny-aotc` library, execute, assert); verify the vendored spec corpus passes every applicable directive in AOT mode
- [x] 6.2 Add differential testing with both features enabled (corpus and regressions through both engines, results compared); verify a `cargo test` run with both features shows no divergence
- [x] 6.3 Extend the security corpus with artifact rejection tests (tampered, unsigned, version-mismatched, ISA-mismatched); verify rejection never panics, hangs, or crashes the host
- [x] 6.4 Confirm existing regression suites (spine_repro, atomic, host_region_wait_notify) pass with the AOT path enabled; verify green `cargo test` with default features

## 7. Feature Default and Tooling

- [x] 7.3 Flip the default feature set to `["aot"]`; verify the default build enables the AOT path and that `--no-default-features --features interpreter` still compiles
- [x] 7.4 Update the `wasmtiny` CLI binary for `.aot` artifact loading; verify end-to-end: `wasmtiny-aotc` compiles a fixture, `wasmtiny` loads and runs it
- [x] 7.5 Update docs and README for the new build modes and artifact workflow; verify README documents the `aot` default, the `interpreter` feature, and the `.wasm` → `.aot` workflow

## 8. Operations and Verification

- [x] 8.1 Verify version-skew behaviour: bump the ABI version in a fixture artifact and confirm the loader refuses it; document the regeneration policy in the loader docs
- [x] 8.2 Run the pre-commit gates (`cargo fmt --all`, `cargo clippy -- -D warnings`, `cargo test`) across all feature combinations; verify a CI matrix for aot-only and aot+interpreter builds is green
- [x] 8.3 Run the fuzz and corpus-runner binaries against the AOT path with the `security-test` feature; verify no new findings and the corpus remains crash-free