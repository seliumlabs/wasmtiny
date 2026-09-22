//! Multi-return functions whose results overflow the target's return
//! registers: the implicit StructReturn path.
//!
//! x86_64 has only 2 integer return registers (rax/rdx), so functions with
//! 3+ i64 results previously failed to compile with Cranelift's "Too many
//! return values" error; `enable_multi_ret_implicit_sret` lets Cranelift
//! introduce a hidden return-area pointer instead.
//!
//! On aarch64 the same overflow needs 9 i64 results (8 return registers,
//! x0-x7), so these tests force the sret path on the host and verify the
//! full runtime round-trip: entry trampoline -> callee -> results array,
//! wasm->wasm direct calls, call_indirect, and host-call stubs.

use std::sync::{Arc, OnceLock};

use wasmtiny::{
    aot::{AotExtern, AotInstance, AotLoader, AotStore},
    runtime::{FunctionType, HostCaller, HostFunc, NumType, Result, ValType, WasmValue},
};
use wasmtiny_aotc::{CompileError, CompilerConfig, compile_artifact};

/// Host-call stub: an imported host function with 9 i64 results, invoked from
/// compiled wasm. The stub's machine ABI gets the hidden return-area pointer
/// and the caller must pass one too.
struct NineHost;

impl HostFunc for NineHost {
    fn call(&self, _caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let value = args[0].i64()?;
        Ok(vec![WasmValue::I64(value); 9])
    }

    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: OnceLock<FunctionType> = OnceLock::new();
        Some(TYPE.get_or_init(|| {
            FunctionType::new(
                vec![ValType::Num(NumType::I64)],
                vec![ValType::Num(NumType::I64); 9],
            )
        }))
    }
}

/// Compiles for the host and asserts success.
fn compile(source: &str) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds")
}

fn instantiate(source: &str) -> AotInstance {
    let loader = AotLoader::new();
    let module = loader.load(&compile(source)).expect("artifact loads");
    AotInstance::new(&module).expect("instantiation succeeds")
}

/// wasm -> wasm call_indirect through a table where the callee's results
/// overflow the return registers.
#[test]
fn nine_i64_results_call_indirect() {
    let mut instance = instantiate(
        "(module
           (type $t (func (param i64) (result i64 i64 i64 i64 i64 i64 i64 i64 i64)))
           (table 1 funcref)
           (func $nine (type $t)
             (local.get 0) (local.get 0) (local.get 0) (local.get 0) (local.get 0)
             (local.get 0) (local.get 0) (local.get 0) (local.get 0))
           (elem (i32.const 0) $nine)
           (func (export \"caller\") (param i64) (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
             (call_indirect (type $t) (local.get 0) (i32.const 0))))",
    );
    let results = instance
        .invoke(1, &[WasmValue::I64(5)])
        .expect("invoke succeeds");
    assert_eq!(results.len(), 9);
    assert!(
        results.iter().all(|v| *v == WasmValue::I64(5)),
        "{results:?}"
    );
}

/// Cross-module import: module B calls module A's exported 9-i64-result
/// function through the store-wide `FuncDesc` entry. The caller and callee are
/// compiled independently but must agree on the implicit return-area pointer.
#[test]
fn nine_i64_results_cross_module() {
    let store = AotStore::shared();
    let loader = AotLoader::new();
    let a = loader
        .load(&compile(
            "(module (func (export \"nine\") (param i64)
               (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
               (local.get 0) (local.get 0) (local.get 0) (local.get 0) (local.get 0)
               (local.get 0) (local.get 0) (local.get 0) (local.get 0)))",
        ))
        .expect("module A loads");
    let a_instance = AotInstance::instantiate(&store, &a, &[]).expect("module A instantiates");
    let handle = a_instance
        .func_handle(0)
        .expect("module A exports a function handle");

    let b = loader
        .load(&compile(
            "(module
               (import \"a\" \"nine\" (func $nine (param i64)
                  (result i64 i64 i64 i64 i64 i64 i64 i64 i64)))
               (func (export \"main\") (param i64)
                  (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
                  (call $nine (local.get 0))))",
        ))
        .expect("module B loads");

    let imports = [("a".to_string(), "nine".to_string(), AotExtern::Func(handle))];
    let mut b_instance =
        AotInstance::instantiate(&store, &b, &imports).expect("module B instantiates");

    let results = b_instance
        .invoke(1, &[WasmValue::I64(13)])
        .expect("invoke succeeds");
    assert_eq!(results.len(), 9);
    assert!(
        results.iter().all(|v| *v == WasmValue::I64(13)),
        "{results:?}"
    );
}

