//! `.wast` spec-corpus runner for the AOT path.
//!
//! Each core-spec fixture is compiled in-process with the `wasmtiny-aotc`
//! library (no external binaries), loaded by the strict artifact loader, and
//! executed natively through `AotInstance`.

#![cfg(feature = "aot")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use wasmtiny::aot::{AotExtern, AotInstance, AotLoader, AotStore, AotTable};
use wasmtiny::runtime::{
    ExportKind, FunctionType, Global, HostCaller, HostFunc, ImportKind, Limits, Memory, MemoryType,
    NumType, RefType, TableType, ValType, WasmValue,
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};
use wast::{
    QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet,
    core::{AbstractHeapType, NanPattern, WastArgCore, WastRetCore},
    parser::{self, ParseBuffer},
};

const SPEC_DIR: &str = "tests/spec-core/";

/// A compiled + instantiated module, keyed by its WAST `$name` (or "current"),
/// plus the loaded artifact (for export-name → index resolution).
struct ModuleState {
    module: wasmtiny::aot::AotModule,
    instance: RefCell<AotInstance>,
}

impl ModuleState {
    fn export_index(&self, name: &str, want: impl Fn(&ExportKind) -> Option<u32>) -> Option<u32> {
        self.module
            .exports
            .iter()
            .find(|export| export.name == name)
            .and_then(|export| want(&export.kind))
    }
}

struct AotSpecHarness {
    store: Arc<Mutex<AotStore>>,
    modules: HashMap<String, Rc<ModuleState>>,
    current: Option<Rc<ModuleState>>,
}

impl AotSpecHarness {
    fn new() -> Self {
        Self {
            store: AotStore::shared(),
            modules: HashMap::new(),
            current: None,
        }
    }

    fn compile_instantiate(&mut self, wasm: &[u8]) -> Result<Rc<ModuleState>, String> {
        let artifact = match compile_artifact(wasm, &CompilerConfig::host()) {
            Ok(artifact) => artifact,
            Err(error) if is_inapplicable(&error) => {
                return Err(format!("unsupported module: {error}"));
            }
            Err(error) => return Err(format!("module compile failed: {error}")),
        };
        let loaded = AotLoader::new()
            .load(&artifact)
            .map_err(|error| format!("artifact load failed: {error}"))?;

        let imports: Vec<(String, String, AotExtern)> = loaded
            .imports
            .iter()
            .map(|import| self.resolve_import(import))
            .collect::<Result<_, _>>()?;

        let instance = AotInstance::instantiate(&self.store, &loaded, &imports)
            .map_err(|error| format!("module instantiation failed: {error}"))?;

        Ok(Rc::new(ModuleState {
            module: loaded,
            instance: RefCell::new(instance),
        }))
    }

