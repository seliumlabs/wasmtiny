//! AOT fuel metering: size-weighted charges at function entry and loop
//! back-edges, the execution-budget trap, host-call exclusion, monotonicity,
//! concurrency safety, and the per-invocation fuel cell that keeps concurrent
//! invocations off one shared cache line.

use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use wasmtiny::{
    aot::{AotExtern, AotInstance, AotLoader, AotStore},
    runtime::{
        FunctionType, HostCaller, HostFunc, NumType, Result, TrapCode, ValType, WasmError,
        WasmValue,
    },
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// A function whose body is `i32.const; end` — a static size of 2, so one
/// invocation charges exactly 2 units (the entry charge).
const CONST_FN: &str = "(module (func (export \"f\") (result i32) (i32.const 42)))";
const ENTRY_CHARGE: u64 = 12;
/// A guest function that calls a host import and then spins a long loop, so the
/// host call happens *before* the loop: whatever the host does to the budget
/// must therefore be visible on the loop's charges.
const HOST_THEN_LOOP_FN: &str = "(module
    (import \"env\" \"hook\" (func $hook))
    (func (export \"run\") (param i32) (result i32) (local $i i32)
      (call $hook)
      (loop $l
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br_if $l (i32.lt_s (local.get $i) (local.get 0))))
      (local.get $i)))";
const LOOP_BODY_CHARGE: u64 = 8;
/// A function with one loop. Static sizing (operator counts):
///
/// - function total: 12 (`loop`, 8 body operators, `end`, `local.get`, `end`)
/// - loop body: 8 operators (between `loop` and its `end`)
///
/// With parameter `n`, the loop header executes `n + 1` times, so one
/// invocation charges `12 + (n + 1) * 8`.
const LOOP_FN: &str = "(module (func (export \"run\") (param i32) (result i32) (local $i i32)
    (loop $l
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_s (local.get $i) (local.get 0))))
    (local.get $i)))";

/// A host function that does a lot of Rust work. None of it must be charged
/// to the instance's meter.
struct BusyHost;

/// Lifts the instance's execution budget when the guest calls it. The instance
/// handle is published after instantiation (the host function must exist
/// before the instance that imports it).
struct RaiseBudget(Arc<OnceLock<Arc<AotInstance>>>);

/// Caps the instance's budget to just above what it has already executed, so
/// the rest of the invocation has almost no allowance left.
struct CapBudget(Arc<OnceLock<Arc<AotInstance>>>);

impl HostFunc for BusyHost {
    fn call(&self, _caller: &mut HostCaller<'_>, _args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let mut acc: u64 = 0;
        for i in 0..1_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        Ok(vec![WasmValue::I32(acc as i32)])
    }

    fn function_type(&self) -> Option<&FunctionType> {
        use std::sync::OnceLock;
        static TYPE: OnceLock<FunctionType> = OnceLock::new();
        Some(TYPE.get_or_init(|| FunctionType::new(Vec::new(), vec![ValType::Num(NumType::I32)])))
    }
}

impl HostFunc for RaiseBudget {
    fn call(&self, _caller: &mut HostCaller<'_>, _args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        if let Some(instance) = self.0.get() {
            instance.set_execution_budget(None).expect("raise budget");
        }
        Ok(Vec::new())
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(nullary_type())
    }
}

impl HostFunc for CapBudget {
    fn call(&self, _caller: &mut HostCaller<'_>, _args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        if let Some(instance) = self.0.get() {
            let executed = executed(instance);
            instance
                .set_execution_budget(Some(executed + 10))
                .expect("cap budget");
        }
        Ok(Vec::new())
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(nullary_type())
    }
}

#[test]
fn a_lowered_budget_takes_effect_at_the_next_host_call() {
    let handle = Arc::new(OnceLock::new());
    let module = load(HOST_THEN_LOOP_FN);
    let imports = [nullary_import("env", Arc::new(CapBudget(handle.clone())))];
    let instance = Arc::new(
        AotInstance::instantiate(&AotStore::shared(), &module, &imports).expect("instantiate"),
    );
    let _ = handle.set(instance.clone());

    // The initial budget is generous, but the guest caps it at the top of the
    // invocation to just past the entry charge. The loop must then trip the
    // budget almost immediately — not after burning the original allowance.
    instance
        .set_execution_budget(Some(10_000))
        .expect("set budget");
    let error = instance
        .invoke_shared(1, &[WasmValue::I32(1_000_000)])
        .expect_err("the capped budget must trap");
    assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
    assert!(
        executed(&instance) < 100,
        "the lowered budget was honoured at the host boundary, not the original 10_000"
    );
}

