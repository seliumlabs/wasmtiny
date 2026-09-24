//! Concurrent (SMP) execution of one AOT instance from multiple host threads.
//!
//! Verifies the `concurrent-instance-execution` contract: an instance wrapped
//! in an `Arc` can be invoked from many threads at once, each invocation
//! carrying its own execution context (stack) and sharing linear memory,
//! tables and globals coherently, without serialising the whole instance
//! behind a single instance-wide lock.
//!
//! Spike findings (task 1.1):
//!
//! - **Memory**: guest loads/stores are direct mmap accesses; `memory.grow` is
//!   an `mprotect` inside the memory lock. Concurrent loads/stores never take
//!   the lock and cannot tear — growth only makes `PROT_NONE` pages accessible,
//!   it never moves or rewrites data. Verified by `grow_racing_reads_never_tears`.
//! - **Tables**: `call_indirect` reads the shared `TableCells` holder lock-free
//!   (immutable base, racy-safe `len`); mutations hold the table mutex.
//!   Verified by `concurrent_call_indirect_is_correct`.
//! - **Globals**: AOT mutable globals are lock-free 8-byte cells; aligned
//!   scalar accesses are atomic on the supported 64-bit targets.
//!   Verified by `concurrent_mutable_global_access_is_correct`.
//! - **Host-callback re-entrancy**: the AOT `host_call` libcall holds the
//!   dispatch store lock across the host callback, so a host function that
//!   re-enters the instance deadlocks. This is pre-existing single-threaded
//!   behaviour and is forbidden by the "Callback-safe lock discipline"
//!   requirement, so it is preserved rather than "fixed" here; concurrent
//!   invocations do not change it.

use std::sync::Arc;

use wasmtiny::{
    aot::{AotInstance, AotLoader, AotModule},
    runtime::{TrapCode, WasmError, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// Task 4.1: concurrency stress — two threads on a shared instance hammering
/// atomic RMWs over shared linear memory, then a wait/notify handshake. Run
/// 10 consecutive times.
#[test]
fn concurrent_atomic_rmw_and_wait_notify_stress() {
    let source = r#"(module
      (memory 1)
      (func (export "bump") (result i32)
        (i32.atomic.rmw.add (i32.const 0) (i32.const 1)))
      (func (export "wait") (result i32)
        (memory.atomic.wait32 (i32.const 4) (i32.const 0) (i64.const 5000000000)))
      (func (export "notify") (result i32)
        (memory.atomic.notify (i32.const 4) (i32.const 1)))
      (func (export "load") (param i32) (result i32)
        (i32.atomic.load (local.get 0))))"#;

    for run in 0..10 {
        let instance = instantiate(source);

        let a = {
            let instance = instance.clone();
            std::thread::spawn(move || {
                for _ in 0..2000 {
                    instance.invoke_shared(0, &[]).expect("A bump succeeds");
                }
                instance.invoke_shared(1, &[])
            })
        };
        // Give A time to park before B notifies; B's own bumps add margin.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let b = {
            let instance = instance.clone();
            std::thread::spawn(move || {
                for _ in 0..2000 {
                    instance.invoke_shared(0, &[]).expect("B bump succeeds");
                }
                instance.invoke_shared(2, &[])
            })
        };

        let a_result = a
            .join()
            .expect("A survives")
            .expect("A wait returns a value");
        let b_result = b
            .join()
            .expect("B survives")
            .expect("B notify returns a value");
        assert_eq!(a_result, vec![WasmValue::I32(0)], "run {run}: A woken");
        assert_eq!(
            b_result,
            vec![WasmValue::I32(1)],
            "run {run}: B woke one waiter"
        );

        let total = instance
            .invoke_shared(3, &[WasmValue::I32(0)])
            .expect("counter read");
        assert_eq!(
            total,
            vec![WasmValue::I32(4000)],
            "run {run}: no lost atomic RMWs"
        );
    }
}