    fn resolve_import(
        &self,
        import: &wasmtiny::runtime::Import,
    ) -> Result<(String, String, AotExtern), String> {
        if import.module == "spectest" {
            return Ok((
                import.module.clone(),
                import.name.clone(),
                self.spectest(import)?,
            ));
        }

        let source = self
            .modules
            .get(&import.module)
            .cloned()
            .ok_or_else(|| format!("unknown import module {}", import.module))?;

        let extern_ = match &import.kind {
            ImportKind::Func(_) => {
                let idx = source
                    .export_index(&import.name, |kind| match kind {
                        ExportKind::Func(idx) => Some(*idx),
                        _ => None,
                    })
                    .ok_or_else(|| format!("function export {} not found", import.name))?;
                let handle = source
                    .instance
                    .borrow()
                    .func_handle(idx)
                    .ok_or_else(|| format!("function {} not found", idx))?;
                AotExtern::Func(handle)
            }
            ImportKind::Table(_) => {
                let idx = source
                    .export_index(&import.name, |kind| match kind {
                        ExportKind::Table(idx) => Some(*idx),
                        _ => None,
                    })
                    .ok_or_else(|| format!("table export {} not found", import.name))?;
                let table = source
                    .instance
                    .borrow()
                    .table_handle(idx)
                    .ok_or_else(|| format!("table {} not found", idx))?;
                AotExtern::Table(table)
            }
            ImportKind::Memory(_) => {
                let idx = source
                    .export_index(&import.name, |kind| match kind {
                        ExportKind::Memory(idx) => Some(*idx),
                        _ => None,
                    })
                    .ok_or_else(|| format!("memory export {} not found", import.name))?;
                let memory = source
                    .instance
                    .borrow()
                    .memory_handle(idx)
                    .ok_or_else(|| format!("memory {} not found", idx))?;
                AotExtern::Memory(memory)
            }
            ImportKind::Global(gty) => {
                let idx = source
                    .export_index(&import.name, |kind| match kind {
                        ExportKind::Global(idx) => Some(*idx),
                        _ => None,
                    })
                    .ok_or_else(|| format!("global export {} not found", import.name))?;
                let value = source
                    .instance
                    .borrow()
                    .global_value(idx)
                    .ok_or_else(|| format!("global {} not found", idx))?;
                AotExtern::Global(
                    Global::new(gty.clone(), value)
                        .map_err(|error| format!("import global type mismatch: {error}"))?,
                )
            }
            ImportKind::Tag(..) => {
                return Err(format!(
                    "tag import {}.{} is unsupported",
                    import.module, import.name
                ));
            }
        };

        Ok((import.module.clone(), import.name.clone(), extern_))
    }

    fn spectest(&self, import: &wasmtiny::runtime::Import) -> Result<AotExtern, String> {
        match &import.kind {
            ImportKind::Func(_) => {
                let ty = spectest_function_type(&import.name)?;
                Ok(AotExtern::HostFunc(Arc::new(NoOpHostFunc {
                    function_type: ty,
                })))
            }
            ImportKind::Memory(_) => {
                if import.name != "memory" {
                    return Err(format!(
                        "unknown spectest memory {}.{}",
                        import.module, import.name
                    ));
                }
                Ok(AotExtern::Memory(Arc::new(Mutex::new(
                    Memory::try_new(MemoryType::new(Limits::MinMax(1, 2)))
                        .map_err(|error| error.to_string())?,
                ))))
            }
            ImportKind::Table(expected) => {
                if import.name != "table" {
                    return Err(format!(
                        "unknown spectest table {}.{}",
                        import.module, import.name
                    ));
                }
                Ok(AotExtern::Table(Arc::new(Mutex::new(
                    AotTable::with_initial(
                        TableType::new(expected.elem_type, Limits::MinMax(10, 20)),
                        10,
                    )
                    .map_err(|error| error.to_string())?,
                ))))
            }
            ImportKind::Global(gty) => {
                let value = match import.name.as_str() {
                    "global_i32" => WasmValue::I32(666),
                    "global_i64" => WasmValue::I64(666),
                    "global_f32" => WasmValue::F32(666.6),
                    "global_f64" => WasmValue::F64(666.6),
                    other => return Err(format!("unsupported spectest global {other}")),
                };
                Ok(AotExtern::Global(Global::new(gty.clone(), value).map_err(
                    |error| format!("invalid spectest global: {error}"),
                )?))
            }
            ImportKind::Tag(..) => Err(format!(
                "unsupported spectest tag {}.{}",
                import.module, import.name
            )),
        }
    }

    fn lookup(&self, name: Option<&str>) -> Result<Rc<ModuleState>, String> {
        match name {
            Some(name) => self
                .modules
                .get(name)
                .cloned()
                .ok_or_else(|| format!("unknown module id ${name}")),
            None => self
                .current
                .clone()
                .ok_or_else(|| "no current module available".to_string()),
        }
    }

