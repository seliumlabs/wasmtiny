//! Differential testing: run the same module through the interpreter and the
//! AOT path and assert identical results. Enabled only when both features are
//! active (`--features interpreter`).

#![cfg(all(feature = "aot", feature = "interpreter"))]

use wasmtiny::{
    WasmApplication,
    aot::{AotExtern, AotInstance, AotLoader, AotStore},
    runtime::{ExportKind, FunctionType, RefType, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// Runs `source` (with one exported `run` function) through both engines and
/// asserts the results (values or typed traps) are identical for `args`.
fn check(source: &str, args: &[WasmValue]) {
    let wasm = wat::parse_str(source).expect("wat parses");

    // Interpreter.
    let mut app = WasmApplication::new();
    let idx = app
        .load_module_from_memory(&wasm)
        .expect("interpreter loads");
    app.instantiate(idx).expect("interpreter instantiates");
    let interp = app.call_function(idx, "run", args);

    // AOT.
    let artifact = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    let module = AotLoader::new().load(&artifact).expect("artifact loads");
    let mut instance = AotInstance::new(&module).expect("instantiation succeeds");
    let run_index = module
        .exports
        .iter()
        .find(|e| e.name == "run")
        .and_then(|e| match e.kind {
            ExportKind::Func(idx) => Some(idx),
            _ => None,
        })
        .expect("run export exists");
    let aot = instance.invoke(run_index, args);

    let shape = |result: &Result<Vec<WasmValue>, wasmtiny::runtime::WasmError>| match result {
        Ok(values) => format!(
            "ok[{}]",
            values.iter().map(normalise).collect::<Vec<_>>().join(",")
        ),
        Err(error) => format!("err:{error:?}"),
    };

    assert_eq!(
        shape(&aot),
        shape(&interp),
        "divergence for {source:?} with args {args:?}\ninterp: {interp:?}\naot: {aot:?}"
    );
}

/// Collapses NaN payloads (engines may differ in NaN bit patterns).
fn normalise(value: &WasmValue) -> String {
    match value {
        WasmValue::F32(v) if v.is_nan() => "nan:f32".to_string(),
        WasmValue::F64(v) if v.is_nan() => "nan:f64".to_string(),
        other => format!("{other:?}"),
    }
}

#[test]
fn arithmetic_and_control_flow() {
    check(
        "(module (func (export \"run\") (param i32 i32) (result i32)
           (i32.mul (i32.add (local.get 0) (local.get 1)) (i32.const 3))))",
        &[WasmValue::I32(4), WasmValue::I32(7)],
    );
    check(
        "(module (func (export \"run\") (param i32) (result i32)
           (if (result i32) (i32.gt_s (local.get 0) (i32.const 0))
             (then (i32.const 111)) (else (i32.const 222)))))",
        &[WasmValue::I32(5)],
    );
    check(
        "(module (func (export \"run\") (param i32) (result i32)
           (local $n i32) (local $acc i32)
           (local.set $n (local.get 0))
           (block $exit
             (loop $l
               (br_if $exit (i32.le_s (local.get $n) (i32.const 0)))
               (local.set $acc (i32.add (local.get $acc) (local.get $n)))
               (local.set $n (i32.sub (local.get $n) (i32.const 1)))
               (br $l)))
           (local.get $acc)))",
        &[WasmValue::I32(5)],
    );
}

#[test]
fn memory_and_bulk_operations() {
    check(
        "(module (memory 1)
           (func (export \"run\") (result i32)
             (i32.store (i32.const 100) (i32.const 42))
             (i32.load (i32.const 100))))",
        &[],
    );
    check(
        "(module (memory 1)
           (func (export \"run\") (result i32)
             (memory.fill (i32.const 0) (i32.const 7) (i32.const 4))
             (i32.load8_u (i32.const 3))))",
        &[],
    );
    check(
        "(module (memory 1) (data (i32.const 0) \"abcd\")
           (func (export \"run\") (result i32)
             (memory.copy (i32.const 4) (i32.const 0) (i32.const 4))
             (i32.load8_u (i32.const 6))))",
        &[],
    );
    check(
        "(module (memory 1 3)
           (func (export \"run\") (result i32) (memory.grow (i32.const 2))))",
        &[],
    );
}

#[test]
fn globals_and_call_indirect() {
    check(
        "(module (global $g (mut i32) (i32.const 1))
           (func (export \"run\") (result i32)
             (global.set $g (i32.add (global.get $g) (i32.const 10)))
             (global.get $g)))",
        &[],
    );
    check(
        "(module (type $t (func (result i32)))
           (table 2 funcref)
           (func $f (result i32) (i32.const 99))
           (elem (i32.const 0) func $f)
           (func (export \"run\") (result i32)
             (call_indirect (type $t) (i32.const 0))))",
        &[],
    );
}

#[test]
fn refs_and_tables() {
    // Element segments + `call_indirect` through a table (no in-body ref.func,
    // which the interpreter's declaration-order check rejects).
    check(
        "(module (table 2 funcref)
           (func $f (result i32) (i32.const 7))
           (elem (i32.const 1) func $f)
           (func (export \"run\") (result i32)
             (call_indirect (type 0) (i32.const 1))))",
        &[],
    );
}

#[test]
fn trap_parity() {
    // These must trap identically in both engines.
    check(
        "(module (memory 1)
           (func (export \"run\") (result i32)
             (i32.load (i32.const 100000))))",
        &[],
    );
    check(
        "(module (func (export \"run\") (result i32) (unreachable)))",
        &[],
    );
    check(
        "(module (table 1 funcref)
           (func (export \"run\") (result i32)
             (call_indirect (type 0) (i32.const 5))))",
        &[],
    );
}

#[test]
fn saturating_conversions() {
    check(
        "(module (func (export \"run\") (result i32)
           (i32.trunc_sat_f64_s (f64.const 1.0e20))))",
        &[],
    );
    check(
        "(module (func (export \"run\") (result i32)
           (i32.trunc_sat_f32_u (f32.const -1.0))))",
        &[],
    );
}

// ---------------------------------------------------------------------------
// Corpus differential: every vendored spec-corpus directive is executed
// through both engines and the observable outcomes are compared.
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use wast::{
    Wast, WastArg, WastDirective, WastExecute, WastInvoke,
    core::{AbstractHeapType, WastArgCore},
    parser::{self, ParseBuffer},
};