/// Task 3.2: `call_indirect` is correct under concurrency. The dispatch path
/// reads the shared `TableCells` holder lock-free while `table.grow` mutates
/// the table under its mutex; both must remain coherent.
#[test]
fn concurrent_call_indirect_is_correct() {
    let instance = instantiate(
        r#"(module
          (type $t (func (param i32) (result i32)))
          (func $inc (type $t) (param i32) (result i32)
            (i32.add (local.get 0) (i32.const 1)))
          (table 4 funcref)
          (elem (i32.const 0) $inc $inc $inc $inc)
          (func (export "callat") (param i32 i32) (result i32)
            (call_indirect (type $t) (local.get 1) (local.get 0)))
          (func (export "grow") (param i32) (result i32)
            (table.grow 0 (ref.func $inc) (local.get 0))))"#,
    );

    let dispatchers: Vec<_> = (0..2)
        .map(|thread| {
            let instance = instance.clone();
            std::thread::spawn(move || {
                for i in 0..2000i32 {
                    let slot = (i + thread) % 4;
                    let result = instance
                        .invoke_shared(1, &[WasmValue::I32(slot), WasmValue::I32(i)])
                        .expect("call_indirect succeeds");
                    assert_eq!(result, vec![WasmValue::I32(i + 1)]);
                }
            })
        })
        .collect();

    // A concurrent table.grow publishes new slots while the dispatchers read
    // the holder; the grown slots must be dispatchable afterwards.
    let grown = instance
        .invoke_shared(2, &[WasmValue::I32(100)])
        .expect("table.grow succeeds");
    assert_eq!(grown, vec![WasmValue::I32(4)], "old table size");

    for dispatcher in dispatchers {
        dispatcher.join().expect("dispatcher thread survives");
    }

    let result = instance
        .invoke_shared(1, &[WasmValue::I32(50), WasmValue::I32(41)])
        .expect("dispatch through a grown slot");
    assert_eq!(result, vec![WasmValue::I32(42)]);
}

/// Spike finding 2 (memory.grow dispatch-state visibility): a concurrent
/// `memory.grow` must be visible to later per-invocation contexts.
///
/// Compiled heap accesses bounds-check against the memory's *capacity* (a
/// fixed reservation; newly grown pages become accessible via `mprotect`),
/// so growth never makes compiled loads/stores stale. This test pins the
/// observable side: after a grow commits on another thread, a fresh
/// `invoke_shared` sees the new size through `memory.size` AND can read the
/// grown pages, which traps `MemoryOutOfBounds` only if the per-invocation
/// context carried a stale view of the memory.
#[test]
fn concurrent_grow_is_visible_to_later_invocations() {
    let source = r#"(module
          (memory 1 10)
          (func (export "grow") (param i32) (result i32)
            (memory.grow (local.get 0)))
          (func (export "size") (result i32)
            (memory.size))
          (func (export "write") (param i32) (param i32)
            (i32.store (local.get 0) (local.get 1)))
          (func (export "read") (param i32) (result i32)
            (i32.load (local.get 0))))"#;

    for run in 0..10 {
        let instance = instantiate(source);
        let grower = spawn_invoke(instance.clone(), 0, vec![WasmValue::I32(2)]);
        // Wait for the grow to commit (the join is the synchronisation).
        grower
            .join()
            .expect("grower survives")
            .expect("grow succeeds");

        // A fresh invocation must observe the grown size...
        let size = instance.invoke_shared(1, &[]).expect("size query");
        assert_eq!(
            size,
            vec![WasmValue::I32(3)],
            "run {run}: grown size visible"
        );

        // ...and be able to access the newly grown pages.
        instance
            .invoke_shared(2, &[WasmValue::I32(2 * 65536), WasmValue::I32(42)])
            .expect("write to grown page succeeds");
        let read = instance
            .invoke_shared(3, &[WasmValue::I32(2 * 65536)])
            .expect("read from grown page succeeds");
        assert_eq!(
            read,
            vec![WasmValue::I32(42)],
            "run {run}: grown page coherent"
        );
    }
}

/// Task 3.4: the memory-page budget check and the growth commit are one
/// atomic step, so concurrent grows cannot push an instance past its
/// configured budget. With a budget of 8 pages and two threads each growing
/// by 2 from a 1-page memory, exactly three grows succeed (1→3→5→7) and
/// every further grow traps with `MemoryLimitExceeded`.
#[test]
fn concurrent_grows_respect_memory_budget() {
    let instance = instantiate(
        r#"(module
          (memory 1 20)
          (func (export "grow") (param i32) (result i32)
            (memory.grow (local.get 0))))"#,
    );
    instance.set_memory_budget(Some(8)).expect("budget set");

    let growers: Vec<_> = (0..2)
        .map(|_| {
            let instance = instance.clone();
            std::thread::spawn(move || {
                let mut successes = Vec::new();
                let mut budget_traps = 0usize;
                for _ in 0..50 {
                    match instance.invoke_shared(0, &[WasmValue::I32(2)]) {
                        Ok(values) => successes.push(values[0].i32().expect("old size")),
                        Err(WasmError::Trap(TrapCode::MemoryLimitExceeded)) => budget_traps += 1,
                        Err(other) => panic!("unexpected grow outcome: {other}"),
                    }
                }
                (successes, budget_traps)
            })
        })
        .collect();

    let mut successes = Vec::new();
    let mut budget_traps = 0usize;
    for grower in growers {
        let (mut s, t) = grower.join().expect("grower thread survives");
        successes.append(&mut s);
        budget_traps += t;
    }

    // Exactly three grows may commit (1→3→5→7); everything past the budget
    // traps with the memory-limit code. The order of the old sizes is
    // interleaving-dependent, so compare as a set.
    let mut old_sizes: Vec<i32> = successes.to_vec();
    old_sizes.sort_unstable();
    assert_eq!(
        old_sizes,
        vec![1, 3, 5],
        "committed grows and their old sizes"
    );
    assert_eq!(budget_traps, 97, "all remaining grows trap on the budget");

    // The committed page count never exceeds the configured budget.
    let stats = instance.stats().expect("stats query");
    assert!(
        stats.memory_pages <= 8,
        "committed pages {} exceed budget 8",
        stats.memory_pages
    );
    assert_eq!(stats.memory_pages, 7);
}