    fn execute(&mut self, exec: WastExecute<'_>) -> Result<Vec<WasmValue>, String> {
        match exec {
            WastExecute::Invoke(invoke) => self.execute_invoke(&invoke),
            WastExecute::Wat(mut module) => {
                let wasm = module
                    .encode()
                    .map_err(|error| format!("module encoding failed: {error}"))?;
                self.compile_instantiate(&wasm)?;
                Ok(Vec::new())
            }
            WastExecute::Get { module, global, .. } => {
                let state = self.lookup(module.map(|id| id.name()))?;
                let idx = state
                    .export_index(global, |kind| match kind {
                        ExportKind::Global(idx) => Some(*idx),
                        _ => None,
                    })
                    .ok_or_else(|| format!("global {global} not found"))?;
                let value = state
                    .instance
                    .borrow()
                    .global_value(idx)
                    .ok_or_else(|| format!("global {idx} not found"))?;
                Ok(vec![value])
            }
        }
    }

    fn execute_invoke(&mut self, invoke: &WastInvoke<'_>) -> Result<Vec<WasmValue>, String> {
        let state = self.lookup(invoke.module.map(|id| id.name()))?;
        let idx = state
            .export_index(invoke.name, |kind| match kind {
                ExportKind::Func(idx) => Some(*idx),
                _ => None,
            })
            .ok_or_else(|| format!("function {} not found", invoke.name))?;
        let args = invoke
            .args
            .iter()
            .map(wast_arg_to_value)
            .collect::<Result<Vec<_>, _>>()?;
        state
            .instance
            .borrow_mut()
            .invoke(idx, &args)
            .map_err(|error| format!("invoke {} failed: {error}", invoke.name))
    }

    fn run_directive(&mut self, directive: WastDirective<'_>) -> Result<Outcome, String> {
        match directive {
            WastDirective::Module(mut module) => {
                let name = module.name().map(|id| id.name().to_string());
                let wasm = module
                    .encode()
                    .map_err(|error| format!("module encoding failed: {error}"))?;
                let state = match self.compile_instantiate(&wasm) {
                    Ok(state) => state,
                    Err(error) if is_skip(&error) => {
                        self.current = None;
                        return Ok(Outcome::Skipped);
                    }
                    Err(error) => return Err(error),
                };
                if let Some(name) = name {
                    self.modules.insert(name, state.clone());
                }
                self.current = Some(state);
                Ok(Outcome::None)
            }
            WastDirective::ModuleDefinition(mut module) => {
                let wasm = module
                    .encode()
                    .map_err(|error| format!("module encoding failed: {error}"))?;
                let compiled = match compile_artifact(&wasm, &CompilerConfig::host()) {
                    Ok(artifact) => artifact,
                    Err(error) if is_inapplicable(&error) => {
                        return Ok(Outcome::Skipped);
                    }
                    Err(error) => return Err(format!("module compile failed: {error}")),
                };
                AotLoader::new()
                    .load(&compiled)
                    .map(|_| ())
                    .map_err(|error| format!("module validation failed: {error}"))?;
                Ok(Outcome::None)
            }
            WastDirective::Register { name, module, .. } => {
                let state = match module {
                    Some(module) => match self.lookup(Some(module.name())) {
                        Ok(state) => state,
                        Err(error) if is_skip(&error) => {
                            return Ok(Outcome::Skipped);
                        }
                        Err(error) => return Err(error),
                    },
                    None => match self.current.clone() {
                        Some(state) => state,
                        None => {
                            return Ok(Outcome::Skipped);
                        }
                    },
                };
                self.modules.insert(name.to_string(), state);
                Ok(Outcome::None)
            }
            WastDirective::Invoke(invoke) => match self.execute_invoke(&invoke) {
                Ok(_) => Ok(Outcome::None),
                Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                Err(error) => Err(error),
            },
            WastDirective::AssertReturn { exec, results, .. } => {
                match self.assert_return(exec, results) {
                    Ok(()) => Ok(Outcome::Passed),
                    Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                    Err(error) => Err(error),
                }
            }
            WastDirective::AssertTrap { exec, .. } => match self.assert_trap(exec) {
                Ok(()) => Ok(Outcome::Passed),
                Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                Err(error) => Err(error),
            },
            WastDirective::AssertExhaustion { call, .. } => match self.execute_invoke(&call) {
                Ok(_) => Err(
                    "assert_exhaustion expected an execution failure, but call succeeded"
                        .to_string(),
                ),
                Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                Err(_) => Ok(Outcome::Passed),
            },
            WastDirective::AssertInvalid { mut module, .. } => {
                match self.assert_invalid(&mut module) {
                    Ok(()) => Ok(Outcome::Passed),
                    Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                    Err(error) => Err(error),
                }
            }
            WastDirective::AssertMalformed { mut module, .. } => {
                match self.assert_malformed(&mut module) {
                    Ok(()) => Ok(Outcome::Passed),
                    Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                    Err(error) => Err(error),
                }
            }
            WastDirective::AssertUnlinkable { module, .. } => {
                match self.assert_unlinkable(module) {
                    Ok(()) => Ok(Outcome::Passed),
                    Err(error) if is_skip(&error) => Ok(Outcome::Skipped),
                    Err(error) => Err(error),
                }
            }
            WastDirective::AssertException { .. }
            | WastDirective::AssertSuspension { .. }
            | WastDirective::AssertInvalidCustom { .. }
            | WastDirective::AssertMalformedCustom { .. }
            | WastDirective::Thread(_)
            | WastDirective::Wait { .. }
            | WastDirective::ModuleInstance { .. } => Ok(Outcome::Skipped),
        }
    }

