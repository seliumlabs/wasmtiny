//! Host-function imports called from compiled wasm code.

use std::sync::{Arc, OnceLock};

use wasmtiny::aot::{AotExtern, AotInstance, AotLoader, AotStore};
use wasmtiny::runtime::{FunctionType, HostCaller, HostFunc, NumType, Result, ValType, WasmValue};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

struct AddHost;

impl HostFunc for AddHost {
    fn call(&self, _caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let a = args[0].i32()?;
        let b = args[1].i32()?;
        Ok(vec![WasmValue::I32(a + b)])
    }

    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: OnceLock<FunctionType> = OnceLock::new();
        Some(TYPE.get_or_init(|| {
            FunctionType::new(
                vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
                vec![ValType::Num(NumType::I32)],
            )
        }))
    }
}

struct ScaleHost;

impl HostFunc for ScaleHost {
    fn call(&self, _caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let factor = args[0].i64()?;
        let value = args[1].f64()?;
        Ok(vec![WasmValue::F64(value * factor as f64)])
    }

    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: OnceLock<FunctionType> = OnceLock::new();
        Some(TYPE.get_or_init(|| {
            FunctionType::new(
                vec![ValType::Num(NumType::I64), ValType::Num(NumType::F64)],
                vec![ValType::Num(NumType::F64)],
            )
        }))
    }
}

fn compile(source: &str) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds")
}

#[test]
fn host_function_is_called_with_correct_arguments() {
    let loader = AotLoader::new();
    let module = loader
        .load(&compile(
            "(module
               (import \"env\" \"add\" (func $add (param i32 i32) (result i32)))
               (func (export \"main\") (param i32 i32) (result i32)
                 (call $add (local.get 0) (local.get 1))))",
        ))
        .expect("artifact loads");

    let imports = [(
        "env".to_string(),
        "add".to_string(),
        AotExtern::HostFunc(Arc::new(AddHost)),
    )];
    let mut instance = AotInstance::instantiate(&AotStore::shared(), &module, &imports)
        .expect("instantiation succeeds");

    // `main` is function index 1 (the import occupies index 0).
    let results = instance
        .invoke(1, &[WasmValue::I32(20), WasmValue::I32(22)])
        .expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}

#[test]
fn mixed_type_host_function_receives_types() {
    let loader = AotLoader::new();
    let module = loader
        .load(&compile(
            "(module
               (import \"env\" \"scale\" (func $scale (param i64 f64) (result f64)))
               (func (export \"main\") (param i64 f64) (result f64)
                 (call $scale (local.get 0) (local.get 1))))",
        ))
        .expect("artifact loads");

    let imports = [(
        "env".to_string(),
        "scale".to_string(),
        AotExtern::HostFunc(Arc::new(ScaleHost)),
    )];
    let mut instance = AotInstance::instantiate(&AotStore::shared(), &module, &imports)
        .expect("instantiation succeeds");

    let results = instance
        .invoke(1, &[WasmValue::I64(3), WasmValue::F64(1.5)])
        .expect("invoke succeeds");
    assert_eq!(results, vec![WasmValue::F64(4.5)]);
}

#[test]
fn unsatisfied_import_is_rejected() {
    let loader = AotLoader::new();
    let module = loader
        .load(&compile(
            "(module (import \"env\" \"missing\" (func $missing (param i32))))",
        ))
        .expect("artifact loads");

    let err = AotInstance::instantiate(&AotStore::shared(), &module, &[])
        .err()
        .expect("unsatisfied import must fail instantiation");
    assert!(format!("{err}").contains("not satisfied"), "got {err}");
}

#[test]
fn import_type_mismatch_is_rejected() {
    let loader = AotLoader::new();
    let module = loader
        .load(&compile(
            "(module (import \"env\" \"add\" (func $add (param i64 i64) (result i64))))",
        ))
        .expect("artifact loads");

    // AddHost declares (i32,i32)->i32; the module demands (i64,i64)->i64.
    let imports = [(
        "env".to_string(),
        "add".to_string(),
        AotExtern::HostFunc(Arc::new(AddHost)),
    )];
    let err = AotInstance::instantiate(&AotStore::shared(), &module, &imports)
        .err()
        .expect("type mismatch must fail instantiation");
    assert!(format!("{err}").contains("mismatch"), "got {err}");
}
