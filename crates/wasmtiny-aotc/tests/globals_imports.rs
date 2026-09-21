//! Global materialisation and memory/global imports for the AOT path.

use std::sync::{Arc, Mutex};

use wasmtiny::{
    aot::{AotExtern, AotInstance, AotLoader, AotStore},
    runtime::{Global, GlobalType, Limits, Memory, MemoryType, NumType, ValType, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

#[test]
fn defined_immutable_global_is_readable() {
    let module = load(
        "(module
           (global $g i32 (i32.const 7))
           (func (export \"get\") (result i32) (global.get $g)))",
    );
    let mut instance = AotInstance::new(&module).expect("instantiation succeeds");
    assert_eq!(instance.invoke(0, &[]).unwrap(), vec![WasmValue::I32(7)]);
}

#[test]
fn defined_mutable_global_get_and_set() {
    let module = load(
        "(module
           (global $g (mut i32) (i32.const 42))
           (func (export \"get\") (result i32) (global.get $g))
           (func (export \"set\") (param i32) (global.set $g (local.get 0))))",
    );
    let mut instance = AotInstance::new(&module).expect("instantiation succeeds");
    assert_eq!(instance.invoke(0, &[]).unwrap(), vec![WasmValue::I32(42)]);
    instance.invoke(1, &[WasmValue::I32(9)]).unwrap();
    assert_eq!(instance.invoke(0, &[]).unwrap(), vec![WasmValue::I32(9)]);
}

#[test]
fn imported_global_is_bound_to_a_value() {
    let module = load(
        "(module
           (import \"env\" \"g\" (global $g i32))
           (func (export \"get\") (result i32) (global.get $g)))",
    );
    let global = Global::new(
        GlobalType::new(ValType::Num(NumType::I32), false),
        WasmValue::I32(11),
    )
    .expect("global constructs");
    let imports = [(
        "env".to_string(),
        "g".to_string(),
        AotExtern::Global(global),
    )];
    let mut instance =
        AotInstance::instantiate(&AotStore::shared(), &module, &imports).expect("instantiation");
    assert_eq!(instance.invoke(0, &[]).unwrap(), vec![WasmValue::I32(11)]);
}

#[test]
fn imported_memory_is_shared_with_host_data() {
    let module = load(
        "(module
           (import \"env\" \"mem\" (memory 1))
           (func (export \"store\") (param i32 i32)
             (i32.store (local.get 0) (local.get 1)))
           (func (export \"load\") (param i32) (result i32)
             (i32.load (local.get 0))))",
    );
    let memory = Memory::try_new(MemoryType::new(Limits::Min(1))).expect("memory allocates");
    let shared = Arc::new(Mutex::new(memory));
    let imports = [(
        "env".to_string(),
        "mem".to_string(),
        AotExtern::Memory(shared.clone()),
    )];
    let mut instance =
        AotInstance::instantiate(&AotStore::shared(), &module, &imports).expect("instantiation");
    instance
        .invoke(0, &[WasmValue::I32(0), WasmValue::I32(123)])
        .unwrap();
    assert_eq!(
        instance.invoke(1, &[WasmValue::I32(0)]).unwrap(),
        vec![WasmValue::I32(123)]
    );
    // The memory is genuinely shared: the host can observe the guest write.
    let memory = shared.lock().unwrap();
    assert_eq!(memory.read_i32(0).unwrap(), 123);
}

fn load(source: &str) -> wasmtiny::aot::AotModule {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

#[test]
fn start_function_runs_at_instantiation() {
    let module = load(
        "(module
           (memory 1)
           (func $start (i32.store (i32.const 0) (i32.const 99)))
           (start $start)
           (func (export \"load\") (result i32) (i32.load (i32.const 0))))",
    );
    let mut instance = AotInstance::new(&module).expect("instantiation succeeds");
    assert_eq!(instance.invoke(1, &[]).unwrap(), vec![WasmValue::I32(99)]);
}