    fn assert_return(
        &mut self,
        exec: WastExecute<'_>,
        results: Vec<WastRet<'_>>,
    ) -> Result<(), String> {
        let values = self.execute(exec)?;
        if values.len() != results.len() {
            return Err(format!(
                "expected {} return values, got {}",
                results.len(),
                values.len()
            ));
        }
        for (actual, expected) in values.iter().zip(results.iter()) {
            if !matches_return(actual, expected) {
                return Err(format!("expected {expected:?}, got {actual:?}"));
            }
        }
        Ok(())
    }

    fn assert_trap(&mut self, exec: WastExecute<'_>) -> Result<(), String> {
        match self.execute(exec) {
            Err(error) if is_skip(&error) => Err(error),
            Err(_) => Ok(()),
            Ok(_) => Err(
                "assert_trap expected an execution failure, but execution succeeded".to_string(),
            ),
        }
    }

    fn assert_invalid(&mut self, module: &mut QuoteWat<'_>) -> Result<(), String> {
        let wasm = match module.encode() {
            Ok(wasm) => wasm,
            Err(_) => return Ok(()), // malformed quotes fail before compilation
        };
        match compile_artifact(&wasm, &CompilerConfig::host()) {
            Ok(_) => Err("assert_invalid expected compilation to fail".to_string()),
            Err(_) => Ok(()),
        }
    }

    fn assert_malformed(&mut self, module: &mut QuoteWat<'_>) -> Result<(), String> {
        match module.encode() {
            Ok(wasm) => match compile_artifact(&wasm, &CompilerConfig::host()) {
                Ok(artifact) => {
                    if AotLoader::new().load(&artifact).is_ok() {
                        Err("assert_malformed expected loading to fail".to_string())
                    } else {
                        Ok(())
                    }
                }
                Err(_) => Ok(()),
            },
            Err(_) => Ok(()),
        }
    }

    fn assert_unlinkable(&mut self, mut module: wast::Wat<'_>) -> Result<(), String> {
        let wasm = module
            .encode()
            .map_err(|error| format!("module encoding failed: {error}"))?;
        let artifact = match compile_artifact(&wasm, &CompilerConfig::host()) {
            Ok(artifact) => artifact,
            Err(error) if is_inapplicable(&error) => {
                return Err(format!("unsupported module: {error}"));
            }
            Err(error) => return Err(format!("module compile failed: {error}")),
        };
        let loaded = AotLoader::new()
            .load(&artifact)
            .map_err(|error| format!("artifact load failed: {error}"))?;
        let imports: Vec<(String, String, AotExtern)> = match loaded
            .imports
            .iter()
            .map(|import| self.resolve_import(import))
            .collect::<Result<_, _>>()
        {
            Ok(imports) => imports,
            // A dependency module was skipped for feature reasons: skip.
            Err(error) if is_skip(&error) => return Err(error),
            // An unresolvable import is exactly the unlinkable condition.
            Err(_) => return Ok(()),
        };
        AotInstance::instantiate(&self.store, &loaded, &imports)
            .err()
            .ok_or_else(|| {
                "assert_unlinkable expected instantiation to fail, but it succeeded".to_string()
            })?;
        Ok(())
    }
}

