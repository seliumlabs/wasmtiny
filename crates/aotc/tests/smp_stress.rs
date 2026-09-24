//! Engine-level SMP stress suite against a **real Rust module** (not `wat`).
//!
//! The fixture (`mt_rust_atomics.wasm`, source and build recipe in
//! `mt_rust_atomics/`) is a genuine `wasm32-unknown-unknown` Rust cdylib
//! compiled with nightly `-Zbuild-std` + `+atomics`: its `std::sync::Mutex`
//! and `Condvar` lower to genuine `memory.atomic.wait32`/`notify` futexes,
//! so every lock acquisition and condvar wake inside the guest routes
//! through the engine's per-address waiter registry
//! (`crates/core/src/runtime/shared_memory.rs`). This exercises the exact
//! load-bearing paths the spike findings identified:
//!
//! - two workers (host threads) entering one shared instance, each running
//!   guest worker-pool code that parks on a single shared wake word;
//! - std lock contention (guest futex `Mutex`) across host threads;
//! - concurrent `memory.grow` with visibility into later invocations;
//! - wait/notify storms (raw wake word + std `Condvar`);
//! - atomic RMWs with no lost updates.
//!
//! The fixture exports `__stack_pointer`/`__heap_base` (forced in its
//! `.cargo/config.toml`), so every `invoke_shared` also allocates a
//! per-invocation shadow-stack slot with a PROT_NONE guard page — the real
//! futex protocols above run on top of the per-invocation stack machinery,
//! exercising slot allocation/recycling and concurrent `memory.grow`
//! serialisation under load.
//!
//! Each phase runs **10 consecutive times** on fresh instances.
//!
//! ## Why a real module
//!
//! The earlier two-thread `wat` spike (`concurrency.rs`) was necessary but
//! not sufficient: only a compiler-generated module with the real wasm
//! threads instruction mix (aligned futex words, compare-exchange loops,
//! `memory.grow` from allocator code, std's condvar protocol) reproduces
//! the engine-level interactions the findings describe.
//!
//! ## Root causes found against this module (spike findings 1 and 2)
//!
//! The mt-demo 2-worker hang and OOB trap had two compounding engine
//! defects, both reproduced and fixed here:
//!
//! 1. **Per-address waiter registry under-delivery.** The registry held one
//!    waiter entry (one flag) per address, so `memory.atomic.notify(n)` could
//!    never wake more than one distinct waiter. The guest worker pool parks
//!    N workers on one wake word and std's futex `Mutex`/`Condvar` park one
//!    thread per word: with `notify(2)` waking only one worker, the surplus
//!    slept until timeout — the hang. Fixed by the per-address waiter queue
//!    (`crates/core/src/runtime/shared_memory.rs`): one node per parked
//!    thread, `notify(n)` pops n.
//! 2. **AOT compiler dropped `memory.atomic.wait32/notify` memarg offsets.**
//!    The wait/notify libcalls received the raw stack address without the
//!    memarg offset (loads/stores fold it via `memory_addr`), so every wait
//!    and notify in a real Rust module landed on address 0: futex value
//!    compares read the wrong word, cross-address collisions corrupted the
//!    futex protocol (the condvar waiter parked on the mutex word and was
//!    never woken), and sync state fell apart — the hang and the
//!    out-of-bounds guest accesses. Fixed in `crates/aotc/src/environment.rs`
//!    / `translate.rs`; pinned by `wait_notify_memarg_offsets_are_respected`
//!    in `concurrency.rs`. All pre-existing AOT wat tests used offset 0, so
//!    the defect was invisible until a compiler-generated module arrived.
//!
//! The OOB side of the mt-demo is further helped by the trap-site diagnostic
//! (`AotInstance::last_trap_site`, tested by
//! `trap_site_diagnostics_report_function_and_offset`): the signal handler
//! classifies a fault to a `TrapCode`, and the diagnostic maps the faulting
//! PC back to the wasm function and code offset for symbolisation. `memory.grow`
//! dispatch-state visibility to concurrent invocations is pinned by
//! `concurrent_grow_is_visible_to_later_invocations`.

use std::sync::Arc;

