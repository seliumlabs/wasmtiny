#![no_main]

//! Fuzz interpreter dispatch over a validated module.
//!
//! The first defined function is invoked with zero-valued arguments under a
//! bounded execution budget, so hostile control flow cannot hang the target.

use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use wasmtiny::interpreter::Interpreter;
use wasmtiny::loader::{Parser, Validator};
use wasmtiny::runtime::{Instance, NumType, ValType, WasmValue};

const MAX_MODULE_BYTES: usize = 64 * 1024;
const EXECUTION_BUDGET: u64 = 100_000;

fn zero(value: &ValType) -> WasmValue {
    match value {
        ValType::Num(NumType::I32) => WasmValue::I32(0),
        ValType::Num(NumType::I64) => WasmValue::I64(0),
        ValType::Num(NumType::F32) => WasmValue::F32(0.0),
        ValType::Num(NumType::F64) => WasmValue::F64(0.0),
        ValType::Ref(reference) => WasmValue::NullRef(*reference),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_MODULE_BYTES {
        return;
    }
    let Ok(module) = Parser::new().parse(data) else {
        return;
    };
    if Validator::new().validate(&module).is_err() {
        return;
    }
    let Some(function) = module.funcs.first() else {
        return;
    };
    let Some(function_type) = module.type_at(function.type_idx) else {
        return;
    };
    let args: Vec<WasmValue> = function_type.params.iter().map(zero).collect();

    let Ok(instance) = Instance::with_imports(Arc::new(module.clone()), &[]) else {
        return;
    };
    let instance = Arc::new(Mutex::new(instance));
    if let Ok(instance) = instance.lock() {
        let _ = instance.set_execution_budget(Some(EXECUTION_BUDGET));
        let _ = instance.set_memory_budget(Some(64));
    }
    let mut interpreter = Interpreter::with_instance(instance);
    let _ = interpreter.execute_function(&module, 0, &args);
});