enum Outcome {
    None,
    Passed,
    Skipped,
}

struct NoOpHostFunc {
    function_type: FunctionType,
}

impl HostFunc for NoOpHostFunc {
    fn call(
        &self,
        _caller: &mut HostCaller<'_>,
        _args: &[WasmValue],
    ) -> Result<Vec<WasmValue>, wasmtiny::runtime::WasmError> {
        Ok(Vec::new())
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(&self.function_type)
    }
}

/// A module that cannot apply to the AOT path: it uses a proposal outside the
/// compiler's v1 feature gate, so compilation rejects it and the harness skips
/// it (and directives that depend on it) rather than failing.
fn is_inapplicable(error: &wasmtiny_aotc::CompileError) -> bool {
    match error {
        wasmtiny_aotc::CompileError::Unsupported(_) => true,
        wasmtiny_aotc::CompileError::Validation(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("constant expression required")
                || message.contains("function references required")
                || message.contains("tables with expression initializers")
                || message.contains("gc feature")
                || message.contains("exceptions proposal")
                || message.contains("table size is out of bounds")
        }
        _ => false,
    }
}

/// The fixed signatures of the `spectest` host functions.
fn spectest_function_type(name: &str) -> Result<FunctionType, String> {
    let params = match name {
        "print" => vec![],
        "print_i32" => vec![ValType::Num(NumType::I32)],
        "print_i64" => vec![ValType::Num(NumType::I64)],
        "print_f32" => vec![ValType::Num(NumType::F32)],
        "print_f64" => vec![ValType::Num(NumType::F64)],
        "print_i32_f32" => vec![ValType::Num(NumType::I32), ValType::Num(NumType::F32)],
        "print_f64_f64" => vec![ValType::Num(NumType::F64), ValType::Num(NumType::F64)],
        other => {
            return Err(format!("unknown spectest function {other}"));
        }
    };
    Ok(FunctionType::new(params, vec![]))
}

/// Whether an execution error represents a skipped (rather than failed)
/// directive: the module it depends on was skipped for feature reasons, or it
/// has no current module.
fn is_skip(error: &str) -> bool {
    error.contains("no current module")
        || error.contains("register directive")
        || error.contains("unknown module id")
        || error.contains("unknown import module")
        || error.starts_with("unsupported module:")
}

// ---------------------------------------------------------------------------
// WAST value helpers (mirrors `tests/spec.rs`).
// ---------------------------------------------------------------------------

fn heap_type_to_ref_type(heap_type: &wast::core::HeapType<'_>) -> Result<RefType, String> {
    match heap_type {
        wast::core::HeapType::Abstract {
            ty: AbstractHeapType::Func | AbstractHeapType::NoFunc,
            ..
        }
        | wast::core::HeapType::Concrete(_)
        | wast::core::HeapType::Exact(_) => Ok(RefType::FuncRef),
        wast::core::HeapType::Abstract {
            ty: AbstractHeapType::Extern | AbstractHeapType::NoExtern,
            ..
        } => Ok(RefType::ExternRef),
        _ => Err("unsupported heap type in WAST reference".to_string()),
    }
}

fn wast_arg_to_value(arg: &WastArg<'_>) -> Result<WasmValue, String> {
    match arg {
        WastArg::Core(core) => wast_core_arg_to_value(core),
        _ => Err("component-model WAST arguments are unsupported".to_string()),
    }
}