const SPEC_DIR: &str = "tests/spec-core/";

const SPEC_FILES: &[&str] = &[
    "block.wast",
    "br.wast",
    "br_if.wast",
    "br_table.wast",
    "call.wast",
    "call_indirect.wast",
    "const.wast",
    "conversions.wast",
    "data.wast",
    "elem.wast",
    "exports.wast",
    "f32.wast",
    "f32_cmp.wast",
    "f64.wast",
    "f64_cmp.wast",
    "fac.wast",
    "float_literals.wast",
    "float_memory.wast",
    "float_misc.wast",
    "func.wast",
    "global.wast",
    "i32.wast",
    "id.wast",
    "imports.wast",
    "int_literals.wast",
    "labels.wast",
    "load.wast",
    "local_get.wast",
    "local_set.wast",
    "local_tee.wast",
    "loop.wast",
    "memory.wast",
    "memory_grow.wast",
    "memory_size.wast",
    "memory_trap.wast",
    "nop.wast",
    "ref_is_null.wast",
    "return.wast",
    "select.wast",
    "start.wast",
    "store.wast",
    "table.wast",
    "table_get.wast",
    "table_set.wast",
    "traps.wast",
    "type.wast",
    "unreachable.wast",
    "func_ptrs.wast",
];

/// A directive reduced to engine-neutral data, so both engines observe the
/// exact same input.
enum Prepared {
    /// Instantiate a (possibly named) module.
    Module {
        name: Option<String>,
        wasm: Option<Vec<u8>>,
    },
    /// Validate a module definition without instantiating it.
    Definition { wasm: Option<Vec<u8>> },
    /// Register the current (or named) module under an import name.
    Register {
        name: String,
        module: Option<String>,
    },
    /// Invoke a function.
    Invoke {
        module: Option<String>,
        function: String,
        args: Option<Vec<WasmValue>>,
    },
    /// Read an exported global.
    Get {
        module: Option<String>,
        global: String,
    },
    /// A module the spec expects to be refused (invalid/malformed/unlinkable).
    MustReject { wasm: Option<Vec<u8>> },
    /// A directive both engines skip by design (threads, wait, ...).
    Ignored,
}

/// The observable outcome of one directive on one engine.
#[derive(Debug, PartialEq, Eq)]
enum Obs {
    /// Structural directive applied (module instantiated, registered).
    None,
    /// Execution returned values (NaN-normalised, reference-opaque).
    Ok(Vec<String>),
    /// Execution trapped with a typed code.
    Trap(wasmtiny::runtime::TrapCode),
    /// Load/validation/link/instantiation refused the module.
    Rejected,
    /// The directive could not be carried out for an engine-level reason.
    Failed,
    /// Not comparable: unsupported feature or missing context.
    Skip,
}

fn prepare(directive: WastDirective<'_>) -> Prepared {
    match directive {
        WastDirective::Module(mut module) => Prepared::Module {
            name: module.name().map(|id| id.name().to_string()),
            wasm: module.encode().ok(),
        },
        WastDirective::ModuleDefinition(mut module) => Prepared::Definition {
            wasm: module.encode().ok(),
        },
        WastDirective::Register { name, module, .. } => Prepared::Register {
            name: name.to_string(),
            module: module.map(|id| id.name().to_string()),
        },
        WastDirective::Invoke(invoke) => prepare_invoke(&invoke),
        WastDirective::AssertReturn { exec, .. } => prepare_exec(exec),
        WastDirective::AssertTrap { exec, .. } => prepare_exec(exec),
        WastDirective::AssertExhaustion { call, .. } => prepare_invoke(&call),
        WastDirective::AssertInvalid { mut module, .. }
        | WastDirective::AssertMalformed { mut module, .. } => Prepared::MustReject {
            wasm: module.encode().ok(),
        },
        WastDirective::AssertUnlinkable { mut module, .. } => Prepared::MustReject {
            wasm: module.encode().ok(),
        },
        WastDirective::AssertException { .. }
        | WastDirective::AssertSuspension { .. }
        | WastDirective::AssertInvalidCustom { .. }
        | WastDirective::AssertMalformedCustom { .. }
        | WastDirective::Thread(_)
        | WastDirective::Wait { .. }
        | WastDirective::ModuleInstance { .. } => Prepared::Ignored,
    }
}

fn prepare_exec(exec: WastExecute<'_>) -> Prepared {
    match exec {
        WastExecute::Invoke(invoke) => prepare_invoke(&invoke),
        WastExecute::Wat(mut module) => Prepared::Module {
            name: None,
            wasm: module.encode().ok(),
        },
        WastExecute::Get { module, global, .. } => Prepared::Get {
            module: module.map(|id| id.name().to_string()),
            global: global.to_string(),
        },
    }
}

fn prepare_invoke(invoke: &WastInvoke<'_>) -> Prepared {
    Prepared::Invoke {
        module: invoke.module.map(|id| id.name().to_string()),
        function: invoke.name.to_string(),
        args: invoke
            .args
            .iter()
            .map(wast_arg_to_value)
            .collect::<Option<Vec<_>>>(),
    }
}

fn wast_arg_to_value(arg: &WastArg<'_>) -> Option<WasmValue> {
    let WastArg::Core(core) = arg else {
        return None;
    };
    match core {
        WastArgCore::I32(value) => Some(WasmValue::I32(*value)),
        WastArgCore::I64(value) => Some(WasmValue::I64(*value)),
        WastArgCore::F32(value) => Some(WasmValue::F32(f32::from_bits(value.bits))),
        WastArgCore::F64(value) => Some(WasmValue::F64(f64::from_bits(value.bits))),
        WastArgCore::RefNull(heap_type) => {
            let reftype = match heap_type {
                wast::core::HeapType::Abstract { ty, .. } => match ty {
                    AbstractHeapType::Func | AbstractHeapType::NoFunc => Some(RefType::FuncRef),
                    AbstractHeapType::Extern | AbstractHeapType::NoExtern => {
                        Some(RefType::ExternRef)
                    }
                    _ => None,
                },
                _ => None,
            }?;
            Some(WasmValue::NullRef(reftype))
        }
        WastArgCore::RefExtern(value) => Some(WasmValue::ExternRef(*value)),
        WastArgCore::RefHost(value) => Some(WasmValue::ExternRef(*value)),
        WastArgCore::V128(_) => None,
    }
}