use wasmtiny::{
    aot::{AotInstance, AotLoader, AotModule},
    runtime::{TrapCode, WasmError, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// Guest park timeout backstop (ns): a regression must fail the assertion,
/// not hang the suite.
const PARK_BACKSTOP_NS: i64 = 5_000_000_000;
/// The prebuilt real-Rust-module bytes (see `mt_rust_atomics/README.md`).
const RUST_MODULE: &[u8] = include_bytes!("mt_rust_atomics.wasm");

struct Fixture {
    instance: Arc<AotInstance>,
    park: u32,
    wake: u32,
    reset: u32,
    parked: u32,
    lock_storm: u32,
    lock_value: u32,
    cv_wait: u32,
    cv_bump: u32,
    grow: u32,
    rmw_bump: u32,
    rmw_read: u32,
}

impl Fixture {
    fn new() -> Self {
        let instance = Arc::new(AotInstance::new(&load_rust_module()).expect("instantiate"));
        let idx = |name: &str| {
            instance
                .export_func_index(name)
                .unwrap_or_else(|| panic!("export {name} missing"))
        };
        Self {
            park: idx("worker_park"),
            wake: idx("worker_wake"),
            reset: idx("worker_reset"),
            parked: idx("parked_count"),
            lock_storm: idx("lock_storm"),
            lock_value: idx("lock_value"),
            cv_wait: idx("cv_wait"),
            cv_bump: idx("cv_bump"),
            grow: idx("grow"),
            rmw_bump: idx("rmw_bump"),
            rmw_read: idx("rmw_read"),
            instance,
        }
    }

    fn call(&self, func: u32, args: &[WasmValue]) -> Vec<WasmValue> {
        self.instance
            .invoke_shared(func, args)
            .expect("invocation succeeds")
    }

    fn call_i32(&self, func: u32, args: &[WasmValue]) -> i32 {
        self.call(func, args)[0].i32().expect("i32 result")
    }

    fn parked_count(&self) -> u32 {
        self.call_i32(self.parked, &[]) as u32
    }

    /// Wakes parked workers until none remain, returning the total notify
    /// count.
    ///
    /// The guest bumps its parked counter *before* registering its futex
    /// waiter, so a single wake's return count is racy by design: a worker
    /// that bumped the counter but had not yet parked when a wake fired is
    /// covered by the word-based re-check in `worker_park` (it returns 0 when
    /// the wake word is already set). Draining `parked_count` to zero is the
    /// deterministic assertion — every worker returns woken, none times out —
    /// while the exact notify(N) == N semantics are pinned deterministically
    /// in the host-side registry tests (`host_region_wait_notify`).
    fn drain_wake(&self, count: u32) -> u32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut total = 0u32;
        while self.parked_count() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "workers never all woken (parked={})",
                self.parked_count()
            );
            total += self.call_i32(self.wake, &[WasmValue::I32(count as i32)]) as u32;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        total
    }
}

fn load_rust_module() -> AotModule {
    let bytes =
        compile_artifact(RUST_MODULE, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

/// Spike finding 2 + 5: concurrent `memory.grow` from the real module. Each
/// thread grows the shared memory; the old sizes across both threads must be
/// strictly increasing (no grow is lost or double-applied) and every later
/// invocation observes the grown size.
#[test]
fn smp_rust_concurrent_memory_grow() {
    for run in 0..10 {
        let fx = Fixture::new();
        let instance = fx.instance.clone();

        let grower = |delta: i32| {
            let instance = instance.clone();
            std::thread::spawn(move || {
                let mut old_sizes = Vec::new();
                for _ in 0..40 {
                    match instance.invoke_shared(fx.grow, &[WasmValue::I32(delta)]) {
                        Ok(values) => {
                            old_sizes.push(values[0].i32().expect("old size"));
                        }
                        Err(WasmError::Trap(TrapCode::MemoryLimitExceeded)) => break,
                        Err(other) => panic!("unexpected grow outcome: {other}"),
                    }
                }
                old_sizes
            })
        };

        let a = grower(1);
        let b = grower(1);
        let mut sizes = a.join().expect("grower A survives");
        sizes.extend(b.join().expect("grower B survives"));
        sizes.sort_unstable();

        // The module starts at 17 pages; 80 single-page grows can commit at
        // most 80, all must be distinct and strictly increasing.
        assert!(
            sizes.windows(2).all(|w| w[0] < w[1]),
            "run {run}: grow old-sizes must be strictly increasing (no lost or double grows): {sizes:?}"
        );
        assert_eq!(sizes.len(), 80, "run {run}: all 80 grows committed");

        // A later invocation observes the grown memory. The module exports
        // `__stack_pointer` (init 1048576), so each concurrent invocation
        // also carves an engine stack slot: 16 usable pages + 1 guard page.
        // The two grower threads run sequentially within themselves, so at
        // most two slots exist; the guest-visible size is therefore the
        // module's 17 initial pages + 80 guest grows + 1..2 slots of 17.
        const SLOT_PAGES: u32 = 17;
        let memory = instance.memory_handle(0).expect("memory");
        let size = memory.lock().expect("lock").size();
        let extra = size - (17 + 80);
        assert!(
            extra == SLOT_PAGES || extra == 2 * SLOT_PAGES,
            "run {run}: grown size {size} must be the guest's 80 grows plus 1-2 engine stack slots \
             (17 pages each), got extra {extra}"
        );
    }
}

/// Spike finding 5: atomic RMWs from two host threads over the real module
/// must not lose updates (the memory.atomic.rmw.add path).
#[test]
fn smp_rust_rmw_no_lost_updates() {
    for run in 0..10 {
        let fx = Fixture::new();
        const ITERS: u32 = 2000;

        let a = {
            let instance = fx.instance.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERS {
                    instance.invoke_shared(fx.rmw_bump, &[]).expect("A bump");
                }
            })
        };
        let b = {
            let instance = fx.instance.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERS {
                    instance.invoke_shared(fx.rmw_bump, &[]).expect("B bump");
                }
            })
        };
        a.join().expect("A survives");
        b.join().expect("B survives");

        let total = fx.call_i32(fx.rmw_read, &[]) as u32;
        assert_eq!(total, 2 * ITERS, "run {run}: no lost RMWs");
    }
}

