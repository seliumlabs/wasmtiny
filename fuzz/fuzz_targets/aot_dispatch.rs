#![no_main]

//! Fuzz AOT compilation, artifact loading, and native dispatch.
//!
//! Arbitrary wasm bytes are compiled by the AOT compiler, loaded through the
//! artifact verifier, instantiated, and the first defined function is invoked
//! under a bounded execution budget. Every stage returns an error rather than
//! panicking on hostile input.

use libfuzzer_sys::fuzz_target;
use wasmtiny::aot::{AotInstance, AotLoader};
use wasmtiny::runtime::{NumType, ValType, WasmValue};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

const MAX_MODULE_BYTES: usize = 32 * 1024;
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
    let Ok(artifact) = compile_artifact(data, &CompilerConfig::host()) else {
        return;
    };
    let Ok(module) = AotLoader::new().load(&artifact) else {
        return;
    };
    let Ok(mut instance) = AotInstance::new(&module) else {
        return;
    };
    let _ = instance.set_execution_budget(Some(EXECUTION_BUDGET));

    let Some(function) = module.functions.first() else {
        return;
    };
    let Some(function_type) = module.types.get(function.type_idx as usize) else {
        return;
    };
    let args: Vec<WasmValue> = function_type.params.iter().map(zero).collect();
    let _ = instance.invoke(function.func_index, &args);
});