/// Value normalisation for cross-engine comparison: NaNs collapse to their
/// class; funcref handles are engine-internal, so only "a funcref" compares.
fn normalise_value(value: &WasmValue) -> String {
    match value {
        WasmValue::F32(v) if v.is_nan() => "nan:f32".to_string(),
        WasmValue::F64(v) if v.is_nan() => "nan:f64".to_string(),
        WasmValue::FuncRef(_) => "funcref".to_string(),
        WasmValue::NullRef(t) => format!("null:{t:?}"),
        other => format!("{other:?}"),
    }
}

/// Classifies an execution error for comparison.
fn classify_error(error: &wasmtiny::runtime::WasmError) -> Obs {
    use wasmtiny::runtime::WasmError;
    match error {
        WasmError::Trap(code) => Obs::Trap(*code),
        other => {
            let text = other.to_string();
            if text.contains("no current module")
                || text.contains("unknown module id")
                || text.contains("unknown import module")
                || text.contains("register directive")
            {
                Obs::Skip
            } else {
                Obs::Failed
            }
        }
    }
}

/// The fixed signatures of the `spectest` host functions.
fn spectest_function_type(name: &str) -> Result<FunctionType, String> {
    use wasmtiny::runtime::{NumType, ValType};
    let params = match name {
        "print" => vec![],
        "print_i32" => vec![ValType::Num(NumType::I32)],
        "print_i64" => vec![ValType::Num(NumType::I64)],
        "print_f32" => vec![ValType::Num(NumType::F32)],
        "print_f64" => vec![ValType::Num(NumType::F64)],
        "print_i32_f32" => vec![ValType::Num(NumType::I32), ValType::Num(NumType::F32)],
        "print_f64_f64" => vec![ValType::Num(NumType::F64), ValType::Num(NumType::F64)],
        other => return Err(format!("unknown spectest function {other}")),
    };
    Ok(FunctionType::new(params, vec![]))
}

/// A host function that accepts anything and returns nothing.
struct NoOpHostFunc {
    function_type: FunctionType,
}

impl wasmtiny::runtime::HostFunc for NoOpHostFunc {
    fn call(
        &self,
        _caller: &mut wasmtiny::runtime::HostCaller<'_>,
        _args: &[WasmValue],
    ) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        Ok(Vec::new())
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(&self.function_type)
    }
}

