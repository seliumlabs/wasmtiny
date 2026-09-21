//! Atomic RMW and `memory.atomic.notify`/`wait` parity for the AOT path.

use wasmtiny::{
    aot::AotLoader,
    runtime::{TrapCode, WasmError, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

#[test]
fn atomic_cmpxchg_swaps_only_on_match() {
    let source = "(module (memory 1)
        (func (export \"cmpxchg\") (param i32 i32) (result i32)
          (i32.atomic.rmw.cmpxchg (i32.const 0) (local.get 0) (local.get 1))))";
    let mut instance = instantiate(source);
    // [0] = 0; expect 0, replace 7 -> succeeds, returns old 0.
    assert_eq!(
        instance
            .invoke(0, &[WasmValue::I32(0), WasmValue::I32(7)])
            .unwrap(),
        vec![WasmValue::I32(0)]
    );
    // expect 0 again -> mismatch, returns old 7, unchanged.
    assert_eq!(
        instance
            .invoke(0, &[WasmValue::I32(0), WasmValue::I32(9)])
            .unwrap(),
        vec![WasmValue::I32(7)]
    );
    // expect 7 -> succeeds, returns old 7, now 9.
    assert_eq!(
        instance
            .invoke(0, &[WasmValue::I32(7), WasmValue::I32(9)])
            .unwrap(),
        vec![WasmValue::I32(7)]
    );
}

#[test]
fn atomic_load_reads_zeroed_memory() {
    let source = "(module (memory 1)
        (func (export \"load\") (result i32)
          (i32.atomic.load (i32.const 0))))";
    let result = invoke(source, &[]).unwrap();
    assert_eq!(result, vec![WasmValue::I32(0)]);
}

#[test]
fn atomic_rmw_add_accumulates() {
    let source = "(module (memory 1)
        (func (export \"rmw\") (param i32) (result i32)
          (i32.atomic.rmw.add (i32.const 0) (local.get 0))))";
    let mut instance = instantiate(source);
    assert_eq!(
        instance.invoke(0, &[WasmValue::I32(5)]).unwrap(),
        vec![WasmValue::I32(0)]
    );
    assert_eq!(
        instance.invoke(0, &[WasmValue::I32(3)]).unwrap(),
        vec![WasmValue::I32(5)]
    );
    assert_eq!(
        instance.invoke(0, &[WasmValue::I32(1)]).unwrap(),
        vec![WasmValue::I32(8)]
    );
}

fn instantiate(source: &str) -> wasmtiny::aot::AotInstance {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    let module = AotLoader::new().load(&bytes).expect("artifact loads");
    wasmtiny::aot::AotInstance::new(&module).expect("instantiation succeeds")
}

fn invoke(source: &str, args: &[WasmValue]) -> Result<Vec<WasmValue>, ()> {
    let mut instance = instantiate(source);
    instance.invoke(0, args).map_err(|_| ())
}

#[test]
fn misaligned_notify_traps() {
    let code = trap(
        "(module (memory 1)
           (func (export \"notify\") (result i32)
             (memory.atomic.notify (i32.const 1) (i32.const 1))))",
        &[],
    );
    assert_eq!(code, TrapCode::MemoryOutOfBounds);
}

#[test]
fn notify_with_no_waiters_returns_zero() {
    let source = "(module (memory 1)
        (func (export \"notify\") (result i32)
          (memory.atomic.notify (i32.const 0) (i32.const 1))))";
    let result = invoke(source, &[]).unwrap();
    assert_eq!(result, vec![WasmValue::I32(0)]);
}

#[test]
fn out_of_bounds_atomic_load_traps() {
    let code = trap(
        "(module (memory 1)
           (func (export \"load\") (result i32)
             (i32.atomic.load (i32.const 100000))))",
        &[],
    );
    assert_eq!(code, TrapCode::MemoryOutOfBounds);
}

fn trap(source: &str, args: &[WasmValue]) -> TrapCode {
    let mut instance = instantiate(source);
    match instance.invoke(0, args) {
        Err(WasmError::Trap(code)) => code,
        Err(other) => panic!("expected a trap, got error {other}"),
        Ok(values) => panic!("expected a trap, got {values:?}"),
    }
}

#[test]
fn wait32_equal_with_zero_timeout_times_out() {
    // Memory is zeroed; the value matches, but a zero timeout must not block.
    let source = "(module (memory 1)
        (func (export \"wait\") (result i32)
          (memory.atomic.wait32 (i32.const 0) (i32.const 0) (i64.const 0))))";
    let result = invoke(source, &[]).unwrap();
    assert_eq!(result, vec![WasmValue::I32(2)]);
}

#[test]
fn wait32_mismatch_returns_one() {
    // Memory is zeroed; waiting for 42 must report "not equal".
    let source = "(module (memory 1)
        (func (export \"wait\") (result i32)
          (memory.atomic.wait32 (i32.const 0) (i32.const 42) (i64.const 0))))";
    let result = invoke(source, &[]).unwrap();
    assert_eq!(result, vec![WasmValue::I32(1)]);
}

#[test]
fn wait64_equal_with_timeout_times_out() {
    let source = "(module (memory 1)
        (func (export \"wait\") (result i32)
          (memory.atomic.wait64 (i32.const 0) (i64.const 0) (i64.const 1000000))))";
    let result = invoke(source, &[]).unwrap();
    assert_eq!(result, vec![WasmValue::I32(2)]);
}