/// Task 2.2 + design decision 2: the shared context's stack limit is
/// disabled on the concurrent path (indirect callees read it through their
/// `FuncDesc`, and one thread's bound is not valid for another), so a deep
/// indirect-call recursion overflows the native stack and must be recovered
/// by the thread's guard page + signal alt-stack and reported as
/// `TrapCode::StackOverflow` — never a process crash, and never
/// misclassified as a memory trap. Two threads recurse concurrently; run 10
/// consecutive times.
#[test]
fn concurrent_indirect_recursion_traps_stack_overflow() {
    let instance = instantiate(
        r#"(module
          (type $t (func (param i32) (result i32)))
          (table 1 funcref)
          (elem (i32.const 0) $rec)
          (func $rec (type $t) (param $n i32) (result i32)
            (if (result i32) (i32.eqz (local.get $n))
              (then (i32.const 0))
              (else
                (call_indirect (type $t)
                  (i32.sub (local.get $n) (i32.const 1))
                  (i32.const 0)))))
          (func (export "deep") (param i32) (result i32)
            (call_indirect (type $t) (local.get 0) (i32.const 0))))"#,
    );

    for run in 0..10 {
        let a = spawn_invoke(instance.clone(), 1, vec![WasmValue::I32(i32::MAX)]);
        let b = spawn_invoke(instance.clone(), 1, vec![WasmValue::I32(i32::MAX)]);
        for (thread, handle) in [("A", a), ("B", b)] {
            let result = handle.join().expect("thread survives the overflow");
            assert!(
                matches!(result, Err(WasmError::Trap(TrapCode::StackOverflow))),
                "run {run}: thread {thread} must trap StackOverflow, got {result:?}"
            );
        }
    }
}

/// Spike finding 3 (per-invocation VmCtx copy contract): mutable globals are
/// SHARED across concurrent invocations.
///
/// `invoke_shared` runs on a copy of the instance context, but the copy is
/// shallow — only `stack_limit` differs; `vmctx.globals` still references
/// the instance's single cell buffer. This test pins that contract: a value
/// written by thread A is observable by thread B through its own
/// per-invocation context copy. If the copy ever snapshotted globals, B
/// would read stale zeros and this fails.
#[test]
fn concurrent_invocations_share_mutable_globals() {
    let instance = instantiate(
        r#"(module
          (global $g (mut i32) (i32.const 0))
          (func (export "write") (param i32) (global.set $g (local.get 0)))
          (func (export "read") (result i32) (global.get $g)))"#,
    );

    for run in 0..10 {
        // A writes a distinct value...
        instance
            .invoke_shared(0, &[WasmValue::I32(0xDEAD_BEEFu32 as i32 + run)])
            .expect("write succeeds");
        // ...and B (a fresh invocation on this thread) must observe it.
        let value = instance.invoke_shared(1, &[]).expect("read succeeds");
        assert_eq!(
            value,
            vec![WasmValue::I32(0xDEAD_BEEFu32 as i32 + run)],
            "run {run}: mutable globals are shared across invocations, not per-invocation"
        );
    }
}