#[test]
fn a_raised_budget_takes_effect_at_the_next_host_call() {
    let handle = Arc::new(OnceLock::new());
    let module = load(HOST_THEN_LOOP_FN);
    let imports = [nullary_import("env", Arc::new(RaiseBudget(handle.clone())))];
    let instance = Arc::new(
        AotInstance::instantiate(&AotStore::shared(), &module, &imports).expect("instantiate"),
    );
    let _ = handle.set(instance.clone());

    // The allowance alone (100) cannot cover a million loop iterations
    // (8 units each). The host call at the top of the function lifts the
    // ceiling, and the flush at that boundary must make the new headroom
    // visible to the charges that follow.
    instance
        .set_execution_budget(Some(100))
        .expect("set budget");
    let result = instance
        .invoke_shared(1, &[WasmValue::I32(1_000_000)])
        .expect("the raised budget lets the loop finish");
    assert_eq!(result, vec![WasmValue::I32(1_000_000)]);
    assert!(
        executed(&instance) > 100,
        "the whole run was charged, well past the entry-time allowance"
    );
}

#[test]
fn an_indirect_callee_is_charged_to_its_own_instance() {
    // `call_indirect` loads the callee's context from its `FuncDesc`, which is
    // the *shared* instance context — not the invocation's per-invocation copy.
    // The callee's fuel must therefore still be attributed to (and counted by)
    // the instance that owns it, and must not be lost: `run`'s own entry charge
    // is drained from the invocation-local cell, the callee's is charged to the
    // shared cells directly, and the counter sums both.
    let mut instance = instantiate(
        "(module
           (type $t (func (result i32)))
           (table 1 funcref)
           (elem (i32.const 0) $callee)
           (func $callee (result i32) (i32.const 7))
           (func (export \"run\") (result i32)
             (call_indirect (type $t) (i32.const 0))))",
    );

    // Function indices: `callee` is 0, `run` is 1.
    let result = instance.invoke(1, &[]).expect("invoke succeeds");
    assert_eq!(result, vec![WasmValue::I32(7)]);
    assert_eq!(
        executed(&instance),
        5,
        "run's 3-operator entry charge plus callee's 2-operator entry charge"
    );
}

#[test]
fn aot_budget_exhaustion_in_a_loop_traps_distinctly() {
    let mut instance = instantiate(LOOP_FN);
    // Budget is enough for the entry charge but not for many loop iterations.
    instance.set_execution_budget(Some(20)).expect("set budget");
    let error = instance
        .invoke(0, &[WasmValue::I32(1_000_000)])
        .expect_err("the loop must exhaust the budget");
    assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
}

#[test]
fn aot_execution_charges_the_counter() {
    let mut instance = instantiate(CONST_FN);
    assert_eq!(executed(&instance), 0, "nothing charged before invocation");

    let result = instance.invoke(0, &[]).expect("invoke succeeds");
    assert_eq!(result, vec![WasmValue::I32(42)]);
    assert!(
        executed(&instance) > 0,
        "an AOT invocation must charge the instance meter"
    );
    // The const function's static size is 2, charged once at entry.
    assert_eq!(executed(&instance), 2);
}

#[test]
fn charges_land_when_an_invocation_traps() {
    // The loop charges five times, then traps. A trap ends the invocation
    // through the same path as a return, so the invocation's accumulated fuel
    // must still reach the authoritative counter.
    let mut instance = instantiate(
        "(module (func (export \"run\") (local $i i32)
           (loop $l
             (local.set $i (i32.add (local.get $i) (i32.const 1)))
             (br_if $l (i32.lt_s (local.get $i) (i32.const 5)))
             (unreachable))))",
    );

    let error = instance.invoke(0, &[]).expect_err("the guest traps");
    assert_eq!(error, WasmError::Trap(TrapCode::Unreachable));
    assert!(
        executed(&instance) > 0,
        "the charges made before the trap must be committed"
    );
}

