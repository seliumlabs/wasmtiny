## Context

The runtime is currently an interpreter-only engine: `loader/` parses and validates `.wasm` into a `Module`, `runtime/` provides `Instance`/`Memory`/`Table`/`Global`/host-function state (mmap-backed, mprotect-grown memories), and `interpreter/exec.rs` executes bytecode with a stack-machine, match-dispatch loop. The `engine/` module is named "Ahead-of-Time" for historical WAMR reasons but dispatches into the interpreter.

See proposal.md - Why: interpreter throughput is the bottleneck, JIT is rejected (TCB + cold-start), and Selium's client-server architecture with a private WASM repository points to ahead-of-time compilation. The compiler must live in a separate binary and never be linked into the runtime. The runtime builds with the AOT path by default, supports an interpreter-only build, and can build with both for differential testing; the interpreter and `.wasm` parsing are retained in every build. Integrity verification of artifacts is fail-closed from v1 (SHA512 default, PKI later).

## Goals / Non-Goals

**Goals:**
- A `.wasm` → `.aot` compiler crate (`wasmtiny-aotc`) whose library is usable in-process by tests, with a thin CLI on top.
- An `.aot` artifact format with a mandatory, scheme-tagged integrity section; the runtime refuses artifacts without a verifiable section.
- A runtime AOT path: artifact loading (strict header/version/ISA checks), integrity verification, native execution with explicit bounds checks and stack-overflow guards, traps mapped to `WasmError`/`TrapCode`.
- The crate builds with the AOT path by default, supports an interpreter-only build, and can build with both for differential testing; the compiler is never linked into the runtime crate.
- v1 feature coverage matches the interpreter's current coverage (core spec, bulk memory, reference types, atomics, threads, shared regions, `os_wake`).
- Spec-corpus parity for the AOT path and differential testing when both features are enabled.

**Non-Goals:**
- No JIT anywhere. No runtime relocations in the loader (artifacts arrive finish-linked; absolute relocations are rejected at compile time). Traps are recovered by a process-global signal handler with per-call recovery (see D5).
- No PKI signing in v1 — only the extensibility point (scheme tag + reserved key-id field) and the mandatory integrity section. HMAC-SHA512 explicitly rejected (see Decisions).
- No SIMD, GC, or exception-handling proposals in v1; unsupported proposals fail compilation with explicit errors rather than falling back.
- No in-process compile feature in the runtime for v1 (the repo/server compiles); the library lives in `wasmtiny-aotc` and can be linked by embedders later without a runtime change.
- No fast-interpreter work; the interpreter is a test oracle, not a performance target.

## Decisions

### D1: Separate `wasmtiny-aotc` workspace crate (library + thin CLI)

The compiler is its own crate: a library exposing `compile(wasm: &[u8]) -> Result<Vec<u8>>` (plus config for ISA/features) and a small `main.rs` CLI (`.wasm` in → `.aot` out).

Rationale: the runtime crate must not even *declare* Cranelift dependencies — a separate crate makes that guarantee structural rather than a dependency-management discipline. The library form is required by the "no external binaries in `cargo test`" spec requirement: the spec-corpus runner compiles modules in-process. Alternatives considered: same-crate bin (pollutes runtime's dependency graph; rejected), workspace member sharing the runtime crate as an optional dependency (unnecessary coupling).

### D2: `cranelift-wasm` with our own `FuncEnvironment`/`ModuleEnvironment`

The compiler pipeline: `wasmparser` (operator reader + `FuncValidator`) → `cranelift-wasm`'s `FuncTranslator` guided by a project-owned `FuncEnvironment` → `cranelift-codegen` → finish-linked machine code.

Rationale: `cranelift-wasm` is explicitly designed for embedding: `FuncEnvironment` defines how memories, tables, globals, direct calls, `call_indirect`, `memory.grow`/`memory.size`, imports and atomics (`memory.atomic.notify`/`wait`) resolve. We bind those to wasmtiny's `Instance`/`Memory`/`HostCaller` structures, so compiled code speaks our ABI, not Wasmtime's. Alternatives considered: Wasmtime's own `FuncEnvironment`/`wasmtime-environ` (bound to the VMContext ABI — would force Wasmtime's instance layout onto us; rejected), `wasmtime-cranelift`'s `Compiler` (same coupling), hand-rolled backend (6–18 months, correctness risk; rejected — Cranelift brings spec-tested lowering, register allocation, multi-arch for free), LLVM (C toolchain conflicts with the fresh-clone test requirement; rejected).