/// Task 3.3: mutable globals are coherent under concurrent access. The AOT
/// path stores mutable globals as lock-free 8-byte cells; aligned scalar
/// accesses never tear, so readers must always observe one whole written
/// value, never a blend.
#[test]
fn concurrent_mutable_global_access_is_correct() {
    let instance = instantiate(
        r#"(module
          (global $g (mut i32) (i32.const 0))
          (func (export "seta") (global.set $g (i32.const 0x11111111)))
          (func (export "setb") (global.set $g (i32.const 0x22222222)))
          (func (export "get") (result i32) (global.get $g)))"#,
    );

    let a = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..5000 {
                instance.invoke_shared(0, &[]).expect("seta succeeds");
            }
        })
    };
    let b = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..5000 {
                instance.invoke_shared(1, &[]).expect("setb succeeds");
            }
        })
    };

    for _ in 0..2000 {
        let value = instance.invoke_shared(2, &[]).expect("get succeeds");
        assert!(
            matches!(
                value.as_slice(),
                [WasmValue::I32(0 | 0x1111_1111 | 0x2222_2222)]
            ),
            "global read {value:?} must be one whole written value (or the initial 0), never a blend"
        );
    }

    a.join().expect("seta thread survives");
    b.join().expect("setb thread survives");

    let value = instance.invoke_shared(2, &[]).expect("final get succeeds");
    assert!(
        matches!(
            value.as_slice(),
            [WasmValue::I32(0x1111_1111 | 0x2222_2222)]
        ),
        "final global {value:?} must be one of the written constants"
    );
}

/// Task 3.1: `memory.grow` synchronised against concurrent loads/stores.
/// Growth is an `mprotect` inside the memory lock; concurrent direct loads
/// either trap (page not yet accessible) or read coherent zeroed data — they
/// never tear or race. A panic, crash, or non-zero read is a finding.
#[test]
fn grow_racing_reads_never_tears() {
    let instance = instantiate(
        r#"(module
          (memory 0 10)
          (func (export "grow") (param i32) (result i32)
            (memory.grow (local.get 0)))
          (func (export "read") (param i32) (result i32)
            (i32.load (local.get 0))))"#,
    );

    let grower = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..300 {
                let result = instance.invoke_shared(0, &[WasmValue::I32(1)]);
                match result {
                    Ok(_) => {}
                    Err(WasmError::Trap(TrapCode::MemoryLimitExceeded)) => {}
                    Err(other) => panic!("unexpected grow error: {other}"),
                }
            }
        })
    };

    let readers: Vec<_> = (0..2)
        .map(|_| {
            let instance = instance.clone();
            std::thread::spawn(move || {
                for i in 0..1000 {
                    let addr = (i % 4) * 65536;
                    match instance.invoke_shared(1, &[WasmValue::I32(addr)]) {
                        Ok(values) => {
                            assert_eq!(
                                values,
                                vec![WasmValue::I32(0)],
                                "grown pages must read coherent zeroed data"
                            );
                        }
                        Err(WasmError::Trap(TrapCode::MemoryOutOfBounds)) => {}
                        Err(other) => panic!("unexpected read error: {other}"),
                    }
                }
            })
        })
        .collect();

    grower.join().expect("grower thread survives");
    for reader in readers {
        reader.join().expect("reader thread survives");
    }
}

/// F5: `InstanceOptions::stack_size` overrides the derived slot size. With a
/// large override the module's own heap stays untouched by stack slots (the
/// slots are carved above it either way), and concurrent invocations still
/// get private stacks.
#[test]
fn instance_options_override_stack_size() {
    use parking_lot::Mutex as ParkingMutex;
    use wasmtiny::{
        aot::{AotStore, InstanceOptions},
        runtime::SharedMemoryRegistry,
    };

    let module = load(
        r#"(module
          (memory 1)
          (global $sp (mut i32) (i32.const 65536))
          (export "__stack_pointer" (global $sp))
          (func (export "run") (result i32)
            (global.set $sp (i32.sub (global.get $sp) (i32.const 16)))
            (global.set $sp (i32.add (global.get $sp) (i32.const 16)))
            (i32.const 1)))"#,
    );
    let shared_store = AotStore::shared();
    let registry = Arc::new(ParkingMutex::new(SharedMemoryRegistry::default()));
    let instance = Arc::new(
        AotInstance::instantiate_with_registry_and_options(
            &shared_store,
            &module,
            &[],
            registry,
            InstanceOptions {
                stack_size: Some(2 * 1024 * 1024),
            },
        )
        .expect("instantiation succeeds"),
    );

    // The override must not break concurrent invocations; each returns 1.
    let a = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..500 {
                assert_eq!(
                    instance.invoke_shared(0, &[]).expect("run succeeds"),
                    vec![WasmValue::I32(1)]
                );
            }
        })
    };
    let b = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..500 {
                assert_eq!(
                    instance.invoke_shared(0, &[]).expect("run succeeds"),
                    vec![WasmValue::I32(1)]
                );
            }
        })
    };
    a.join().expect("thread A survives");
    b.join().expect("thread B survives");
}

fn instantiate(source: &str) -> Arc<AotInstance> {
    Arc::new(AotInstance::new(&load(source)).expect("instantiation succeeds"))
}

