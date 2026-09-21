//! Typed trap recovery: out-of-bounds, unreachable, stack overflow, host
//! errors, and indirect-call traps all map to typed `TrapCode`s without
//! crashing the host.

use std::sync::{Arc, OnceLock};

use wasmtiny::{
    aot::AotLoader,
    runtime::{
        FunctionType, HostCaller, HostFunc, Result, TrapCode, ValType, WasmError, WasmValue,
    },
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

struct FailingHost;

impl HostFunc for FailingHost {
    fn call(&self, _caller: &mut HostCaller<'_>, _args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        Err(WasmError::Trap(TrapCode::HostTrap))
    }

    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: OnceLock<FunctionType> = OnceLock::new();
        Some(TYPE.get_or_init(|| {
            FunctionType::new(
                Vec::new(),
                vec![ValType::Num(wasmtiny::runtime::NumType::I32)],
            )
        }))
    }
}

#[test]
fn call_indirect_null_traps() {
    let code = trap(
        "(module
           (type $t (func (param i32) (result i32)))
           (table 2 funcref)
           (func (export \"call\") (param i32) (result i32)
             (call_indirect (type $t) (local.get 0) (i32.const 0))))",
        0,
        &[WasmValue::I32(3)],
    );
    assert_eq!(code, TrapCode::CallIndirectNull);
}

#[test]
fn call_indirect_type_mismatch_traps() {
    let code = trap(
        "(module
           (type $ii (func (param i32) (result i32)))
           (type $ff (func (param f32) (result f32)))
           (func $f (type $ff) (param f32) (result f32) (local.get 0))
           (table 1 funcref)
           (elem (i32.const 0) $f)
           (func (export \"call\") (param i32) (result i32)
             (call_indirect (type $ii) (local.get 0) (i32.const 0))))",
        1,
        &[WasmValue::I32(3)],
    );
    assert_eq!(code, TrapCode::IndirectCallTypeMismatch);
}

#[test]
fn host_error_propagates_as_a_trap() {
    let wasm = wat::parse_str(
        "(module
           (import \"env\" \"f\" (func $f (result i32)))
           (func (export \"main\") (result i32) (call $f)))",
    )
    .unwrap();
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).unwrap();
    let module = AotLoader::new().load(&bytes).unwrap();
    let imports = [(
        "env".to_string(),
        "f".to_string(),
        wasmtiny::aot::AotExtern::HostFunc(Arc::new(FailingHost)),
    )];
    let mut instance = wasmtiny::aot::AotInstance::instantiate(
        &wasmtiny::aot::AotStore::shared(),
        &module,
        &imports,
    )
    .unwrap();

    match instance.invoke(1, &[]) {
        Err(WasmError::Trap(TrapCode::HostTrap)) => {}
        other => panic!("expected HostTrap, got {other:?}"),
    }
}

#[test]
fn in_bounds_memory_access_still_works_after_trap() {
    // The recovery boundary must leave the instance healthy: after a trap,
    // a second invocation succeeds.
    let mut instance = instantiate(
        "(module
           (memory 1)
           (func (export \"load\") (param i32) (result i32)
             (i32.load (local.get 0))))",
    );
    assert!(matches!(
        instance.invoke(0, &[WasmValue::I32(100_000)]),
        Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
    ));
    let results = instance.invoke(0, &[WasmValue::I32(0)]).expect("recovers");
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

fn instantiate(source: &str) -> wasmtiny::aot::AotInstance {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    let module = AotLoader::new().load(&bytes).expect("artifact loads");
    wasmtiny::aot::AotInstance::new(&module).expect("instantiation succeeds")
}

#[test]
fn out_of_bounds_read_traps() {
    let code = trap(
        "(module
           (memory 1)
           (func (export \"load\") (param i32) (result i32)
             (i32.load (local.get 0))))",
        0,
        &[WasmValue::I32(100_000)],
    );
    assert_eq!(code, TrapCode::MemoryOutOfBounds);
}

#[test]
fn out_of_bounds_write_traps() {
    let code = trap(
        "(module
           (memory 1)
           (func (export \"store\") (param i32)
             (i32.store (local.get 0) (i32.const 7))))",
        0,
        &[WasmValue::I32(100_000)],
    );
    assert_eq!(code, TrapCode::MemoryOutOfBounds);
}

/// Regression: trap containment must hold on a thread that never
/// instantiated anything — `sigaction` is process-wide but `sigaltstack` is
/// per-thread, so the invoking thread registers its own alt-stack before
/// entering native code. The instance below is instantiated on the main
/// thread and only *invoked* on the secondary one; a deep recursion there
/// must trap, not crash the process.
#[test]
fn stack_overflow_traps_on_a_secondary_thread() {
    use std::sync::mpsc;

    // Instantiated on THIS thread; the alt-stack used to be registered
    // only here, leaving the invoking thread unprotected.
    // Instantiated on THIS thread; the alt-stack used to be registered
    // only here, leaving the invoking thread unprotected.
    struct SendInstance(wasmtiny::aot::AotInstance);
    // SAFETY: the instance is moved to the thread and used only there; no
    // reference is retained on the main thread after the move. This is the
    // supported cross-thread story: compiled code and the store are
    // reentrant, and recovery state is per-thread.
    unsafe impl Send for SendInstance {}
    let instance = SendInstance(instantiate(
        "(module (func $f (export \"recurse\") (call $f)))",
    ));

    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("secondary-invoker".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut instance = instance;
            let outcome = instance.0.invoke(0, &[]);
            tx.send(outcome.err()).expect("channel alive");
        })
        .expect("thread spawns");
    handle.join().expect("thread must survive trap recovery");

    let error = rx.recv().expect("result delivered");
    match error {
        Some(WasmError::Trap(TrapCode::StackOverflow)) => {}
        other => panic!("expected StackOverflow on the secondary thread, got {other:?}"),
    }
}

#[test]
fn stack_overflow_traps_without_crashing() {
    let code = trap("(module (func $f (export \"recurse\") (call $f)))", 0, &[]);
    assert_eq!(code, TrapCode::StackOverflow);
}

fn trap(source: &str, func_idx: u32, args: &[WasmValue]) -> TrapCode {
    let mut instance = instantiate(source);
    match instance.invoke(func_idx, args) {
        Err(WasmError::Trap(code)) => code,
        Err(other) => panic!("expected a trap, got error {other}"),
        Ok(values) => panic!("expected a trap, got {values:?}"),
    }
}

#[test]
fn unreachable_traps() {
    let code = trap("(module (func (export \"trap\") (unreachable)))", 0, &[]);
    assert_eq!(code, TrapCode::Unreachable);
}
