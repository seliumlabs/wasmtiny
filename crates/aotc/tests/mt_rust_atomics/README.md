# mt_rust_atomics — real Rust module for the SMP stress suite

A genuine Rust module (not `wat`) exercising wasm threads semantics,
consumed by `../smp_stress.rs`:

- futex-backed `std::sync::Mutex` / `Condvar` (Rust std on wasm lowers
  these to genuine `memory.atomic.wait32` / `memory.atomic.notify` when
  compiled with the `atomics` target feature — see the engine-side waiter
  registry in `crates/core/src/runtime/shared_memory.rs`).
- a worker-pool park/wake pattern: N workers park on ONE shared wake word,
  a dispatcher `worker_wake(N)` releases up to N distinct waiters
  (`memory.atomic.notify`), exactly the shape that exposed the old
  single-flag-per-address registry under-delivery (spike finding 1).
- `memory.grow` and `memory.atomic.rmw.add` helpers.

The checked-in `mt_rust_atomics.wasm` was built with **nightly** Rust +
`-Zbuild-std` (the released `wasm32-unknown-unknown` std has no atomics):

```console
$ rustup toolchain install nightly --component rust-src
$ cd crates/aotc/tests/mt_rust_atomics
$ cargo +nightly build --release
$ cp ../../../../target/wasm32-unknown-unknown/release/mt_rust_atomics.wasm \
      ../mt_rust_atomics.wasm
```

The `.cargo/config.toml` pins the flags:

- `-C target-feature=+atomics,+bulk-memory,+mutable-globals`
- `-Z build-std=std,panic_abort`
- `-C link-arg=--max-memory=268435456 --shared-memory --export-memory`
- `-C link-arg=--export=__stack_pointer --export=__heap_base`

The `__stack_pointer`/`__heap_base` exports let the engine detect the
module's shadow stack and give every concurrent invocation a private stack
slot (see the per-invocation context contract in `crates/core/src/aot/`).
rustc/LLD does not export them by default, and may GC the shadow-stack
global entirely when nothing references it, so the exports are forced here.

(Only the engine-observable bytecode matters; the exact toolchain version
is not pinned. Rebuilding from `src/lib.rs` with the flags above must
produce a module with the same exports: `worker_park`, `worker_wake`,
`worker_reset`, `parked_count`, `lock_storm`, `lock_value`, `cv_wait`,
`cv_bump`, `grow`, `rmw_bump`, `rmw_read`, plus `__stack_pointer` and
`__heap_base`.)

AGENTS.md forbids WASI — this target is `wasm32-unknown-unknown`, matching
the engine's supported guest target.