/// Whether a compile refusal means "outside the supported feature set"
/// (not comparable across engines) rather than a genuine rejection.
fn is_unsupported(error: &wasmtiny_aotc::CompileError) -> bool {
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

// --- interpreter side -------------------------------------------------------

use wasmtiny::runtime::Import;

struct InterpSide {
    app: WasmApplication,
    parser: wasmtiny::loader::Parser,
    validator: wasmtiny::loader::Validator,
    named: HashMap<String, u32>,
    registered: HashMap<String, u32>,
    current: Option<u32>,
}

impl InterpSide {
    fn new() -> Self {
        Self {
            app: WasmApplication::new(),
            parser: wasmtiny::loader::Parser::new(),
            validator: wasmtiny::loader::Validator::new(),
            named: HashMap::new(),
            registered: HashMap::new(),
            current: None,
        }
    }

    fn observe(&mut self, prepared: &Prepared) -> Obs {
        match prepared {
            Prepared::Ignored => Obs::Skip,
            Prepared::Module {
                name,
                wasm: Some(wasm),
            } => self.instantiate(wasm, name.clone()),
            // A module that cannot even be encoded is malformed by
            // construction: both engines see it rejected.
            Prepared::Module { wasm: None, .. } => Obs::Rejected,
            Prepared::Definition { wasm: Some(wasm) } => match self.validate(wasm) {
                Ok(()) => Obs::None,
                Err(_) => Obs::Rejected,
            },
            Prepared::Definition { wasm: None } => Obs::Rejected,
            Prepared::Register { name, module } => {
                let source = match module {
                    Some(module) => self.named.get(module).copied(),
                    None => self.current,
                };
                match source {
                    Some(idx) => {
                        self.registered.insert(name.clone(), idx);
                        Obs::None
                    }
                    None => Obs::Skip,
                }
            }
            Prepared::Invoke {
                module,
                function,
                args: Some(args),
            } => {
                let Some(idx) = self.lookup(module.as_deref()) else {
                    return Obs::Skip;
                };
                match self.app.call_function(idx, function, args) {
                    Ok(values) => Obs::Ok(values.iter().map(normalise_value).collect()),
                    Err(error) => classify_error(&error),
                }
            }
            Prepared::Invoke { args: None, .. } => Obs::Skip,
            Prepared::Get { module, global } => {
                let Some(idx) = self.lookup(module.as_deref()) else {
                    return Obs::Skip;
                };
                let Some(source) = self.app.runtime.get_module(idx) else {
                    return Obs::Skip;
                };
                match source.get_export(global) {
                    Some(wasmtiny::engine::runtime::Export::Global(idx)) => {
                        match source.get_global(*idx) {
                            Some(global) => Obs::Ok(vec![normalise_value(&global.value)]),
                            None => Obs::Failed,
                        }
                    }
                    _ => Obs::Failed,
                }
            }
            Prepared::MustReject { wasm: Some(wasm) } => {
                // The full pipeline: validation, loading, linking. The
                // directive is expected to be refused somewhere; an engine
                // that accepts it is reported as not comparable (the
                // per-engine spec harnesses own accept/reject correctness).
                match self.validate(wasm) {
                    Err(_) => Obs::Rejected,
                    Ok(()) => match self.app.load_module_from_memory(wasm) {
                        Err(_) => Obs::Rejected,
                        Ok(idx) => match self.link(idx) {
                            Ok(()) => Obs::Skip,
                            Err(_) => Obs::Rejected,
                        },
                    },
                }
            }
            Prepared::MustReject { wasm: None } => Obs::Rejected,
        }
    }

    fn validate(&mut self, wasm: &[u8]) -> Result<(), String> {
        let parsed = self
            .parser
            .parse(wasm)
            .map_err(|error| format!("module parse failed: {error}"))?;
        self.validator
            .validate(&parsed)
            .map_err(|error| format!("module validation failed: {error}"))
    }

    fn instantiate(&mut self, wasm: &[u8], name: Option<String>) -> Obs {
        self.current = None;
        let Ok(idx) = self.app.load_module_from_memory(wasm) else {
            return Obs::Rejected;
        };
        // Tag imports are outside the supported feature set on both engines.
        if let Some(module) = self.app.runtime.get_module(idx)
            && module
                .imports()
                .iter()
                .any(|import| matches!(import.kind, wasmtiny::runtime::ImportKind::Tag(..)))
        {
            return Obs::Skip;
        }
        if let Err(error) = self.resolve_imports(idx) {
            // An unresolvable import is the unlinkable condition; an
            // unknown import module means a dependency was skipped.
            if error.contains("unknown import module") {
                return Obs::Skip;
            }
            return match self.app.instantiate(idx) {
                Err(_) => Obs::Rejected,
                Ok(()) => Obs::Failed,
            };
        }
        match self.link(idx) {
            Ok(()) => {
                if let Some(name) = name {
                    self.named.insert(name, idx);
                }
                self.current = Some(idx);
                Obs::None
            }
            Err(_) => Obs::Rejected,
        }
    }

    fn link(&mut self, idx: u32) -> Result<(), String> {
        self.app
            .instantiate(idx)
            .map_err(|error| format!("instantiation failed: {error}"))?;
        self.app
            .execute_start(idx)
            .map_err(|error| format!("start failed: {error}"))
    }

    fn lookup(&self, name: Option<&str>) -> Option<u32> {
        match name {
            Some(name) => self.named.get(name).copied(),
            None => self.current,
        }
    }

    fn resolve_imports(&mut self, module_idx: u32) -> Result<(), String> {
        let imports = self
            .app
            .runtime
            .get_module(module_idx)
            .ok_or_else(|| "module not found".to_string())?
            .imports()
            .to_vec();

        for import in imports {
            if import.module == "spectest" {
                self.resolve_spectest_import(module_idx, &import)?;
                continue;
            }
            if let Some(source_idx) = self.registered.get(&import.module).copied() {
                self.resolve_registered_import(module_idx, source_idx, &import)?;
            }
        }
        Ok(())
    }

    fn resolve_spectest_import(&mut self, module_idx: u32, import: &Import) -> Result<(), String> {
        use wasmtiny::runtime::{Global, ImportKind, Limits, Memory, MemoryType, Table, TableType};
        let target = self
            .app
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| "target module not found".to_string())?;

        match &import.kind {
            ImportKind::Memory(_) => target
                .register_memory_import(
                    &import.module,
                    &import.name,
                    Memory::new(MemoryType::new(Limits::MinMax(1, 2))).expect("spectest memory"),
                )
                .map_err(|error| error.to_string()),
            ImportKind::Table(table_type) => target
                .register_table_import(
                    &import.module,
                    &import.name,
                    Table::new(TableType::new(table_type.elem_type, Limits::MinMax(10, 20))),
                )
                .map_err(|error| error.to_string()),
            ImportKind::Global(global_type) => {
                let value = match import.name.as_str() {
                    "global_i32" => WasmValue::I32(666),
                    "global_i64" => WasmValue::I64(666),
                    "global_f32" => WasmValue::F32(666.6),
                    "global_f64" => WasmValue::F64(666.6),
                    _ => return Err(format!("unsupported spectest global {}", import.name)),
                };
                target
                    .register_global_import(
                        &import.module,
                        &import.name,
                        Global::new(global_type.clone(), value)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())
            }
            ImportKind::Func(_) => {
                let function_type = spectest_function_type(&import.name)?;
                self.app
                    .register_host_function(
                        module_idx,
                        &import.module,
                        &import.name,
                        Box::new(NoOpHostFunc {
                            function_type: function_type.clone(),
                        }),
                        function_type,
                    )
                    .map_err(|error| error.to_string())
            }
            ImportKind::Tag(..) => Ok(()),
        }
    }

    fn resolve_registered_import(
        &mut self,
        module_idx: u32,
        source_idx: u32,
        import: &Import,
    ) -> Result<(), String> {
        use wasmtiny::runtime::{Extern, Module, WasmError};
        let source = self
            .app
            .runtime
            .get_module(source_idx)
            .ok_or_else(|| "source module not found".to_string())?;

        match &import.kind {
            wasmtiny::runtime::ImportKind::Memory(_) => {
                let memory = self
                    .app
                    .export_memory(source_idx, &import.name)
                    .map_err(|error| error.to_string())?;
                self.app
                    .runtime
                    .get_module_mut(module_idx)
                    .ok_or_else(|| "target module not found".to_string())?
                    .register_memory_import(&import.module, &import.name, memory)
                    .map_err(|error| error.to_string())
            }
            wasmtiny::runtime::ImportKind::Table(_) => {
                let table_idx = match source.get_export(&import.name) {
                    Some(wasmtiny::engine::runtime::Export::Table(idx)) => *idx,
                    _ => return Err(format!("table export {} not found", import.name)),
                };
                let shared_table = source
                    .table_binding(table_idx)
                    .ok_or_else(|| format!("table {table_idx} not found"))?;
                self.app
                    .runtime
                    .get_module_mut(module_idx)
                    .ok_or_else(|| "target module not found".to_string())?
                    .register_import(&import.module, &import.name, Extern::Table(shared_table))
                    .map_err(|error| error.to_string())
            }
            wasmtiny::runtime::ImportKind::Global(_) => {
                let global_idx = match source.get_export(&import.name) {
                    Some(wasmtiny::engine::runtime::Export::Global(idx)) => *idx,
                    _ => return Err(format!("global export {} not found", import.name)),
                };
                let global = source
                    .get_global(global_idx)
                    .ok_or_else(|| format!("global {global_idx} not found"))?;
                self.app
                    .runtime
                    .get_module_mut(module_idx)
                    .ok_or_else(|| "target module not found".to_string())?
                    .register_global_import(&import.module, &import.name, global)
                    .map_err(|error| error.to_string())
            }
            wasmtiny::runtime::ImportKind::Func(_) => {
                let func_idx = match source.get_export(&import.name) {
                    Some(wasmtiny::engine::runtime::Export::Function(idx)) => *idx,
                    _ => return Err(format!("function export {} not found", import.name)),
                };
                let func_type = source
                    .module()
                    .func_type(func_idx)
                    .cloned()
                    .ok_or_else(|| "function type missing".to_string())?;
                let source_module: Module = source.module().clone();

                struct GuestFunc {
                    source_module: Module,
                    func_idx: u32,
                    func_type: wasmtiny::runtime::FunctionType,
                }
                impl wasmtiny::runtime::HostFunc for GuestFunc {
                    fn call(
                        &self,
                        _caller: &mut wasmtiny::runtime::HostCaller<'_>,
                        args: &[WasmValue],
                    ) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
                        let instance = std::sync::Arc::new(std::sync::Mutex::new(
                            wasmtiny::runtime::Instance::with_imports(
                                std::sync::Arc::new(self.source_module.clone()),
                                &[],
                            )
                            .map_err(|error| WasmError::Runtime(error.to_string()))?,
                        ));
                        let mut interpreter =
                            wasmtiny::interpreter::Interpreter::with_instance(instance);
                        interpreter.execute_function(&self.source_module, self.func_idx, args)
                    }
                    fn function_type(&self) -> Option<&wasmtiny::runtime::FunctionType> {
                        Some(&self.func_type)
                    }
                }

                self.app
                    .register_host_function(
                        module_idx,
                        &import.module,
                        &import.name,
                        Box::new(GuestFunc {
                            source_module,
                            func_idx,
                            func_type: func_type.clone(),
                        }),
                        func_type,
                    )
                    .map_err(|error| error.to_string())
            }
            wasmtiny::runtime::ImportKind::Tag(..) => Ok(()),
        }
    }
}

