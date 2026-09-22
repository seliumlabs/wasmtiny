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

fn load(source: &str) -> AotModule {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

fn instantiate(source: &str) -> Arc<AotInstance> {
    Arc::new(AotInstance::new(&load(source)).expect("instantiation succeeds"))
}

fn spawn_invoke(
    instance: Arc<AotInstance>,
    func: u32,
    args: Vec<WasmValue>,
) -> std::thread::JoinHandle<Result<Vec<WasmValue>, WasmError>> {
    std::thread::spawn(move || instance.invoke_shared(func, &args))
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