### D3: Hand-rolled binary artifact format, deterministic emission

`.aot` layout (append-only, self-describing sections, all counts validated during load):

```
header    magic "WTA0" | format_version | abi_version | isa triple
          endianness   | feature flags
types     signatures (recgroup-shaped, exactly what exec needs)
imports   module/name/kind/type indices
exports   name -> kind/index
mem/tab/global metadata + const-init expressions (compiler may fold const exponentials)
data/elem segments (replayed at instantiation, bounds-checked)
func map  function index -> signature, code offset/len, trap table offset
code      finish-linked machine code blobs (relocations resolved by compiler)
trap tables  code offset -> TrapCode, per function
integrity  scheme u8 | key_id_len u8 | key_id (v1: absent) | payload = SHA512(every byte preceding the digest)
```

Rationale: no serde in the tree today; a hand-rolled format (WAMR heritage) keeps dependencies minimal and gives precise control over validation strictness. The compiler emits byte-stable output (fixed section order, no timestamps) so that sign-once/distribute is possible when PKI lands. Alternatives: serde/bincode (adds a serialisation stack and weakens strictness guarantees; rejected), embedding the original `.wasm` inside the artifact (doubles size, re-exposes wasm parsing in the runtime; rejected).

### D4: Mandatory scheme-tagged integrity section, fail-closed verification

The compiler always writes the integrity section. The runtime loader verifies it before execution and **refuses to load any artifact without it**. Scheme `0x01` = SHA512 over every byte preceding the digest (header included — header fields such as feature flags are trust inputs), with a reserved length-prefixed key-id field that v1 writes empty and that unkeyed schemes require to be empty. A future signing scheme = new scheme tag + its own key-id policy + new verifier arm; the format and loader architecture do not change, and no artifact regeneration is required.

Verification is a dispatch point in the loader: `scheme u8 → verifier fn` (`sha2` in v1; e.g. `ed25519-dalek` later). The policy is fail-closed from day one by explicit requirement; there is no "dev mode" that skips verification.

Rationale: v1 trust model = channel trust (repo → broker over TLS) + corruption/tampering detection (SHA512). PKI adds origin authentication later with no format or loader change. HMAC-SHA512 was considered as an intermediate and rejected: it needs shared-secret distribution to every broker, and if the channel is trusted it adds nothing over TLS; if it is not trusted, real PKI is wanted instead. Cost accepted: `sha2` (pure-Rust, small) enters the runtime's dependencies; verification is microseconds for typical module sizes.

### D5: Trap model — signal-based recovery confined to the faulting call

Compiled code keeps Cranelift's default trap encoding: wasm `unreachable`, OOB `heap_addr` checks (Spectre-guarded), division, bad conversions and the entry-time stack-overflow check lower to `ud2` (`udf` on aarch64), and guard-page faults (PROT_NONE gap, read-only shared mappings, beyond-capacity) raise SIGSEGV. The runtime installs a **process-global** SIGILL/SIGSEGV/SIGBUS handler on a dedicated `sigaltstack`, plus a process-global registry mapping every loaded code range to its per-function trap table.

On a fault the handler: classifies the faulting PC (a registered trap site → its typed `TrapCode`; any other fault inside a registered code range → `MemoryOutOfBounds`), records the code in a thread-local slot, and `siglongjmp`s back to the `invoke_function` trampoline on the **faulting thread**. Recovery state is per-thread, so a trap is confined to the one call: no process teardown, no other tenant disturbed. This is the containment model a multi-tenant hypervisor requires.