/// wasm -> wasm direct call where the callee's results overflow the return
/// registers: the caller must pass the hidden return-area pointer.
#[test]
fn nine_i64_results_direct_call() {
    let mut instance = instantiate(
        "(module
           (func $nine (param i64) (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
             (local.get 0) (local.get 0) (local.get 0) (local.get 0) (local.get 0)
             (local.get 0) (local.get 0) (local.get 0) (local.get 0))
           (func (export \"caller\") (param i64) (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
             (call $nine (local.get 0))))",
    );
    let results = instance
        .invoke(1, &[WasmValue::I64(3)])
        .expect("invoke succeeds");
    assert_eq!(results.len(), 9);
    assert!(
        results.iter().all(|v| *v == WasmValue::I64(3)),
        "{results:?}"
    );
}

#[test]
fn nine_i64_results_host_import() {
    let loader = AotLoader::new();
    let module = loader
        .load(&compile(
            "(module
               (import \"env\" \"nine\" (func $nine (param i64)
                  (result i64 i64 i64 i64 i64 i64 i64 i64 i64)))
               (func (export \"main\") (param i64)
                  (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
                  (call $nine (local.get 0))))",
        ))
        .expect("artifact loads");

    let imports = [(
        "env".to_string(),
        "nine".to_string(),
        AotExtern::HostFunc(Arc::new(NineHost)),
    )];
    let mut instance = AotInstance::instantiate(&AotStore::shared(), &module, &imports)
        .expect("instantiation succeeds");

    let results = instance
        .invoke(1, &[WasmValue::I64(11)])
        .expect("invoke succeeds");
    assert_eq!(results.len(), 9);
    assert!(
        results.iter().all(|v| *v == WasmValue::I64(11)),
        "{results:?}"
    );
}

/// The same 9-i64-result function, spelled in a few ways, forced through the
/// entry trampoline.
#[test]
fn nine_i64_results_via_trampoline() {
    let mut instance = instantiate(
        "(module (func (export \"f\") (param i64)
           (result i64 i64 i64 i64 i64 i64 i64 i64 i64)
           (local.get 0) (local.get 0) (local.get 0) (local.get 0) (local.get 0)
           (local.get 0) (local.get 0) (local.get 0) (local.get 0)))",
    );
    let results = instance
        .invoke(0, &[WasmValue::I64(7)])
        .expect("invoke succeeds");
    assert_eq!(results.len(), 9);
    assert!(
        results.iter().all(|v| *v == WasmValue::I64(7)),
        "{results:?}"
    );
}

/// The CI regression, targeted: x86_64 must now compile a 3-i64-result
/// function instead of rejecting it.
#[test]
fn x86_64_three_i64_results_compiles() {
    let wasm = wat::parse_str(
        "(module (func (export \"f\") (param i64 i64) (result i64 i64 i64)
           (local.get 0) (local.get 1) (local.get 0)))",
    )
    .unwrap();
    let config = CompilerConfig::for_target("x86_64-unknown-linux-gnu").unwrap();
    match compile_artifact(&wasm, &config) {
        Ok(_) => {}
        Err(CompileError::Codegen(message)) => {
            panic!("x86_64 codegen failed: {message}")
        }
        Err(other) => panic!("x86_64 other error: {other:?}"),
    }
}