/// Spike finding 5: std lock contention — two host threads run guest
/// `std::sync::Mutex` (futex-backed) accumulate into one shared u64. Every
/// update must land: the final value equals the sum, proving the futex
/// wait/notify path loses neither wakes nor lock handoffs.
#[test]
fn smp_rust_std_lock_contention() {
    for run in 0..10 {
        let fx = Fixture::new();
        const ITERS: u32 = 2000;

        let a = {
            let instance = fx.instance.clone();
            std::thread::spawn(move || {
                instance.invoke_shared(fx.lock_storm, &[WasmValue::I32(ITERS as i32)])
            })
        };
        let b = {
            let instance = fx.instance.clone();
            std::thread::spawn(move || {
                instance.invoke_shared(fx.lock_storm, &[WasmValue::I32(ITERS as i32)])
            })
        };

        // No lost updates: each lock()/unlock() critical section is serialised by
        // the guest futex, so the shared accumulator is exact. (Each thread's
        // *returned* sum is interleaving-dependent — it accumulates the
        // running counter — so only the shared final value is asserted.)
        a.join().expect("thread A survives").expect("A lock_storm");
        b.join().expect("thread B survives").expect("B lock_storm");
        let value = fx.call(fx.lock_value, &[]);
        assert_eq!(
            value,
            vec![WasmValue::I64(2 * ITERS as i64)],
            "run {run}: no lost lock updates"
        );
    }
}

/// Spike finding 1 + 5: wait/notify storms.
///
/// Phase A: a raw wake-word storm — two workers repeatedly park on the word
/// and are woken, 100 rounds, all woken every round.
/// Phase B: a std `Condvar` storm — one waiter parks on a futex condvar
/// while another thread bumps it 50 times; the waiter must observe all 50.
#[test]
fn smp_rust_wait_notify_storm() {
    for run in 0..10 {
        let fx = Fixture::new();

        // Phase A: raw wake-word storm.
        for round in 0..100 {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let instance = fx.instance.clone();
                    std::thread::spawn(move || {
                        instance.invoke_shared(fx.park, &[WasmValue::I64(PARK_BACKSTOP_NS)])
                    })
                })
                .collect();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while fx.parked_count() < 2 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "run {run} round {round}: workers never parked"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            // Drain until both workers have returned; the joins assert both
            // were woken (0), not timed out (2).
            fx.drain_wake(2);
            for handle in handles {
                let result = handle.join().expect("worker survives");
                assert_eq!(
                    result,
                    Ok(vec![WasmValue::I32(0)]),
                    "run {run} round {round}: woken not timed out"
                );
            }
            fx.call(fx.reset, &[]);
        }

        // Phase B: std Condvar storm. The waiter's `cv_wait` has a bounded
        // per-wait timeout, so a lost notify fails the assertion instead of
        // hanging the suite.
        let waiter = {
            let instance = fx.instance.clone();
            std::thread::spawn(move || {
                instance.invoke_shared(
                    fx.cv_wait,
                    &[WasmValue::I32(50), WasmValue::I64(5_000_000_000)],
                )
            })
        };
        for _ in 0..50 {
            fx.call(fx.cv_bump, &[]);
        }
        let result = waiter.join().expect("waiter survives");
        assert_eq!(
            result,
            Ok(vec![WasmValue::I32(50)]),
            "run {run}: condvar waiter must observe all 50 bumps (no lost wakes)"
        );
    }
}

/// Spike finding 1 + 5: the guest worker pool — N workers park on ONE shared
/// wake word, a dispatcher `worker_wake(N)` must release N distinct waiters.
///
/// Reproduces the 2-worker hang against the real Rust module: with the old
/// single-flag registry, `notify(N)` woke at most one waiter and the
/// surplus slept out the backstop (returning 2). Runs 10 times with 2 and 4
/// workers.
#[test]
fn smp_rust_worker_pool_wakes_all() {
    for run in 0..10 {
        for workers in [2usize, 4] {
            let fx = Fixture::new();
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let instance = fx.instance.clone();
                    std::thread::spawn(move || {
                        instance.invoke_shared(fx.park, &[WasmValue::I64(PARK_BACKSTOP_NS)])
                    })
                })
                .collect();

            // Wait until every worker has registered as parked before waking,
            // so the drain below has real waiters to release.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while fx.parked_count() < workers as u32 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "run {run}/{workers}: workers never all parked (parked={})",
                    fx.parked_count()
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            // Drain until every worker has returned; each worker's join
            // asserts it was woken (0), not timed out (2).
            fx.drain_wake(workers as u32);

            for handle in handles {
                let result = handle.join().expect("worker thread survives");
                assert_eq!(
                    result,
                    Ok(vec![WasmValue::I32(0)]),
                    "run {run}/{workers}: every parked worker must be woken (0), not time out (2)"
                );
            }

            // Reset the wake word for the next round.
            fx.call(fx.reset, &[]);
            assert_eq!(fx.parked_count(), 0, "run {run}/{workers}: pool reset");
        }
    }
}