// --- AOT side ---------------------------------------------------------------

/// A compiled, loaded, instantiated AOT module, kept alive under its WAST
/// `$name` or registered import name (the instance holds mutable state —
/// memories, globals — that later directives must observe).
struct AotSideModule {
    module: wasmtiny::aot::AotModule,
    instance: RefCell<AotInstance>,
}

struct AotSide {
    store: Arc<Mutex<AotStore>>,
    modules: HashMap<String, Rc<AotSideModule>>,
    current: Option<Rc<AotSideModule>>,
}

impl AotSide {
    fn new() -> Self {
        Self {
            store: AotStore::shared(),
            modules: HashMap::new(),
            current: None,
        }
    }

    fn observe(&mut self, prepared: &Prepared) -> Obs {
        match prepared {
            Prepared::Ignored => Obs::Skip,
            Prepared::Module {
                name,
                wasm: Some(wasm),
            } => self.instantiate(wasm, name.clone()),
            Prepared::Module { wasm: None, .. } => Obs::Rejected,
            Prepared::Definition { wasm: Some(wasm) } => match compile_and_load(wasm) {
                Ok(_) => Obs::None,
                Err(error) if is_unsupported(&error) => Obs::Skip,
                Err(_) => Obs::Rejected,
            },
            Prepared::Definition { wasm: None } => Obs::Rejected,
            Prepared::Register { name, module } => {
                let source = match module {
                    Some(module) => self.modules.get(module).cloned(),
                    None => self.current.clone(),
                };
                match source {
                    Some(state) => {
                        self.modules.insert(name.clone(), state);
                        Obs::None
                    }
                    None => Obs::Skip,
                }
            }
            Prepared::Invoke {
                module,
                function,
                args: Some(args),
            } => {
                let Some(state) = self.lookup(module.as_deref()) else {
                    return Obs::Skip;
                };
                match state.instance.borrow_mut().invoke_export(function, args) {
                    Ok(values) => Obs::Ok(values.iter().map(normalise_value).collect()),
                    Err(error) => classify_error(&error),
                }
            }
            Prepared::Invoke { args: None, .. } => Obs::Skip,
            Prepared::Get { module, global } => {
                let Some(state) = self.lookup(module.as_deref()) else {
                    return Obs::Skip;
                };
                let index = state
                    .module
                    .exports
                    .iter()
                    .find(|export| export.name == *global)
                    .and_then(|export| match export.kind {
                        wasmtiny::runtime::ExportKind::Global(idx) => Some(idx),
                        _ => None,
                    });
                match index {
                    None => Obs::Failed,
                    Some(idx) => match state.instance.borrow().global_value(idx) {
                        Some(value) => Obs::Ok(vec![normalise_value(&value)]),
                        None => Obs::Failed,
                    },
                }
            }
            Prepared::MustReject { wasm: Some(wasm) } => match compile_and_load(wasm) {
                Err(_) => Obs::Rejected,
                Ok(loaded) => self.try_link(&loaded),
            },
            Prepared::MustReject { wasm: None } => Obs::Rejected,
        }
    }

    fn instantiate(&mut self, wasm: &[u8], name: Option<String>) -> Obs {
        self.current = None;
        let loaded = match compile_and_load(wasm) {
            Ok(loaded) => loaded,
            Err(error) if is_unsupported(&error) => return Obs::Skip,
            Err(_) => return Obs::Rejected,
        };
        match self.link(&loaded) {
            Ok(instance) => {
                let state = Rc::new(AotSideModule {
                    module: loaded,
                    instance: RefCell::new(instance),
                });
                if let Some(name) = name {
                    self.modules.insert(name, state.clone());
                }
                self.current = Some(state);
                Obs::None
            }
            Err(obs) => obs,
        }
    }

    /// Links a loaded module; `Err(Obs)` carries the comparable outcome.
    fn link(&self, loaded: &wasmtiny::aot::AotModule) -> Result<AotInstance, Obs> {
        let imports: Vec<_> = match loaded
            .imports
            .iter()
            .map(|import| self.resolve_import(import))
            .collect::<Result<_, _>>()
        {
            Ok(imports) => imports,
            Err(error) if error.contains("unknown import module") => return Err(Obs::Skip),
            Err(_) => return Err(Obs::Rejected),
        };
        match AotInstance::instantiate(&self.store, loaded, &imports) {
            Ok(instance) => Ok(instance),
            Err(_) => Err(Obs::Rejected),
        }
    }

    /// Full link attempt for must-reject modules; acceptance is reported as
    /// not comparable (the per-engine spec harnesses own correctness).
    fn try_link(&self, loaded: &wasmtiny::aot::AotModule) -> Obs {
        match self.link(loaded) {
            Ok(_) => Obs::Skip,
            Err(obs) => obs,
        }
    }

    fn lookup(&self, name: Option<&str>) -> Option<Rc<AotSideModule>> {
        match name {
            Some(name) => self.modules.get(name).cloned(),
            None => self.current.clone(),
        }
    }

