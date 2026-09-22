//! End-to-end native execution: `.wasm` → `.aot` → load → invoke.

use wasmtiny::{
    aot::{AotInstance, AotLoader},
    runtime::WasmValue,
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

#[test]
fn argument_type_mismatch_is_rejected() {
    let mut instance =
        instantiate("(module (func (export \"f\") (param i32) (result i32) (local.get 0)))");
    let err = instance
        .invoke(0, &[WasmValue::I64(1)])
        .expect_err("type mismatch is rejected");
    assert!(format!("{err}").contains("argument"), "got {err}");
}

fn compile(source: &str) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds")
}

#[test]
fn float_arithmetic_executes() {
    let mut instance = instantiate(
        "(module (func (export \"addf\") (param f64 f64) (result f64)
           (f64.add (local.get 0) (local.get 1))))",
    );
    let results = instance
        .invoke(0, &[WasmValue::F64(1.5), WasmValue::F64(2.25)])
        .expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::F64(3.75)]);
}

#[test]
fn function_with_no_results_executes() {
    let mut instance = instantiate("(module (func (export \"nop\") (result i32) (i32.const 42)))");
    let results = instance.invoke(0, &[]).expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}

#[test]
fn i64_arithmetic_executes() {
    let mut instance = instantiate(
        "(module (func (export \"mul\") (param i64 i64) (result i64)
           (i64.mul (local.get 0) (local.get 1))))",
    );
    let results = instance
        .invoke(0, &[WasmValue::I64(0x1234_5678), WasmValue::I64(3)])
        .expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::I64(0x1234_5678 * 3)]);
}

fn instantiate(source: &str) -> AotInstance {
    let loader = AotLoader::new();
    let module = loader.load(&compile(source)).expect("artifact loads");
    AotInstance::new(&module).expect("instantiation succeeds")
}

#[test]
fn invoking_i32_add_returns_3() {
    let mut instance = instantiate(
        "(module (func (export \"add\") (param i32 i32) (result i32)
           (i32.add (local.get 0) (local.get 1))))",
    );
    let results = instance
        .invoke(0, &[WasmValue::I32(1), WasmValue::I32(2)])
        .expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::I32(3)]);
}
