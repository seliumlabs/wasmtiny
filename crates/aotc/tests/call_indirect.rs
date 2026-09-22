//! `call_indirect` and cross-module dispatch through shared AOT state.

use wasmtiny::{
    aot::{AotExtern, AotInstance, AotLoader, AotStore},
    runtime::WasmValue,
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

#[test]
fn call_indirect_dispatches_through_a_table() {
    let module = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (func $inc (type $t) (param i32) (result i32)
             (i32.add (local.get 0) (i32.const 1)))
           (func $dec (type $t) (param i32) (result i32)
             (i32.sub (local.get 0) (i32.const 1)))
           (table 2 funcref)
           (elem (i32.const 0) $inc $dec)
           (func (export \"callat\") (param i32 i32) (result i32)
             (call_indirect (type $t) (local.get 1) (local.get 0))))",
    );

    let mut instance = AotInstance::new(&module).expect("instantiation");

    // `callat(index, value)`. Function index 0 = inc, 1 = dec.
    let results = instance
        .invoke(2, &[WasmValue::I32(0), WasmValue::I32(41)])
        .expect("inc via table");
    assert_eq!(results, vec![WasmValue::I32(42)]);

    let results = instance
        .invoke(2, &[WasmValue::I32(1), WasmValue::I32(41)])
        .expect("dec via table");
    assert_eq!(results, vec![WasmValue::I32(40)]);
}

#[test]
fn cross_module_function_import_dispatches_to_provider() {
    let module_a = load(
        "(module (func (export \"inc\") (param i32) (result i32)
           (i32.add (local.get 0) (i32.const 1))))",
    );
    let module_b = load(
        "(module
           (import \"a\" \"inc\" (func $inc (param i32) (result i32)))
           (func (export \"main\") (param i32) (result i32)
             (call $inc (local.get 0))))",
    );

    let store = AotStore::shared();
    let a = AotInstance::instantiate(&store, &module_a, &[]).expect("A instantiates");
    let handle = a.func_handle(0).expect("inc has a store handle");

    let imports = [("a".to_string(), "inc".to_string(), AotExtern::Func(handle))];
    let mut b = AotInstance::instantiate(&store, &module_b, &imports).expect("B instantiates");

    let results = b
        .invoke(1, &[WasmValue::I32(41)])
        .expect("cross-module call");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}

fn load(source: &str) -> wasmtiny::aot::AotModule {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

#[test]
fn shared_imported_table_dispatches_across_modules() {
    let module_a = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (func $inc (type $t) (param i32) (result i32)
             (i32.add (local.get 0) (i32.const 10)))
           (table (export \"tab\") 2 funcref)
           (elem (i32.const 0) $inc))",
    );
    let module_b = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (import \"a\" \"tab\" (table 2 funcref))
           (func (export \"callat\") (param i32 i32) (result i32)
             (call_indirect (type $t) (local.get 1) (local.get 0))))",
    );

    let store = AotStore::shared();
    let a = AotInstance::instantiate(&store, &module_a, &[]).expect("A instantiates");
    let table = a.table_handle(0).expect("A has a shared table");

    let imports = [("a".to_string(), "tab".to_string(), AotExtern::Table(table))];
    let mut b = AotInstance::instantiate(&store, &module_b, &imports).expect("B instantiates");

    // Calls A's `$inc` through A's own vmctx, via the shared imported table.
    let results = b
        .invoke(0, &[WasmValue::I32(0), WasmValue::I32(32)])
        .expect("cross-module indirect call");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}

/// Regression: a shared imported table grown by one instance must be
/// usable through another instance — the growth is published to the shared
/// cells holder, not to a per-instance snapshot.
#[test]
fn shared_table_growth_is_visible_across_instances() {
    let module_a = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (func $inc (type $t) (param i32) (result i32)
             (i32.add (local.get 0) (i32.const 10)))
           (table (export \"tab\") 1 funcref)
           (elem (i32.const 0) $inc)
           (func (export \"grow\") (param i32) (result i32)
             (table.grow 0 (ref.func $inc) (local.get 0))))",
    );
    let module_b = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (import \"a\" \"tab\" (table 1 funcref))
           (func (export \"callat\") (param i32 i32) (result i32)
             (call_indirect (type $t) (local.get 1) (local.get 0))))",
    );

    let store = AotStore::shared();
    let mut a = AotInstance::instantiate(&store, &module_a, &[]).expect("A instantiates");
    let table = a.table_handle(0).expect("A has a shared table");

    let imports = [("a".to_string(), "tab".to_string(), AotExtern::Table(table))];
    let mut b = AotInstance::instantiate(&store, &module_b, &imports).expect("B instantiates");

    // B must see the pre-growth bound.
    let results = b
        .invoke(0, &[WasmValue::I32(0), WasmValue::I32(32)])
        .expect("B dispatches through the shared table");
    assert_eq!(results, vec![WasmValue::I32(42)]);

    // A grows the shared table; B — instantiated earlier — must observe it.
    let results = a
        .invoke(1, &[WasmValue::I32(4)])
        .expect("A grows the shared table");
    assert_eq!(results, vec![WasmValue::I32(1)]);
    let results = b
        .invoke(0, &[WasmValue::I32(4), WasmValue::I32(32)])
        .expect("B dispatches through a slot grown after its instantiation");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}

/// Regression: `table.grow` must not invalidate the cell base or the bound
/// that already-compiled code sees. The compiled `call_indirect` re-loads
/// both from the shared cells holder, and the backing storage is
/// capacity-reserved so it never moves on growth.
#[test]
fn table_grow_then_call_indirect_on_new_slots() {
    let module = load(
        "(module
           (type $t (func (param i32) (result i32)))
           (func $inc (type $t) (param i32) (result i32)
             (i32.add (local.get 0) (i32.const 1)))
           (table 1 funcref)
           (elem (i32.const 0) $inc)
           (func (export \"grow\") (param i32) (result i32)
             (table.grow 0 (ref.func $inc) (local.get 0)))
           (func (export \"callat\") (param i32 i32) (result i32)
             (call_indirect (type $t) (local.get 1) (local.get 0))))",
    );

    let mut instance = AotInstance::new(&module).expect("instantiation");

    // Grow past the original size: the underlying cell storage must not
    // move and the published bound must advance.
    let results = instance
        .invoke(1, &[WasmValue::I32(9)])
        .expect("table.grow succeeds");
    assert_eq!(results, vec![WasmValue::I32(1)]);

    // Dispatch through a slot beyond the original table length.
    let results = instance
        .invoke(2, &[WasmValue::I32(9), WasmValue::I32(41)])
        .expect("call_indirect through a grown slot");
    assert_eq!(results, vec![WasmValue::I32(42)]);

    // And again, deeper into the reservation, to catch capacity-dependent
    // reallocations in the cell buffer.
    let results = instance
        .invoke(1, &[WasmValue::I32(1000)])
        .expect("second grow succeeds");
    assert_eq!(results, vec![WasmValue::I32(10)]);
    let results = instance
        .invoke(2, &[WasmValue::I32(999), WasmValue::I32(41)])
        .expect("call_indirect through the second grown region");
    assert_eq!(results, vec![WasmValue::I32(42)]);
}