fn wast_core_arg_to_value(arg: &WastArgCore<'_>) -> Result<WasmValue, String> {
    match arg {
        WastArgCore::I32(value) => Ok(WasmValue::I32(*value)),
        WastArgCore::I64(value) => Ok(WasmValue::I64(*value)),
        WastArgCore::F32(value) => Ok(WasmValue::F32(f32::from_bits(value.bits))),
        WastArgCore::F64(value) => Ok(WasmValue::F64(f64::from_bits(value.bits))),
        WastArgCore::RefNull(heap_type) => {
            Ok(WasmValue::NullRef(heap_type_to_ref_type(heap_type)?))
        }
        WastArgCore::RefExtern(value) => Ok(WasmValue::ExternRef(*value)),
        WastArgCore::RefHost(value) => Ok(WasmValue::ExternRef(*value)),
        WastArgCore::V128(_) => Err("v128 WAST arguments are unsupported".to_string()),
    }
}

fn matches_core_return(actual: &WasmValue, expected: &WastRetCore<'_>) -> bool {
    match expected {
        WastRetCore::I32(expected) => matches!(actual, WasmValue::I32(value) if value == expected),
        WastRetCore::I64(expected) => matches!(actual, WasmValue::I64(value) if value == expected),
        WastRetCore::F32(pattern) => {
            matches!(actual, WasmValue::F32(value) if matches_f32_pattern(*value, pattern))
        }
        WastRetCore::F64(pattern) => {
            matches!(actual, WasmValue::F64(value) if matches_f64_pattern(*value, pattern))
        }
        WastRetCore::RefNull(_) => matches!(actual, WasmValue::NullRef(_)),
        WastRetCore::RefExtern(Some(expected)) => {
            matches!(actual, WasmValue::ExternRef(value) if value == expected)
        }
        WastRetCore::RefExtern(None) => matches!(actual, WasmValue::ExternRef(_)),
        WastRetCore::RefFunc(_) => matches!(actual, WasmValue::FuncRef(_)),
        WastRetCore::Either(cases) => cases.iter().any(|case| matches_core_return(actual, case)),
        _ => false,
    }
}

fn matches_return(actual: &WasmValue, expected: &WastRet<'_>) -> bool {
    match expected {
        WastRet::Core(expected) => matches_core_return(actual, expected),
        _ => false,
    }
}

fn matches_f32_pattern(actual: f32, pattern: &NanPattern<wast::token::F32>) -> bool {
    match pattern {
        NanPattern::Value(expected) => actual.to_bits() == expected.bits,
        NanPattern::CanonicalNan | NanPattern::ArithmeticNan => actual.is_nan(),
    }
}

fn matches_f64_pattern(actual: f64, pattern: &NanPattern<wast::token::F64>) -> bool {
    match pattern {
        NanPattern::Value(expected) => actual.to_bits() == expected.bits,
        NanPattern::CanonicalNan | NanPattern::ArithmeticNan => actual.is_nan(),
    }
}

// ---------------------------------------------------------------------------
// Runner plumbing.
// ---------------------------------------------------------------------------

macro_rules! spec_test {
    ($name:ident, $file:literal) => {
        #[test]
        fn $name() {
            assert_spec_passes($file);
        }
    };
}