fn load(source: &str) -> AotModule {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

/// Spike finding 1 (multi-waiter wait/notify): the 2-worker hang repro.
///
/// A worker pool parks N workers on ONE shared wake word and a dispatcher
/// calls `notify(N)`. The pre-queue registry held a single flag per address,
/// so `notify(N)` could never wake more than one distinct waiter — the
/// other N-1 workers slept until their timeout (the observed hang). With the
/// per-address waiter queue, `notify(N)` releases up to N distinct waiters.
///
/// Runs 10 times with 2 and 4 workers; every worker must report woken (0),
/// never timed out (2). Synchronisation is counter-based (a guest parked
/// counter, drained by repeated notifies) rather than a fixed sleep: a
/// worker that bumped the counter but had not yet registered when a notify
/// fired simply parks and the next drain notify releases it, so the test is
/// deterministic. Without the queue fix the surplus workers return 2 after
/// the 2 s timeout backstop and the join assertion fails.
#[test]
fn many_workers_on_one_wake_word_all_wake() {
    let source = r#"(module
      (memory 1)
      (func (export "park") (result i32)
        (local $r i32)
        ;; Atomic parked counter at address 4 (address 0 is the wake word).
        (i32.atomic.rmw.add (i32.const 4) (i32.const 1))
        (drop)
        (memory.atomic.wait32 (i32.const 0) (i32.const 0) (i64.const 2000000000))
        (local.set $r)
        (i32.atomic.rmw.add (i32.const 4) (i32.const -1))
        (drop)
        (local.get $r))
      (func (export "parked_count") (result i32)
        (i32.atomic.load (i32.const 4)))
      (func (export "wake") (param i32) (result i32)
        (memory.atomic.notify (i32.const 0) (local.get 0))))"#;

    for run in 0..10 {
        for workers in [2usize, 4] {
            let instance = instantiate(source);
            let handles: Vec<_> = (0..workers)
                .map(|_| spawn_invoke(instance.clone(), 0, vec![]))
                .collect();

            // Wait until every worker has entered `park`.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while instance
                .invoke_shared(1, &[])
                .expect("parked_count succeeds")[0]
                .i32()
                .unwrap()
                < workers as i32
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "run {run}/{workers} workers: never all parked"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            // Drain: notify until no worker remains parked. A worker whose
            // counter bump preceded its registration is released by a later
            // drain iteration; the joins assert every worker was woken.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while instance
                .invoke_shared(1, &[])
                .expect("parked_count succeeds")[0]
                .i32()
                .unwrap()
                > 0
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "run {run}/{workers} workers: never all woken"
                );
                instance
                    .invoke_shared(2, &[WasmValue::I32(workers as i32)])
                    .expect("notify succeeds");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            for handle in handles {
                let result = handle.join().expect("worker thread survives");
                assert_eq!(
                    result,
                    Ok(vec![WasmValue::I32(0)]),
                    "run {run}/{workers} workers: every parked worker must be woken, none may time out"
                );
            }
        }
    }
}

/// Task 1.2: `memory.atomic.wait` parks a waiter without holding the memory
/// or instance lock. While one invocation is parked, another thread must be
/// able to invoke the same instance, and a notifier on another thread must
/// wake the parked waiter.
#[test]
fn parked_waiter_does_not_block_other_invocations() {
    let instance = instantiate(
        r#"(module
          (memory 1)
          (func (export "wait") (result i32)
            (memory.atomic.wait32 (i32.const 0) (i32.const 0) (i64.const 5000000000)))
          (func (export "notify") (result i32)
            (memory.atomic.notify (i32.const 0) (i32.const 1)))
          (func (export "spin") (param i32) (result i32)
            (local $i i32)
            (local.set $i (i32.const 0))
            (block $done
              (loop $again
                (br_if $done (i32.ge_s (local.get $i) (local.get 0)))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $again)))
            (local.get $i)))"#,
    );

    // Thread A parks in `memory.atomic.wait32` (memory[0] == 0 matches; the
    // 5s timeout is a backstop so a regression fails cleanly instead of
    // hanging the suite).
    let a = spawn_invoke(instance.clone(), 0, vec![]);
    std::thread::sleep(std::time::Duration::from_millis(100));

    // While A is parked, another invocation of the same instance must still
    // run to completion — no instance-wide lock is held by the waiter.
    let spin = instance
        .invoke_shared(2, &[WasmValue::I32(100_000)])
        .expect("spin runs while another invocation is parked");
    assert_eq!(spin, vec![WasmValue::I32(100_000)]);

    // Notify from another thread wakes the parked waiter. `notify` takes the
    // memory lock, so it completing (and A waking) proves the park does not
    // hold the memory lock.
    let notified = instance.invoke_shared(1, &[]).expect("notify succeeds");
    assert_eq!(notified, vec![WasmValue::I32(1)], "one waiter woken");

    let wait_result = a
        .join()
        .expect("waiter thread survives")
        .expect("wait returns");
    assert_eq!(
        wait_result,
        vec![WasmValue::I32(0)],
        "waiter must be woken by the notifier, not time out"
    );
}

