//! Memory/table management and bulk-memory parity for the AOT path.

use wasmtiny::aot::AotLoader;
use wasmtiny::runtime::{TrapCode, WasmError, WasmValue};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

fn instantiate(source: &str) -> wasmtiny::aot::AotInstance {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    let module = AotLoader::new().load(&bytes).expect("artifact loads");
    wasmtiny::aot::AotInstance::new(&module).expect("instantiation succeeds")
}

fn result(source: &str, args: &[WasmValue]) -> Vec<WasmValue> {
    let mut instance = instantiate(source);
    instance.invoke(0, args).expect("invoke succeeds")
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
fn memory_size_reports_declared_pages() {
    let source = "(module (memory 3) (func (export \"size\") (result i32) (memory.size)))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(3)]);
}

#[test]
fn memory_grow_returns_old_size_and_grows() {
    let source = "(module (memory 1 4)
        (func (export \"grow\") (param i32) (result i32) (memory.grow (local.get 0)))
        (func (export \"size\") (result i32) (memory.size)))";
    let mut instance = instantiate(source);
    assert_eq!(
        instance.invoke(0, &[WasmValue::I32(2)]).unwrap(),
        vec![WasmValue::I32(1)]
    );
    assert_eq!(instance.invoke(1, &[]).unwrap(), vec![WasmValue::I32(3)]);
}

#[test]
fn memory_grow_beyond_max_returns_minus_one() {
    let source = "(module (memory 1 1)
        (func (export \"grow\") (param i32) (result i32) (memory.grow (local.get 0))))";
    assert_eq!(
        result(source, &[WasmValue::I32(1)]),
        vec![WasmValue::I32(-1)]
    );
}

#[test]
fn memory_copy_is_memmove_for_overlapping_ranges() {
    // "abc" at [0..3]; copy dst=1 src=0 len=3 must yield "aab c"-style
    // memmove (byte 2 must remain 'b', not be clobbered by a forward copy).
    let source = "(module (memory 1) (data (i32.const 0) \"abc\")
        (func (export \"copy\") (result i32)
          (memory.copy (i32.const 1) (i32.const 0) (i32.const 3))
          (i32.load8_u (i32.const 2))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(98)]); // 'b'
}

#[test]
fn memory_fill_writes_the_byte() {
    let source = "(module (memory 1)
        (func (export \"fill\") (result i32)
          (memory.fill (i32.const 5) (i32.const 42) (i32.const 4))
          (i32.load8_u (i32.const 7))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(42)]);
}

#[test]
fn memory_init_copies_passive_data() {
    let source = "(module (memory 1) (data \"abcd\")
        (func (export \"init\") (result i32)
          (memory.init 0 (i32.const 0) (i32.const 1) (i32.const 3))
          (i32.load8_u (i32.const 2))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(100)]); // 'd'
}

#[test]
fn memory_init_after_data_drop_traps() {
    let source = "(module (memory 1) (data \"abcd\")
        (func (export \"f\") (result i32)
          (data.drop 0)
          (memory.init 0 (i32.const 0) (i32.const 0) (i32.const 1))
          (i32.const 0)))";
    assert_eq!(trap(source, &[]), TrapCode::MemoryOutOfBounds);
}

#[test]
fn table_size_and_grow() {
    let source = "(module (table 1 4 funcref)
        (func (export \"grow\") (result i32)
          (table.grow 0 (ref.null func) (i32.const 2)))
        (func (export \"size\") (result i32)
          (table.size 0)))";
    let mut instance = instantiate(source);
    assert_eq!(instance.invoke(0, &[]).unwrap(), vec![WasmValue::I32(1)]);
    assert_eq!(instance.invoke(1, &[]).unwrap(), vec![WasmValue::I32(3)]);
}

#[test]
fn table_grow_beyond_max_returns_minus_one() {
    let source = "(module (table 1 2 funcref)
        (func (export \"grow\") (result i32)
          (table.grow 0 (ref.null func) (i32.const 2))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(-1)]);
}

#[test]
fn table_set_then_get_non_null() {
    let source = "(module (table 4 funcref)
        (func (export \"run\") (result i32)
          (table.set 0 (i32.const 2) (ref.func 0))
          (ref.is_null (table.get 0 (i32.const 2)))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(0)]);
}

#[test]
fn table_copy_moves_handles() {
    let source = "(module (table 4 funcref)
        (func (export \"run\") (result i32)
          (table.set 0 (i32.const 0) (ref.func 0))
          (table.copy 0 0 (i32.const 1) (i32.const 0) (i32.const 1))
          (ref.is_null (table.get 0 (i32.const 1)))))";
    assert_eq!(result(source, &[]), vec![WasmValue::I32(0)]);
}

#[test]
fn table_init_copies_passive_elements() {
    let source = "(module (table 4 funcref)
        (func $a)
        (func $b)
        (elem $e func $a $b)
        (func (export \"run\") (result i32)
          (table.init 0 $e (i32.const 1) (i32.const 0) (i32.const 2))
          (ref.is_null (table.get 0 (i32.const 1)))))";
    let mut instance = instantiate(source);
    assert_eq!(instance.invoke(2, &[]).unwrap(), vec![WasmValue::I32(0)]);
}

#[test]
fn table_init_after_elem_drop_traps() {
    let source = "(module (table 4 funcref)
        (func $a)
        (elem $e func $a)
        (func (export \"run\") (result i32)
          (elem.drop $e)
          (table.init 0 $e (i32.const 0) (i32.const 0) (i32.const 1))
          (ref.is_null (table.get 0 (i32.const 0)))))";
    let mut instance = instantiate(source);
    let code = match instance.invoke(1, &[]) {
        Err(WasmError::Trap(code)) => code,
        Err(other) => panic!("expected a trap, got error {other}"),
        Ok(values) => panic!("expected a trap, got {values:?}"),
    };
    assert_eq!(code, TrapCode::TableOutOfBounds);
}