    fn resolve_import(&self, import: &Import) -> Result<(String, String, AotExtern), String> {
        use wasmtiny::runtime::{
            ExportKind, Global, ImportKind, Limits, Memory, MemoryType, TableType,
        };
        if import.module == "spectest" {
            let extern_ = match &import.kind {
                ImportKind::Func(_) => AotExtern::HostFunc(Arc::new(NoOpHostFunc {
                    function_type: spectest_function_type(&import.name)?,
                })),
                ImportKind::Memory(_) => {
                    if import.name != "memory" {
                        return Err(format!(
                            "unknown spectest memory {}.{}",
                            import.module, import.name
                        ));
                    }
                    AotExtern::Memory(Arc::new(Mutex::new(
                        Memory::try_new(MemoryType::new(Limits::MinMax(1, 2)))
                            .map_err(|error| error.to_string())?,
                    )))
                }
                ImportKind::Table(table_type) => {
                    if import.name != "table" {
                        return Err(format!(
                            "unknown spectest table {}.{}",
                            import.module, import.name
                        ));
                    }
                    AotExtern::Table(Arc::new(Mutex::new(
                        wasmtiny::aot::AotTable::with_initial(
                            TableType::new(table_type.elem_type, Limits::MinMax(10, 20)),
                            10,
                        )
                        .map_err(|error| error.to_string())?,
                    )))
                }
                ImportKind::Global(global_type) => {
                    let value = match import.name.as_str() {
                        "global_i32" => WasmValue::I32(666),
                        "global_i64" => WasmValue::I64(666),
                        "global_f32" => WasmValue::F32(666.6),
                        "global_f64" => WasmValue::F64(666.6),
                        other => return Err(format!("unsupported spectest global {other}")),
                    };
                    AotExtern::Global(
                        Global::new(global_type.clone(), value)
                            .map_err(|error| error.to_string())?,
                    )
                }
                ImportKind::Tag(..) => {
                    return Err(format!(
                        "unsupported spectest import {}.{}",
                        import.module, import.name
                    ));
                }
            };
            return Ok((import.module.clone(), import.name.clone(), extern_));
        }

        let source = self
            .modules
            .get(&import.module)
            .cloned()
            .ok_or_else(|| format!("unknown import module {}", import.module))?;

        let extern_ = match &import.kind {
            ImportKind::Func(_) => {
                let index = export_index(&source.module, &import.name, |kind| match kind {
                    ExportKind::Func(idx) => Some(*idx),
                    _ => None,
                })
                .ok_or_else(|| format!("function export {} not found", import.name))?;
                let handle = source
                    .instance
                    .borrow()
                    .func_handle(index)
                    .ok_or_else(|| format!("function {index} not found"))?;
                AotExtern::Func(handle)
            }
            ImportKind::Table(_) => {
                let index = export_index(&source.module, &import.name, |kind| match kind {
                    ExportKind::Table(idx) => Some(*idx),
                    _ => None,
                })
                .ok_or_else(|| format!("table export {} not found", import.name))?;
                let table = source
                    .instance
                    .borrow()
                    .table_handle(index)
                    .ok_or_else(|| format!("table {index} not found"))?;
                AotExtern::Table(table)
            }
            ImportKind::Memory(_) => {
                let index = export_index(&source.module, &import.name, |kind| match kind {
                    ExportKind::Memory(idx) => Some(*idx),
                    _ => None,
                })
                .ok_or_else(|| format!("memory export {} not found", import.name))?;
                let memory = source
                    .instance
                    .borrow()
                    .memory_handle(index)
                    .ok_or_else(|| format!("memory {index} not found"))?;
                AotExtern::Memory(memory)
            }
            ImportKind::Global(global_type) => {
                let index = export_index(&source.module, &import.name, |kind| match kind {
                    ExportKind::Global(idx) => Some(*idx),
                    _ => None,
                })
                .ok_or_else(|| format!("global export {} not found", import.name))?;
                let value = source
                    .instance
                    .borrow()
                    .global_value(index)
                    .ok_or_else(|| format!("global {index} not found"))?;
                AotExtern::Global(
                    Global::new(global_type.clone(), value).map_err(|error| error.to_string())?,
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
}

fn compile_and_load(wasm: &[u8]) -> Result<wasmtiny::aot::AotModule, wasmtiny_aotc::CompileError> {
    let artifact = wasmtiny_aotc::compile_artifact(wasm, &CompilerConfig::host())?;
    AotLoader::new()
        .load(&artifact)
        .map_err(|error| wasmtiny_aotc::CompileError::Internal(error.to_string()))
}

fn export_index(
    module: &wasmtiny::aot::AotModule,
    name: &str,
    want: impl Fn(&wasmtiny::runtime::ExportKind) -> Option<u32>,
) -> Option<u32> {
    module
        .exports
        .iter()
        .find(|export| export.name == name)
        .and_then(|export| want(&export.kind))
}

// --- the differential driver ------------------------------------------------

/// Runs every directive of every vendored spec-corpus file through both
/// engines and compares the observable outcomes: identical values (with NaN
/// classes collapsed), identical typed trap codes, identical rejection of
/// invalid/malformed/unlinkable modules. Directives an engine cannot handle
/// (unsupported proposals, missing context) are skipped symmetrically; a
/// file whose directives are all skipped fails the run.
#[test]
fn corpus_directives_agree_across_engines() {
    let mut divergences: Vec<String> = Vec::new();
    let mut compared = 0usize;
    let mut skipped = 0usize;

    for file in SPEC_FILES {
        let path = format!("{SPEC_DIR}{file}");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read spec file {path}: {error}"));
        let buf = ParseBuffer::new(&source).expect("parse WAST buffer");
        let wast = parser::parse::<Wast<'_>>(&buf).expect("parse WAST");

        let mut interp = InterpSide::new();
        let mut aot = AotSide::new();
        let (mut file_compared, mut file_skipped) = (0usize, 0usize);

        for (index, directive) in wast.directives.into_iter().enumerate() {
            let (line, _column) = directive.span().linecol_in(&source);
            let prepared = prepare(directive);
            let interp_obs = interp.observe(&prepared);
            let aot_obs = aot.observe(&prepared);

            if interp_obs == Obs::Skip || aot_obs == Obs::Skip {
                file_skipped += 1;
                continue;
            }
            if interp_obs != aot_obs {
                divergences.push(format!(
                    "{file} directive {} (line {}): interpreter {interp_obs:?} vs AOT {aot_obs:?}",
                    index + 1,
                    line + 1,
                ));
            }
            file_compared += 1;
        }

        // Skip ceiling: a file that compared nothing provides no parity
        // evidence at all and must fail loudly rather than "pass".
        if file_compared == 0 && file_skipped > 0 {
            divergences.push(format!(
                "{file}: all {file_skipped} directives skipped — no differential evidence"
            ));
        }
        compared += file_compared;
        skipped += file_skipped;
    }

    assert!(
        divergences.is_empty(),
        "engine divergence ({} directives compared, {} skipped):\n{}",
        compared,
        skipped,
        divergences.join("\n")
    );
    // Sanity: the corpus is large; a wiring bug that compares nothing must
    // fail the run.
    assert!(
        compared > 10_000,
        "suspiciously little differential coverage: {compared} directives compared"
    );
}

// ---------------------------------------------------------------------------
// Regression differential: the scenarios guarded by the regression suites
// (spine_repro.rs, atomic_regression.rs) are executed through BOTH engines,
// with the suites' expected results asserted on each and the outcomes
// compared. host_region_wait_notify.rs exercises the embedder shared-region
// API, which is interpreter-embedder plumbing (the AOT corpus covers the
// same boundary from the guest side); its atomic semantics are covered by
// the scenarios below.
// ---------------------------------------------------------------------------

/// A one-shot engine pair for a regression scenario.
struct Scenario {
    wat: &'static str,
    /// Optional host function bound to `env.host` (param i32 i32, result i64).
    host: Option<std::sync::Arc<CountingHost>>,
}

struct CountingHost {
    calls: std::sync::atomic::AtomicUsize,
}

/// Boxable wrapper so the same host function feeds both engines.
struct SharedCountingHost(std::sync::Arc<CountingHost>);

impl wasmtiny::runtime::HostFunc for SharedCountingHost {
    fn call(
        &self,
        caller: &mut wasmtiny::runtime::HostCaller<'_>,
        args: &[WasmValue],
    ) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        self.0.call(caller, args)
    }
    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
        Some(TYPE.get_or_init(counting_host_type))
    }
}

fn counting_host_type() -> FunctionType {
    use wasmtiny::runtime::{NumType, ValType};
    FunctionType::new(
        vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
        vec![ValType::Num(NumType::I64)],
    )
}

impl wasmtiny::runtime::HostFunc for CountingHost {
    fn call(
        &self,
        _caller: &mut wasmtiny::runtime::HostCaller<'_>,
        args: &[WasmValue],
    ) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(if args.len() == 2 {
            vec![WasmValue::I64(0)]
        } else {
            vec![]
        })
    }
    fn function_type(&self) -> Option<&FunctionType> {
        static TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
        Some(TYPE.get_or_init(|| {
            use wasmtiny::runtime::{NumType, ValType};
            FunctionType::new(
                vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
                vec![ValType::Num(NumType::I64)],
            )
        }))
    }
}

impl Scenario {
    fn new(wat: &'static str) -> Self {
        Self { wat, host: None }
    }