/// A module that exports `__stack_pointer` (the wasm-threads shadow-stack
/// convention) must get a private shadow stack per concurrent invocation: two
/// threads carving frames and writing a tag through `$sp` must never clobber
/// each other. Without per-invocation stacks both invocations compute the same
/// `$sp`-relative slot, so one thread reads back the other's tag and the check
/// fails.
#[test]
fn shadow_stack_is_per_invocation() {
    let instance = instantiate(
        r#"(module
          (memory 1)
          (global $sp (mut i32) (i32.const 65536))
          (export "__stack_pointer" (global $sp))
          (func (export "run") (param $tag i32) (result i32)
            (local $k i32)
            (local $slot i32)
            ;; Carve 16 bytes of shadow stack and write the tag.
            (global.set $sp (i32.sub (global.get $sp) (i32.const 16)))
            (local.set $slot (global.get $sp))
            (i32.store (local.get $slot) (local.get $tag))
            ;; Spin to widen the overlap window.
            (block $done
              (loop $l
                (br_if $done (i32.ge_u (local.get $k) (i32.const 50000)))
                (local.set $k (i32.add (local.get $k) (i32.const 1)))
                (br $l)))
            ;; Read the tag back and restore the stack pointer.
            (local.set $k (i32.load (local.get $slot)))
            (global.set $sp (i32.add (global.get $sp) (i32.const 16)))
            (i32.eq (local.get $k) (local.get $tag))))"#,
    );

    let a = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..2000 {
                let value = instance
                    .invoke_shared(0, &[WasmValue::I32(0x1111_1111)])
                    .expect("run succeeds");
                assert_eq!(
                    value,
                    vec![WasmValue::I32(1)],
                    "thread A read back a clobbered stack slot"
                );
            }
        })
    };
    let b = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..2000 {
                let value = instance
                    .invoke_shared(0, &[WasmValue::I32(0x2222_2222)])
                    .expect("run succeeds");
                assert_eq!(
                    value,
                    vec![WasmValue::I32(1)],
                    "thread B read back a clobbered stack slot"
                );
            }
        })
    };
    a.join().expect("thread A survives");
    b.join().expect("thread B survives");
}

/// F5: each shadow-stack slot carries a PROT_NONE guard page at its bottom,
/// so a stack overflow traps `MemoryOutOfBounds` instead of silently
/// writing into the guest heap below the slot.
#[test]
fn shadow_stack_overflow_traps_not_corrupts() {
    let instance = instantiate(
        r#"(module
          (memory 1)
          (global $sp (mut i32) (i32.const 65536))
          (export "__stack_pointer" (global $sp))
          (func (export "blow") (result i32)
            ;; Carve 70 KiB — past the 64 KiB usable slot into the guard.
            (global.set $sp (i32.sub (global.get $sp) (i32.const 71680)))
            (i32.store (global.get $sp) (i32.const 1))
            (i32.const 0)))"#,
    );

    let result = instance.invoke_shared(0, &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "a shadow-stack overflow past the slot must trap MemoryOutOfBounds, got {result:?}"
    );
}

fn spawn_invoke(
    instance: Arc<AotInstance>,
    func: u32,
    args: Vec<WasmValue>,
) -> std::thread::JoinHandle<Result<Vec<WasmValue>, WasmError>> {
    std::thread::spawn(move || instance.invoke_shared(func, &args))
}

/// Spike finding 2 (trap backtrace): after a guest trap, the faulting
/// function and code offset are recoverable for diagnostics.
///
/// The signal handler classifies the PC to a `TrapCode`; `last_trap_site`
/// maps that PC back to the wasm function index and byte offset so an
/// embedder can symbolise the fault (mt-demo OOB root-causing).
#[test]
fn trap_site_diagnostics_report_function_and_offset() {
    let source = r#"(module
          (memory 1)
          (func (export "bounce") (param i32) (result i32)
            (i32.load (local.get 0)))
          (func (export "trap") (result i32)
            (call 0 (i32.const 0xFFFF_FF00))))"#;
    let instance = instantiate(source);

    // A successful invocation clears the record.
    let _ = instance
        .invoke_shared(0, &[WasmValue::I32(0)])
        .expect("valid load succeeds");
    assert!(
        instance.last_trap_site().is_none(),
        "a successful invocation must clear the last-trap record"
    );

    let result = instance.invoke_shared(1, &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "expected OOB trap, got {result:?}"
    );
    let (func_index, offset, code) = instance
        .last_trap_site()
        .expect("trap site must be recorded");
    assert_eq!(code, TrapCode::MemoryOutOfBounds);
    // The faulting PC is inside "bounce" (module-local index 0), the callee
    // whose load traps; the exported "trap" wrapper (index 1) called it.
    assert_eq!(
        func_index, 0,
        "trap must map to the function containing the faulting instruction"
    );
    // The offset is inside that function's code, which is non-empty.
    let function = &load(source).functions[func_index as usize];
    assert!(
        offset < function.code_len,
        "offset {offset} must be within function {} ({} bytes)",
        func_index,
        function.code_len
    );
}

