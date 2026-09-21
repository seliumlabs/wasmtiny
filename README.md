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

## Credits

This project is based on the excellent [WASM Micro Runtime (WAMR)](https://github.com/bytecodealliance/wasm-micro-runtime).