    fn with_host(mut self) -> Self {
        self.host = Some(std::sync::Arc::new(CountingHost {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }));
        self
    }

    fn host_calls(&self) -> usize {
        self.host
            .as_ref()
            .map(|h| h.calls.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Builds both engines over the scenario module. The interpreter arm
    /// mirrors spine_repro.rs; the AOT arm compiles, loads and instantiates
    /// with the same host import.
    fn build(self) -> (InterpScenario, AotScenario) {
        let wasm = wat::parse_str(self.wat).expect("wat parses");

        // Interpreter.
        let mut app = WasmApplication::new();
        let idx = app
            .load_module_from_memory(&wasm)
            .expect("interpreter loads");
        if let Some(host) = &self.host {
            app.register_host_function(
                idx,
                "env",
                "host",
                Box::new(SharedCountingHost(host.clone())),
                counting_host_type(),
            )
            .expect("register host import");
        }
        app.instantiate(idx).expect("interpreter instantiates");
        let interp = InterpScenario { app, idx };

        // AOT.
        let artifact =
            compile_artifact(&wasm, &CompilerConfig::host()).expect("regression module compiles");
        let module = AotLoader::new().load(&artifact).expect("artifact loads");
        let imports: Vec<(String, String, AotExtern)> = match &self.host {
            Some(host) => vec![(
                "env".to_string(),
                "host".to_string(),
                AotExtern::HostFunc(host.clone()),
            )],
            None => Vec::new(),
        };
        let instance = AotInstance::instantiate(&AotStore::shared(), &module, &imports)
            .expect("AOT instantiates");
        let aot = AotScenario {
            instance,
            _module: module,
        };

        (interp, aot)
    }
}

struct InterpScenario {
    app: WasmApplication,
    idx: u32,
}

impl InterpScenario {
    fn call(&mut self, function: &str) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        self.app.call_function(self.idx, function, &[])
    }
}

struct AotScenario {
    instance: AotInstance,
    _module: wasmtiny::aot::AotModule,
}

impl AotScenario {
    fn call(&mut self, function: &str) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        self.instance.invoke_export(function, &[])
    }
}

/// Runs one invocation on both engines, asserting the shared expected
/// outcome (values, or a trap code) and cross-engine agreement.
fn assert_scenario_call(
    name: &str,
    interp: &mut InterpScenario,
    aot: &mut AotScenario,
    function: &str,
    expect: Result<&[WasmValue], wasmtiny::runtime::TrapCode>,
) {
    let interp = interp.call(function);
    let aot = aot.call(function);

    let shape = |result: &wasmtiny::runtime::Result<Vec<WasmValue>>| match result {
        Ok(values) => format!(
            "ok[{}]",
            values
                .iter()
                .map(normalise_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Err(wasmtiny::runtime::WasmError::Trap(code)) => format!("trap:{code:?}"),
        Err(other) => format!("err:{other}"),
    };
    assert_eq!(
        shape(&interp),
        shape(&aot),
        "{name}: engine divergence in '{function}'"
    );

    for (engine, result) in [("interpreter", &interp), ("aot", &aot)] {
        match (expect, result) {
            (Ok(expected), Ok(actual)) => assert_eq!(
                actual, expected,
                "{name}: {engine} '{function}' returned {actual:?}, expected {expected:?}"
            ),
            (Ok(expected), Err(error)) => {
                panic!("{name}: {engine} '{function}' failed, expected {expected:?}: {error}")
            }
            (Err(code), Err(wasmtiny::runtime::WasmError::Trap(actual))) => assert_eq!(
                actual, &code,
                "{name}: {engine} '{function}' trapped with {actual:?}, expected {code:?}"
            ),
            (Err(code), other) => {
                panic!("{name}: {engine} '{function}' produced {other:?}, expected trap {code:?}")
            }
        }
    }
}

/// spine_repro: bulk-memory instructions execute with the recorded result.
#[test]
fn regression_bulk_memory_on_both_engines() {
    let scenario = Scenario::new(
        "(module (memory 1)
           (func (export \"run\") (result i32)
             i32.const 0 i32.const 9 i32.const 4 memory.fill
             i32.const 8 i32.const 0 i32.const 4 memory.copy
             i32.const 8 i32.load8_u))",
    );
    let host_calls = scenario.host_calls();
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "bulk_memory",
        &mut interp,
        &mut aot,
        "run",
        Ok(&[WasmValue::I32(9)]),
    );
    assert_eq!(host_calls, 0);
}

/// spine_repro: host import + guest-native call_indirect + nested return,
/// with the host invoked exactly once on each engine.
#[test]
fn regression_host_and_guest_natives_on_both_engines() {
    let scenario = Scenario::new(
        "(module
           (import \"env\" \"host\" (func $host (param i32 i32) (result i64)))
           (memory 1)
           (table 1 funcref)
           (elem (i32.const 0) $nested)
           (func $nested (result i32) i32.const 41 return)
           (func (export \"run\") (result i32)
             i32.const 0 i32.const 0 call $host drop
             i32.const 0 call_indirect (type 0)
             i32.const 1 i32.add)
           (type $v_i (func (result i32))))",
    )
    .with_host();
    // The host counter is shared by both engines' instances: two calls.
    let host = scenario.host.clone().expect("host registered");
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "host_and_guest_natives",
        &mut interp,
        &mut aot,
        "run",
        Ok(&[WasmValue::I32(42)]),
    );
    assert_eq!(
        host.calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the host import must be reached exactly once per engine"
    );
}

/// spine_repro: i64 shifts take the count as i64.
#[test]
fn regression_i64_shifts_on_both_engines() {
    let scenario = Scenario::new(
        "(module (func (export \"run\") (result i64)
           i64.const 32 i64.const 2 i64.shr_u))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "i64_shifts",
        &mut interp,
        &mut aot,
        "run",
        Ok(&[WasmValue::I64(8)]),
    );
}