Rationale: Wasmtime's default and the shortest correct path to "typed traps, never crash the host". The earlier preference for status returns was discarded because Cranelift 0.111 offers no status-return trap mode — traps are `ud2`/`udf`/guard-page faults by construction — and reworking the lowering per function would recreate signal-recovery machinery with worse determinism. `sigaltstack` makes stack-overflow traps recoverable even on an exhausted stack. Signal SA is installed once at engine init on supported platforms (fails closed — init error — where a given OS cannot provide it).

### D6: Calling convention — hidden context argument, finish-linked code, typed trampolines

Every compiled function receives a hidden context pointer as its first argument (marked non-wasm via `is_wasm_parameter`), pointing to per-instance state the `FuncEnvironment` lays out (memory bases, global bases, table bases, host-import table, stack limit). Host imports are reached through typed trampolines that marshal from the compiled ABI into the existing `HostCaller::call(&[WasmValue])` shape and back; `TypedHostImport` is reused. `invoke_function` dispatches through a per-function trampoline into native entry points instead of `Interpreter::execute_function`.

Relocations (direct calls between functions, calls to trampolines) are resolved by the compiler before emission — the loader performs no relocation and has no code-mutation capability after verification. This keeps the loader read-only after the integrity check and shrinks the attack surface.

### D7: Feature matrix

```
wasmtiny features:
  default      = ["aot"]
  aot          artifact loading + integrity verification + native exec (+ sha2)
  interpreter  selects the interpreter path for interpreter-only and differential builds
```

The `aot` feature adds artifact loading, integrity verification, and native execution; the interpreter and `.wasm` parsing (`src/loader/`, `src/interpreter/`) are not gated out and remain available in every build. `interpreter`-only (the interpreter without AOT) remains possible for downstream users. Both enabled yields the differential-test configuration. The real AOT shape is exposed through a new `aot` module (`AotModule`, `AotLoader`, ...) rather than renamed `engine` types, retiring the old requirement that banned those names.

### D8: v1 coverage parity enforced by compiler feature gate

The compiler's feature set matches the interpreter's current coverage: core spec, bulk memory, reference types, atomics (lock-free lowering where the target ISA supports it), threads/shared memory, and `os_wake` routing (`memory.atomic.notify`/`wait` via the `FuncEnvironment` into the existing `shared_notify`/`shared_wait`/`os_wake` machinery). A module using anything outside that set (e.g. SIMD) fails compilation with an explicit unsupported-feature error — there is no silent per-function or per-opcode fallback in single-mode AOT. Rejected modules must not be executed.

### D9: Shared-region memory access — flattened heap bound, guard-page traps

wasmtiny lays out owned pages at `[0, len)` and shared regions top-down inside the same mmap reservation; Cranelift's flat `HeapData` is bound-checked against a single bound. To make compiled loads/stores behave exactly like the interpreter (`is_valid_access` over owned + shared ranges, `check_writable` for read-only ranges):

- The compiled heap **bound is the reservation capacity, not the current `len`**, so owned pages and shared mappings are all in-bounds; `memory.size` still reports the owned page count from the descriptor, not the capacity.
- The PROT_NONE gap between `len` and the first shared mapping, read-only shared pages, and the reserved-but-ungrown range all fault and are mapped to `TrapCode::MemoryOutOfBounds` by the D5 handler — matching interpreter OOB and "read-only shared region write" semantics without extra codegen. The reader-slot allowance (`check_writable`) falls out of the mapping's own RW page.
- `memory.atomic.notify`/`wait` are overridable translation points and route through the existing `notify`/`wait32`/`wait64` machinery (which already understands shared ranges) via a vmctx-resident libcall table.

### D10: Cross-module dispatch — store-global fat call targets and canonical type ids

Tables may be imported/shared across modules, and `call_indirect`/imported calls must reach functions whose `vmctx` differs from the caller's. The store-global `native_funcs` registry already assigns every funcref a stable handle; AOT expands it:

