# Wasmtiny

A small WebAssembly runtime designed specifically for the needs of the [Selium project](https://github.com/seliumlabs/selium).

You are most welcome to use this runtime in your projects, however please note that the project's direction will be heavily influenced by the needs of Selium.

## Execution modes

Wasmtiny executes WebAssembly ahead-of-time: a module is compiled once with the
separate `wasmtiny-aotc` compiler crate into a finish-linked `.aot` artifact,
which the runtime loads, verifies, and executes natively. The interpreter is an
optional, feature-gated reference mode retained for differential testing.

Cargo features:

| Feature | Enables |
| --- | --- |
| `aot` (default) | Artifact loading, mandatory integrity verification (`sha2`), native execution |
| `interpreter` | The classic interpreter execution path (`.wasm` loading) |
| `cli` | The `wasmtiny` CLI binary |
| `security-test` | Corpus-runner / fuzz instrumentation (test builds only) |
| `platform-wake-emission` | OS-level wake emission for shared-region `memory.atomic.notify` |

The default build enables `aot`. `wasmtiny-aotc` (and Cranelift/wasmparser) are
never linked into the runtime crate.

## Compiling a module

```sh
# .wasm -> .aot (the compiler only exists in the standalone crate/binary)
wasmtiny-aotc module.wasm -o module.aot
```

`.aot` artifacts are finish-linked machine code for a specific target ISA, with
a mandatory SHA512 integrity section. The runtime loader refuses artifacts that
are tampered with, unsigned, or built for a different format/ABI/ISA; the owning
repo build regenerates artifacts on compiler or ISA upgrades.

The current artifact ABI version is **3**. ABI v3 added the `meter` field to
the per-instance context for AOT fuel metering (below); a v2 artifact is
refused by the loader with an error naming the version and the remedy
(`regenerate the artifact`). Any repo holding prebuilt `.aot` files must
recompile them with a matching `wasmtiny-aotc` before upgrading the runtime.

## Metering

The runtime keeps one lock-free per-instance meter, shared by both execution
paths, and exposes it through `stats()` (executed metering units plus committed
memory pages) and `set_execution_budget` / `set_memory_budget`.

- **Interpreter path:** one unit per executed instruction (exact).
- **AOT path:** size-weighted fuel. Compiled code charges a function's static
  instruction count at function entry and a loop body's static instruction
  count at each loop back-edge, so the AOT count approximates rather than
  exactly equals executed instructions. Only guest work is charged: executing
  an imported host function adds nothing.
- **Execution budget:** an embedder may set a maximum metering-unit count
  (`None` = unbounded). On the AOT path the inline charge traps
  `ExecutionBudgetExceeded` when the invocation alone exhausts the allowance the
  instance had left, so the counter may overshoot by at most one charge. The
  allowance is snapshotted when an invocation starts and refreshed at each flush
  point (a host-call boundary and invocation end), so a reset between
  invocations takes effect on the next invocation — the same granularity as the
  interpreter's cached budget snapshot.
- Charging is a single atomic add reached through the `vmctx.meter` pointer (no
  per-charge function call). Each invocation charges an *invocation-local* cell
  — a per-invocation copy of that pointer — and the runtime drains it into the
  authoritative meter at flush points. Concurrent invocations of one instance
  therefore never contend on one shared cache line, so two workers running
  independent CPU-bound tasks scale with cores instead of ping-ponging the
  meter.
- The counter saturates at `u64::MAX` and never wraps; saturation (only
  reachable after ~2^64 metering units) is reported once per instance at
  `error` level through the `log` facade, so embedders with a logger installed
  can detect it in production.

## Credits

This project is based on the excellent [WASM Micro Runtime (WAMR)](https://github.com/bytecodealliance/wasm-micro-runtime).