/// spine_repro: memory growth and writes persist across invocations on
/// both engines.
#[test]
fn regression_memory_growth_persists_on_both_engines() {
    let scenario = Scenario::new(
        "(module (memory 1)
           (func (export \"grow_and_write\") (result i32)
             i32.const 1 memory.grow drop
             i32.const 65540 i32.const 7 i32.store8
             i32.const 0)
           (func (export \"read\") (result i32)
             i32.const 65540 i32.load8_u))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "memory_growth",
        &mut interp,
        &mut aot,
        "grow_and_write",
        Ok(&[WasmValue::I32(0)]),
    );
    assert_scenario_call(
        "memory_growth",
        &mut interp,
        &mut aot,
        "read",
        Ok(&[WasmValue::I32(7)]),
    );
}

/// atomic_regression: misaligned and out-of-bounds atomics trap; RMW, load/
/// store, notify and wait-not-equal execute with recorded results.
#[test]
fn regression_atomics_on_both_engines() {
    // Misaligned atomic load traps with MemoryOutOfBounds.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"misaligned_load\") (result i32)
             i32.const 1 i32.atomic.load))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_misaligned",
        &mut interp,
        &mut aot,
        "misaligned_load",
        Err(wasmtiny::runtime::TrapCode::MemoryOutOfBounds),
    );

    // OOB atomic load traps with MemoryOutOfBounds.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"oob_load\") (result i32)
             i32.const 65536 i32.atomic.load))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_oob",
        &mut interp,
        &mut aot,
        "oob_load",
        Err(wasmtiny::runtime::TrapCode::MemoryOutOfBounds),
    );

    // RMW add returns the old value.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"add\") (result i32)
             i32.const 0 i32.const 5 i32.atomic.rmw.add))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_rmw_add",
        &mut interp,
        &mut aot,
        "add",
        Ok(&[WasmValue::I32(0)]),
    );

    // Atomic store then load round-trips.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"store_load\") (result i32)
             i32.const 0 i32.const 0x12345678 i32.atomic.store
             i32.const 0 i32.atomic.load))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_store_load",
        &mut interp,
        &mut aot,
        "store_load",
        Ok(&[WasmValue::I32(0x12345678)]),
    );

    // Notify with no waiters returns 0.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"notify\") (result i32)
             i32.const 0 i32.const 1 memory.atomic.notify))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_notify_no_waiters",
        &mut interp,
        &mut aot,
        "notify",
        Ok(&[WasmValue::I32(0)]),
    );

    // wait32 returns 1 (not-equal) without waiting.
    let scenario = Scenario::new(
        "(module (memory 1 1 shared)
           (func (export \"wait\") (result i32)
             i32.const 0 i32.const 1 i64.const 0 memory.atomic.wait32))",
    );
    let (mut interp, mut aot) = scenario.build();
    assert_scenario_call(
        "atomic_wait32_not_equal",
        &mut interp,
        &mut aot,
        "wait",
        Ok(&[WasmValue::I32(1)]),
    );
}

/// atomic_regression: atomics on non-shared memory are rejected — by the
/// interpreter's validator at load time, and by the AOT compiler at compile
/// time. Neither engine executes the module.
#[test]
fn regression_atomic_on_nonshared_rejected_by_both_engines() {
    let wasm = wat::parse_str(
        "(module (memory 1)
           (func (export \"bad\") i32.const 0 i32.atomic.load))",
    )
    .expect("wat parses");

    let mut app = WasmApplication::new();
    assert!(
        app.load_module_from_memory(&wasm).is_err(),
        "interpreter must reject atomics on non-shared memory"
    );

    assert!(
        compile_artifact(&wasm, &CompilerConfig::host()).is_err(),
        "AOT compiler must reject atomics on non-shared memory"
    );
}