fn compile(source: &str) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds")
}

#[test]
fn concurrent_invocations_charge_safely() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 500;

    let instance = Arc::new(instantiate(CONST_FN));
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let instance = instance.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..PER_THREAD {
                instance
                    .invoke_shared(0, &[])
                    .expect("shared invocation succeeds");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("thread joins");
    }

    // Every invocation charged exactly 2 units; the per-invocation cells must
    // all be drained into the authoritative counter with no lost updates.
    assert_eq!(executed(&instance), 2 * THREADS * PER_THREAD);
}

#[test]
fn configured_budget_traps_with_the_budget_trap_code() {
    let mut instance = instantiate(CONST_FN);
    // The entry charge alone (2) exceeds a budget of 1.
    instance.set_execution_budget(Some(1)).expect("set budget");

    let error = instance
        .invoke(0, &[])
        .expect_err("budget exhaustion must trap");
    assert_eq!(error, WasmError::Trap(TrapCode::ExecutionBudgetExceeded));
}

#[test]
fn counter_is_monotonic_across_invocations() {
    let mut instance = instantiate(LOOP_FN);
    let mut previous = executed(&instance);
    for n in [0, 1, 2, 4, 8] {
        instance.invoke(0, &[WasmValue::I32(n)]).expect("invoke");
        let current = executed(&instance);
        assert!(current > previous, "count must strictly increase");
        previous = current;
    }
}

#[test]
fn entry_charge_records_a_budget_trap_site() {
    use wasmtiny_aotc::environment::{USER_TRAP_BUDGET, user_trap};

    let wasm = wat::parse_str(CONST_FN).expect("wat parses");
    let compiled = wasmtiny_aotc::compile_module(&wasm, &CompilerConfig::host())
        .expect("compilation succeeds");

    // The emitted charge is an ordinary `trapnz` with the budget trap code, so
    // it must appear in the function's recorded trap table (the runtime
    // classifies a fault by that table).
    let budget_trap = user_trap(USER_TRAP_BUDGET);
    assert!(
        compiled
            .functions
            .iter()
            .any(|f| f.traps.iter().any(|(_, code)| *code == budget_trap)),
        "the inline fuel charge must be a recorded trap site"
    );
}

fn executed(instance: &AotInstance) -> u64 {
    instance
        .stats()
        .expect("stats succeed")
        .executed_instructions
}

#[test]
fn fuel_is_size_weighted_at_entry_and_loop_back_edges() {
    // n = 0: the guard fails after one pass, so the header runs once.
    let mut zero = instantiate(LOOP_FN);
    zero.invoke(0, &[WasmValue::I32(0)]).expect("invoke");
    assert_eq!(
        executed(&zero),
        ENTRY_CHARGE + LOOP_BODY_CHARGE,
        "entry plus a single loop-header charge"
    );

    // For n >= 1 the header runs exactly n times, so a fresh instance
    // isolates one invocation's `entry + n * body` charge.
    let mut five = instantiate(LOOP_FN);
    five.invoke(0, &[WasmValue::I32(5)]).expect("invoke");
    assert_eq!(executed(&five), ENTRY_CHARGE + 5 * LOOP_BODY_CHARGE);

    let mut ten = instantiate(LOOP_FN);
    ten.invoke(0, &[WasmValue::I32(10)]).expect("invoke");
    assert_eq!(executed(&ten), ENTRY_CHARGE + 10 * LOOP_BODY_CHARGE);

    // Each extra iteration charges exactly one loop-body count.
    assert_eq!(
        executed(&ten) - executed(&five),
        5 * LOOP_BODY_CHARGE,
        "the charge is linear in the iteration count"
    );
}