- Each store-native handle resolves to a **`FatCallTarget { entry: *const u8, vmctx: *const VmCtx, type_id: u32 }`** in a store-wide table the vmctx points at. Imported-function calls and `call_indirect` both load the fat target, so the callee's own `vmctx` rides along — cross-module calls and import aliasing are structurally correct.
- `type_id` is a canonical signature id from a store-wide registry (deduped by `FunctionType` equality); each module's vmctx carries a per-type `signature_ids[]` array the runtime fills at instantiation. Compiled `call_indirect` compares `fat.type_id` against the expected slot id and traps `IndirectCallTypeMismatch` on mismatch, and `CallIndirectNull` on a null entry.
- Table cells remain 4-byte store handles (with a null sentinel); the artifact's element segments quote function indices that the loader rewrites into store handles via the same `ref.func` const-expr path the interpreter uses, so `(ref null func)` through an imported table type-checks identically to interpreter execution.

## Risks / Trade-offs

- **[Cranelift/wasmparser version drift]** → Pin versions in `Cargo.lock`; the compiler feature gate provides an explicit rejection path when the operator surface grows (e.g. GC); regen policy (below) absorbs upgrades.
- **[Codegen bugs = sandbox escapes]** → Same lowering Wasmtime ships and the spec suite exercises; the security corpus and fuzz binaries (`wasmtiny-fuzz`, `wasmtiny-corpus-runner`) gain AOT-path coverage; integrity verification sits before any code is mapped executable.
- **[Native recursion exhausts host stack]** → Entry-time stack-overflow check per non-leaf function, trapping with `StackOverflow` before host-stack exhaustion; matches the `exhaust-deep-recursion` corpus fixture's invariant (trap, never crash).
- **[Artifact version skew on upgrade]** → Header carries format + ABI + ISA; loader rejects mismatches; regeneration on compiler/ISA change is an accepted operational policy owned by the repo build.
- **[Compiler determinism regressions]** → Emission rules forbid timestamps and unstable ordering; enforced by a determinism test (compile twice, bytes identical) so sign-once/distribute stays possible.
- **[Explicit bounds checks cost throughput]** → Accepted for determinism and corpus-compatible traps; a future signal-based trap design could lift it, but that is a separate decision and out of v1.
- **[`sha2` in the tiny runtime]** → Small pure-Rust crate; verification cost negligible vs load; the price of fail-closed integrity.
- **[Per-target artifacts]** → Artifacts are ISA-specific; the private repo must compile per deployment target (accepted; part of the regen policy).

## Migration Plan

1. Land `wasmtiny-aotc` (crate + CLI) and the artifact format with its own tests — no runtime change yet; artifacts can be produced and inspected.
2. Add the `aot` feature to the runtime (non-default): loader, verifier, exec glue, trampolines; a minimal compiled-function smoke path (e.g. `i32.add`) to de-risk the seam early.
3. Widen AOT coverage to parity; run the vendored `.wast` corpus through the AOT path; add tampered/unsigned/mismatched-version rejection tests to the security corpus; add differential runs when both features are enabled.
4. Flip `default = ["aot"]`; update `wasm-runtime-core`/`wasm-interpreter`/`wasm-test-suites` specs accordingly.
5. Release notes mark the feature flip and the new AOT public module as breaking.

Rollback: the `interpreter` feature remains available; reverting the default feature restores the previous behaviour without data migration (artifacts are generated artifacts, regenerable).

## Open Questions

- Exact scheme byte assignments and key-id encoding for future PKI schemes — the section is self-describing; decided when signing work starts.
- Target-ISA baseline policy for `cranelift-codegen` (baseline vs +AVX etc.) — a compiler-CLI config default; does not affect the format or ABI.
- Whether a future in-process compile feature should link `wasmtiny-aotc` directly — the library split makes it possible without runtime changes; deferred.
- Whether PKI signing will use raw public keys or certificate chains — deferred to the signing change; the reserved key-id field accommodates either.