spec_test!(test_spec_block, "block.wast");
spec_test!(test_spec_br, "br.wast");
spec_test!(test_spec_br_if, "br_if.wast");
spec_test!(test_spec_br_table, "br_table.wast");
spec_test!(test_spec_call, "call.wast");
spec_test!(test_spec_call_indirect, "call_indirect.wast");
spec_test!(test_spec_const, "const.wast");
spec_test!(test_spec_conversions, "conversions.wast");
spec_test!(test_spec_data, "data.wast");
spec_test!(test_spec_elem, "elem.wast");
spec_test!(test_spec_exports, "exports.wast");
spec_test!(test_spec_f32, "f32.wast");
spec_test!(test_spec_f32_cmp, "f32_cmp.wast");
spec_test!(test_spec_f64, "f64.wast");
spec_test!(test_spec_f64_cmp, "f64_cmp.wast");
spec_test!(test_spec_fac, "fac.wast");
spec_test!(test_spec_float_literals, "float_literals.wast");
spec_test!(test_spec_float_memory, "float_memory.wast");
spec_test!(test_spec_float_misc, "float_misc.wast");
spec_test!(test_spec_func, "func.wast");
spec_test!(test_spec_global, "global.wast");
spec_test!(test_spec_i32, "i32.wast");
spec_test!(test_spec_id, "id.wast");
spec_test!(test_spec_imports, "imports.wast");
spec_test!(test_spec_int_literals, "int_literals.wast");
spec_test!(test_spec_labels, "labels.wast");
spec_test!(test_spec_load, "load.wast");
spec_test!(test_spec_local_get, "local_get.wast");
spec_test!(test_spec_local_set, "local_set.wast");
spec_test!(test_spec_local_tee, "local_tee.wast");
spec_test!(test_spec_loop, "loop.wast");
spec_test!(test_spec_memory, "memory.wast");
spec_test!(test_spec_memory_grow, "memory_grow.wast");
spec_test!(test_spec_memory_size, "memory_size.wast");
spec_test!(test_spec_memory_trap, "memory_trap.wast");
spec_test!(test_spec_nop, "nop.wast");
spec_test!(test_spec_ref_is_null, "ref_is_null.wast");
spec_test!(test_spec_return, "return.wast");
spec_test!(test_spec_select, "select.wast");
spec_test!(test_spec_start, "start.wast");
spec_test!(test_spec_store, "store.wast");
spec_test!(test_spec_table, "table.wast");
spec_test!(test_spec_table_get, "table_get.wast");
spec_test!(test_spec_table_set, "table_set.wast");
spec_test!(test_spec_traps, "traps.wast");
spec_test!(test_spec_type, "type.wast");
spec_test!(test_spec_unreachable, "unreachable.wast");
spec_test!(test_spec_func_ptrs, "func_ptrs.wast");

fn assert_spec_passes(name: &str) {
    match run_spec_test(name) {
        TestResult::Passed => {}
        TestResult::Failed(message) => panic!("Expected pass, got failure: {message}"),
        TestResult::Error(message) => panic!("Expected pass, got error: {message}"),
    }
}

enum TestResult {
    Passed,
    Failed(String),
    Error(String),
}

fn run_spec_test(filename: &str) -> TestResult {
    let path = SPEC_DIR.to_owned() + filename;
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => return TestResult::Error(format!("failed to read spec file: {error}")),
    };

    let buf = match ParseBuffer::new(&source) {
        Ok(buf) => buf,
        Err(error) => return TestResult::Error(format!("failed to parse WAST buffer: {error}")),
    };

    let wast = match parser::parse::<Wast<'_>>(&buf) {
        Ok(wast) => wast,
        Err(error) => return TestResult::Error(format!("failed to parse WAST: {error}")),
    };

    let mut harness = AotSpecHarness::new();
    let (mut passed, mut failed, mut skipped, mut executed) = (0usize, 0usize, 0usize, 0usize);
    let mut errors: Vec<String> = Vec::new();

    for (index, directive) in wast.directives.into_iter().enumerate() {
        let (line, _column) = directive.span().linecol_in(&source);
        match harness.run_directive(directive) {
            Ok(Outcome::None) => executed += 1,
            Ok(Outcome::Passed) => {
                passed += 1;
                executed += 1;
            }
            Ok(Outcome::Skipped) => skipped += 1,
            Err(error) => {
                failed += 1;
                executed += 1;
                errors.push(format!(
                    "directive {} (line {}): {}",
                    index + 1,
                    line + 1,
                    error
                ));
            }
        }
    }

    // Skip ceiling: a file whose every directive was skipped provides no
    // evidence and must fail loudly instead of silently "passing" —
    // otherwise an AOT/interpreter coverage drift (or a wiring bug) would
    // hollow out the corpus unnoticed.
    if executed == 0 {
        return TestResult::Failed(format!(
            "all {skipped} directives skipped — the file exercised nothing"
        ));
    }

    if failed > 0 {
        TestResult::Failed(format!(
            "{passed} passed, {failed} failed, {skipped} skipped\n{}",
            errors.join("\n")
        ))
    } else {
        TestResult::Passed
    }
}