#[test]
fn host_function_calls_are_not_charged() {
    let module = load(
        "(module
           (import \"env\" \"f\" (func $f (result i32)))
           (func (export \"run\") (result i32) (call $f)))",
    );
    let imports = [(
        "env".to_string(),
        "f".to_string(),
        AotExtern::HostFunc(Arc::new(BusyHost)),
    )];
    let mut instance =
        AotInstance::instantiate(&AotStore::shared(), &module, &imports).expect("instantiate");

    // `run` is function index 1 (the import occupies index 0).
    instance.invoke(1, &[]).expect("invoke succeeds");
    // `run`'s body is `call $f; end` — a static size of 2, charged once at
    // entry. The host's million-iteration Rust loop adds nothing.
    assert_eq!(
        executed(&instance),
        2,
        "only the guest's own entry charge is counted"
    );
}

fn instantiate(source: &str) -> AotInstance {
    AotInstance::new(&load(source)).expect("instantiation succeeds")
}

fn load(source: &str) -> wasmtiny::aot::AotModule {
    AotLoader::new()
        .load(&compile(source))
        .expect("artifact loads")
}

/// A host import with a `() -> ()` signature.
fn nullary_import(name: &str, func: Arc<dyn HostFunc>) -> (String, String, AotExtern) {
    (
        name.to_string(),
        "hook".to_string(),
        AotExtern::HostFunc(func),
    )
}

fn nullary_type() -> &'static FunctionType {
    static TYPE: OnceLock<FunctionType> = OnceLock::new();
    TYPE.get_or_init(|| FunctionType::new(Vec::new(), Vec::new()))
}

#[test]
fn reset_budget_is_honoured_on_the_next_invocation() {
    let mut instance = instantiate(CONST_FN);
    instance.set_execution_budget(Some(1)).expect("set budget");
    assert!(
        instance.invoke(0, &[]).is_err(),
        "the tight budget traps on the first invocation"
    );

    // Resetting to unbounded lifts the ceiling: the next invocation runs.
    instance.set_execution_budget(None).expect("reset budget");
    assert_eq!(
        instance
            .invoke(0, &[])
            .expect("invoke succeeds after reset"),
        vec![WasmValue::I32(42)]
    );

    // A larger finite budget is also honoured.
    instance
        .set_execution_budget(Some(1_000))
        .expect("set budget");
    assert!(instance.invoke(0, &[]).is_ok());
}

#[test]
fn the_counter_is_monotonic_while_concurrent_invocations_run() {
    const THREADS: u64 = 3;
    const PER_THREAD: u64 = 2_000;
    const ITERATIONS: i32 = 64;

    let instance = Arc::new(instantiate(LOOP_FN));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let instance = instance.clone();
            std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    instance
                        .invoke_shared(0, &[WasmValue::I32(ITERATIONS)])
                        .expect("shared invocation succeeds");
                }
            })
        })
        .collect();

    // Sample while the workers run: the authoritative counter only ever grows
    // (invocations commit into it, never rewind it).
    let mut previous = executed(&instance);
    while workers.iter().any(|worker| !worker.is_finished()) {
        let current = executed(&instance);
        assert!(
            current >= previous,
            "the counter must not decrease while invocations are in flight"
        );
        previous = current;
    }
    for worker in workers {
        worker.join().expect("thread joins");
    }

    assert!(executed(&instance) > THREADS * PER_THREAD);
}

/// The wall-time regression that motivated the per-invocation fuel cell: with
/// the charge aimed at one shared cell, two workers ping-ponged that cache line
/// once per loop iteration and ran ~1.7x *slower* than serial.
#[test]
fn two_workers_beat_serial_wall_time_on_a_hot_loop() {
    // A single-CPU environment cannot show parallelism; don't fail there.
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) < 2 {
        return;
    }

    const ITERATIONS: i32 = 400_000;
    const TASKS: usize = 2;
    const ROUNDS: usize = 3;

    let instance = instantiate(LOOP_FN);
    let task = || {
        instance
            .invoke_shared(0, &[WasmValue::I32(ITERATIONS)])
            .expect("shared invocation succeeds");
    };

    // Warm up the code path so the first measured round is not paying for
    // page faults and branch-misprediction training.
    for _ in 0..TASKS {
        task();
    }

    let mut serial = Duration::MAX;
    let mut parallel = Duration::MAX;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..TASKS {
            task();
        }
        serial = serial.min(start.elapsed());

        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..TASKS {
                scope.spawn(task);
            }
        });
        parallel = parallel.min(start.elapsed());
    }

    assert!(
        parallel < serial,
        "two workers must beat serial wall time on a hot loop \
         (serial {serial:?} vs parallel {parallel:?}); \
         a parallel time at or above serial indicates charge contention"
    );
}