/// Task 1.1 + 2.1 + 2.2: two host threads enter one AOT instance over shared
/// linear memory, exercising memory writes/reads, a mutable global, a
/// `call_indirect` through the shared table, and an independent locals +
/// control-flow loop. Run 10 consecutive times.
#[test]
fn two_threads_enter_one_instance_over_shared_memory() {
    let source = r#"(module
      (memory 1)
      (global $g (mut i32) (i32.const 0))
      (type $t (func (param i32) (result i32)))
      (func $inc (type $t) (param i32) (result i32)
        (i32.add (local.get 0) (i32.const 1)))
      (table 1 funcref)
      (elem (i32.const 0) $inc)
      (func (export "worker") (param $n i32) (param $slot i32) (result i32)
        (local $acc i32)
        (local $i i32)
        (i32.store (local.get $slot) (local.get $n))
        (global.set $g (i32.add (global.get $g) (i32.const 1)))
        (local.set $acc (call_indirect (type $t) (local.get $n) (i32.const 0)))
        (local.set $i (i32.const 0))
        (block $done
          (loop $again
            (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
            (local.set $acc (i32.add (local.get $acc) (i32.const 3)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (local.get $acc))
      (func (export "read") (param $slot i32) (result i32)
        (i32.load (local.get $slot)))
      (func (export "global") (result i32)
        (global.get $g)))"#;

    for run in 0..10 {
        let instance = instantiate(source);

        let a = spawn_invoke(
            instance.clone(),
            1,
            vec![WasmValue::I32(1000), WasmValue::I32(0)],
        );
        let b = spawn_invoke(
            instance.clone(),
            1,
            vec![WasmValue::I32(500), WasmValue::I32(4)],
        );

        let a_result = a
            .join()
            .expect("thread A survives")
            .expect("A invocation succeeds");
        let b_result = b
            .join()
            .expect("thread B survives")
            .expect("B invocation succeeds");

        // worker(n, slot): acc = inc(n) + 3*n = 4n + 1.
        assert_eq!(
            a_result,
            vec![WasmValue::I32(4001)],
            "run {run}: thread A result"
        );
        assert_eq!(
            b_result,
            vec![WasmValue::I32(2001)],
            "run {run}: thread B result"
        );

        // Shared linear memory: each thread's write is observable.
        let read0 = instance
            .invoke_shared(2, &[WasmValue::I32(0)])
            .expect("read slot 0");
        let read4 = instance
            .invoke_shared(2, &[WasmValue::I32(4)])
            .expect("read slot 4");
        assert_eq!(read0, vec![WasmValue::I32(1000)], "run {run}: memory[0]");
        assert_eq!(read4, vec![WasmValue::I32(500)], "run {run}: memory[4]");

        // The mutable global is touched by both threads; the non-atomic
        // increment may lose one update, but must never tear.
        let global = instance.invoke_shared(3, &[]).expect("read global");
        assert!(
            matches!(global.as_slice(), [WasmValue::I32(1 | 2)]),
            "run {run}: global value {global:?} must be one whole increment (1 or 2)"
        );
    }
}

/// F5: a shadow-stack pointer that is **not** exported under the
/// `__stack_pointer` name can be declared through the compiler config. The
/// compiler routes its `global.get`/`set` through the vmctx stack-pointer
/// field and records the index in the artifact, so the runtime still gives
/// each concurrent invocation a private stack.
#[test]
fn unexported_shadow_stack_pointer_via_compiler_config() {
    // `$sp` is a plain mutable global here — never exported.
    let source = r#"(module
          (memory 1)
          (global $sp (mut i32) (i32.const 65536))
          (func (export "run") (param $tag i32) (result i32)
            (local $k i32)
            (local $slot i32)
            (global.set $sp (i32.sub (global.get $sp) (i32.const 16)))
            (local.set $slot (global.get $sp))
            (i32.store (local.get $slot) (local.get $tag))
            (block $done
              (loop $l
                (br_if $done (i32.ge_u (local.get $k) (i32.const 50000)))
                (local.set $k (i32.add (local.get $k) (i32.const 1)))
                (br $l)))
            (local.set $k (i32.load (local.get $slot)))
            (global.set $sp (i32.add (global.get $sp) (i32.const 16)))
            (i32.eq (local.get $k) (local.get $tag))))"#;

    let wasm = wat::parse_str(source).expect("wat parses");
    let mut config = CompilerConfig::host();
    config.shadow_stack_global = Some(0); // global 0 is `$sp`
    let bytes = compile_artifact(&wasm, &config).expect("compilation succeeds");
    let module = AotLoader::new().load(&bytes).expect("artifact loads");
    assert_eq!(
        module.stack_pointer_global,
        Some(0),
        "artifact must record the compiler-config shadow-stack pointer"
    );

    let instance = Arc::new(AotInstance::new(&module).expect("instantiation succeeds"));
    let a = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..2000 {
                let value = instance
                    .invoke_shared(0, &[WasmValue::I32(0x1111_1111)])
                    .expect("run succeeds");
                assert_eq!(
                    value,
                    vec![WasmValue::I32(1)],
                    "thread A read back a clobbered stack slot"
                );
            }
        })
    };
    let b = {
        let instance = instance.clone();
        std::thread::spawn(move || {
            for _ in 0..2000 {
                let value = instance
                    .invoke_shared(0, &[WasmValue::I32(0x2222_2222)])
                    .expect("run succeeds");
                assert_eq!(
                    value,
                    vec![WasmValue::I32(1)],
                    "thread B read back a clobbered stack slot"
                );
            }
        })
    };
    a.join().expect("thread A survives");
    b.join().expect("thread B survives");
}

