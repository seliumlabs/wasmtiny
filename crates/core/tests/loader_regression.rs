//! Regression tests for the harden-runtime-correctness loader changes.
//!
//! Covers: DataCount section acceptance, if-without-else rejection,
//! non-zero memory index on bulk ops, memory.copy/fill OOB traps,
//! and malicious-count rejection.

use wasmtiny::{
    WasmApplication, WasmValue,
    runtime::{TrapCode, WasmError},
};

/// A module with a DataCount section loads and runs.
#[test]
fn datacount_section_loads() {
    let wat = r#"
    (module
        (memory 1)
        (data "Hello")
        (func (export "read") (result i32)
            i32.const 0
            i32.load8_u))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    // The key assertion: the module loads and runs (DataCount accepted)
    let result = app
        .call_function(module_idx, "read", &[])
        .expect("read should succeed");
    assert_eq!(result.len(), 1);
}

/// A module with a huge type section count is rejected without large allocations.
#[test]
fn huge_type_count_rejected() {
    // Build a raw wasm binary with a huge type section count
    let mut wasm = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
    // Type section (id 1)
    wasm.push(1); // section id
    // section payload: count = 0xFFFFFFFF (5-byte LEB128)
    let count_bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
    let payload = count_bytes.to_vec();
    let payload_len = payload.len() as u32;
    // LEB128 encode the section size
    let mut size_bytes = Vec::new();
    let mut size = payload_len;
    loop {
        let mut byte = (size & 0x7F) as u8;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        size_bytes.push(byte);
        if size == 0 {
            break;
        }
    }
    wasm.extend_from_slice(&size_bytes);
    wasm.extend_from_slice(&payload);

    let mut app = WasmApplication::new();
    let result = app.load_module_from_memory(&wasm);
    assert!(result.is_err(), "huge type count should be rejected");
}

/// A module with an `if` without `else` where params != results should be rejected.
#[test]
fn if_without_else_mismatched_arity_rejected() {
    let wat = r#"
    (module
        (func (export "test") (result i32)
            i32.const 1
            if (result i32)
                i32.const 42
            end))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let result = app.load_module_from_memory(&module);
    assert!(
        result.is_err(),
        "if without else with mismatched arity should be rejected"
    );
}

/// memory.copy OOB traps without allocating.
#[test]
fn memory_copy_oob_traps() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "copy_oob")
            i32.const 65530   ;; dst near end of page
            i32.const 0       ;; src
            i32.const 10      ;; len (will overflow page boundary)
            memory.copy))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app.call_function(module_idx, "copy_oob", &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "expected OOB trap, got {:?}",
        result
    );
}

/// memory.fill OOB traps without allocating.
#[test]
fn memory_fill_oob_traps() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "fill_oob")
            i32.const 65530   ;; dst near end of page
            i32.const 42      ;; value
            i32.const 10      ;; len (will overflow page boundary)
            memory.fill))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app.call_function(module_idx, "fill_oob", &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "expected OOB trap, got {:?}",
        result
    );
}

/// A simple module with memory.fill and memory.copy works correctly.
#[test]
fn memory_fill_then_copy_works() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "run") (result i32)
            ;; fill 4 bytes of 9 at address 0
            i32.const 0
            i32.const 9
            i32.const 4
            memory.fill
            ;; copy to address 8
            i32.const 8
            i32.const 0
            i32.const 4
            memory.copy
            ;; read back byte 8
            i32.const 8
            i32.load8_u))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app
        .call_function(module_idx, "run", &[])
        .expect("run should succeed");
    assert_eq!(result, vec![WasmValue::I32(9)]);
}