/// Spike finding 2 (OOB/hang root cause, fixed): `memory.atomic.wait32` /
/// `notify` **memarg offsets** must be folded into the effective address.
///
/// The AOT compiler originally dropped the memarg offset for the wait/notify
/// libcalls (loads/stores folded it via `memory_addr`), so every wait and
/// notify landed on address 0. Real Rust atomics modules (std futexes) use
/// nonzero offsets for their wake words, which routed the guest worker pool
/// and condvar traffic to the wrong address: notify at the intended word
/// never reached the parked waiter (the 2-worker hang), and value compares
/// read the wrong word. This test pins the fix: a waiter parked at effective
/// address 64 must NOT be woken by a notify at address 0, and MUST be woken
/// by a notify at address 64.
#[test]
fn wait_notify_memarg_offsets_are_respected() {
    let source = r#"(module
      (memory 1)
      (func (export "wait") (result i32)
        ;; Atomic parked counter at address 4; the wait word is at 64.
        (i32.atomic.rmw.add (i32.const 4) (i32.const 1))
        (drop)
        (memory.atomic.wait32 offset=64 (i32.const 0) (i32.const 0) (i64.const 2000000000)))
      (func (export "parked_count") (result i32)
        (i32.atomic.load (i32.const 4)))
      (func (export "notify64") (result i32)
        (memory.atomic.notify offset=64 (i32.const 0) (i32.const 1)))
      (func (export "notify0") (result i32)
        (memory.atomic.notify offset=0 (i32.const 0) (i32.const 1))))"#;

    for run in 0..5 {
        let instance = instantiate(source);
        let waiter = spawn_invoke(instance.clone(), 0, vec![]);

        // Counter-based readiness: wait until the worker has entered `wait`,
        // then a short grace period for it to reach the engine's waiter
        // registration. The window between the counter bump and the wait32
        // libcall's register is microseconds; 50 ms is an enormous margin
        // and removes the thread-spawn latency that made a bare sleep flaky.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while instance
            .invoke_shared(1, &[])
            .expect("parked_count succeeds")[0]
            .i32()
            .expect("i32")
            != 1
        {
            assert!(
                std::time::Instant::now() < deadline,
                "run {run}: worker never entered wait"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        // A notify on address 0 must find nobody (the waiter is at 64).
        let notified = instance.invoke_shared(3, &[]).expect("notify0 succeeds");
        assert_eq!(
            notified,
            vec![WasmValue::I32(0)],
            "run {run}: notify at address 0 must not reach the waiter parked at 64"
        );

        // The waiter must still be parked: notify at address 64 wakes it.
        let notified = instance.invoke_shared(2, &[]).expect("notify64 succeeds");
        assert_eq!(
            notified,
            vec![WasmValue::I32(1)],
            "run {run}: notify at address 64 must reach the waiter"
        );
        let result = waiter.join().expect("waiter thread survives");
        assert_eq!(
            result,
            Ok(vec![WasmValue::I32(0)]),
            "run {run}: waiter parked at 64 must be woken by the address-64 notify"
        );
    }
}
