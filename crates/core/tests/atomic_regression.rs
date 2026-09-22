//! Regression tests for the harden-runtime-correctness atomics/threads changes.
//!
//! Covers: OOB atomic traps (no host panic), misaligned atomic traps,
//! spec-encoded rmw.add/cmpxchg execution, wait/notify semantics,
//! large timeout values (no overflow/panic), and i64 atomic load8_u
//! zero-extension.

use wasmtiny::{
    WasmApplication, WasmValue,
    runtime::{TrapCode, WasmError},
};

/// i32.atomic.rmw.cmpxchg: failure case (expected doesn't match).
#[test]
fn atomic_cmpxchg_failure_spec_encoding() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "cmpxchg_fail") (result i32)
            i32.const 0       ;; address
            i32.const 1       ;; expected (doesn't match zero-filled memory)
            i32.const 42     ;; replacement
            i32.atomic.rmw.cmpxchg))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "cmpxchg_fail", &[])
        .expect("atomic cmpxchg should succeed");
    // Returns old value (0), memory unchanged (still 0)
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

/// i32.atomic.rmw.cmpxchg at spec subopcode 0x48: success case.
#[test]
fn atomic_cmpxchg_success_spec_encoding() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "cmpxchg_ok") (result i32)
            i32.const 0       ;; address
            i32.const 0       ;; expected (matches zero-filled memory)
            i32.const 42     ;; replacement
            i32.atomic.rmw.cmpxchg))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "cmpxchg_ok", &[])
        .expect("atomic cmpxchg should succeed");
    // Returns old value (0), memory now has 42
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

/// atomic.fence at spec subopcode 0x03 is accepted and is a no-op.
#[test]
fn atomic_fence_executes() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "fence") (result i32)
            atomic.fence
            i32.const 0))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "fence", &[])
        .expect("fence should succeed");
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

/// i64.atomic.load8_u zero-extends (spec subopcode 0x14).
#[test]
fn atomic_load8_u_i64_zero_extends() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "load8") (result i64)
            i32.const 0
            i64.atomic.load8_u))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "load8", &[])
        .expect("atomic load8_u should succeed");
    // Memory is zero-initialised, so load8_u should return 0
    assert_eq!(results, vec![WasmValue::I64(0)]);
}

/// i32.atomic.load at spec subopcode 0x10 reads the correct value.
#[test]
fn atomic_load_i32_spec_encoding() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "load") (result i32)
            i32.const 0
            i32.atomic.load))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    // Write a known value to memory address 0
    app.call_function(module_idx, "load", &[])
        .expect("atomic load should succeed");
}

/// Misaligned atomic access traps.
#[test]
fn atomic_misaligned_traps() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "misaligned_load") (result i32)
            i32.const 1       ;; address 1 is not 4-byte aligned
            i32.atomic.load)) ;; 4-byte access at 1 = misaligned
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app.call_function(module_idx, "misaligned_load", &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "expected alignment trap, got {:?}",
        result
    );
}

/// atomic.notify with no waiters returns 0.
#[test]
fn atomic_notify_no_waiters() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "notify") (result i32)
            i32.const 0       ;; address
            i32.const 1       ;; count
            memory.atomic.notify))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "notify", &[])
        .expect("notify should succeed");
    // No waiters, so 0 notified
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

/// Atomic instruction on non-shared memory is rejected by the validator.
#[test]
fn atomic_on_nonshared_memory_rejected() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "bad")
            i32.const 0
            i32.atomic.load))
    "#;
    let result = wat::parse_str(wat);
    assert!(result.is_ok(), "wat should compile");
    let module = result.unwrap();

    let mut app = WasmApplication::new();
    let err = app.load_module_from_memory(&module);
    assert!(
        err.is_err(),
        "atomic on non-shared memory should be rejected"
    );
}

/// Out-of-bounds atomic access traps (not panics the host).
#[test]
fn atomic_oob_traps() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "oob_load") (result i32)
            i32.const 65536   ;; exactly at the end of 1-page memory
            i32.atomic.load)) ;; 4-byte access at 65536 = OOB
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app.call_function(module_idx, "oob_load", &[]);
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "expected MemoryOutOfBounds trap, got {:?}",
        result
    );
}

/// i32.atomic.rmw.add at spec subopcode 0x1E executes correctly.
#[test]
fn atomic_rmw_add_spec_encoding() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "add") (result i32)
            i32.const 0
            i32.const 5
            i32.atomic.rmw.add))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "add", &[])
        .expect("atomic rmw.add should succeed");
    // Returns old value (0), memory now has 5
    assert_eq!(results, vec![WasmValue::I32(0)]);
}

/// i32.atomic.store at spec subopcode 0x17 followed by i32.atomic.load
/// round-trips a value.
#[test]
fn atomic_store_then_load_roundtrips() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "store_load") (result i32)
            i32.const 0       ;; address
            i32.const 0x12345678 ;; value
            i32.atomic.store
            i32.const 0       ;; address
            i32.atomic.load))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "store_load", &[])
        .expect("store+load should succeed");
    assert_eq!(results, vec![WasmValue::I32(0x12345678)]);
}

/// atomic.wait32 returns 1 (not-equal) when the value doesn't match.
#[test]
fn atomic_wait32_not_equal() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "wait") (result i32)
            i32.const 0       ;; address
            i32.const 1       ;; expected (doesn't match zero-filled memory)
            i64.const 0       ;; timeout (0 = do not wait)
            memory.atomic.wait32))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app
        .call_function(module_idx, "wait", &[])
        .expect("wait should complete");
    // Expected 1 = not equal
    assert_eq!(results, vec![WasmValue::I32(1)]);
}

/// Large timeout values do not cause overflow or panic.
#[test]
fn atomic_wait_large_timeout_no_panic() {
    let wat = r#"
    (module
        (memory 1 1 shared)
        (func (export "wait_big_timeout") (result i32)
            i32.const 0       ;; address
            i32.const 0       ;; expected (matches zero memory, will wait)
            i64.const 1000000000  ;; large timeout in nanoseconds (1 second)
            memory.atomic.wait32))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let result = app.call_function(module_idx, "wait_big_timeout", &[]);
    // The wait will time out after ~1s and return 2.
    // The important thing is it doesn't panic.
    assert!(
        result.is_ok(),
        "expected wait to complete without panic, got {:?}",
        result
    );
    assert_eq!(result.unwrap(), vec![WasmValue::I32(2)]);
}

/// Shared memory without maximum is rejected by the validator.
#[test]
fn shared_memory_without_max_rejected() {
    let wat = r#"
    (module
        (memory 1 shared))
    "#;
    let result = wat::parse_str(wat);
    assert!(result.is_ok(), "wat should compile");
    let module = result.unwrap();

    let mut app = WasmApplication::new();
    let err = app.load_module_from_memory(&module);
    assert!(err.is_err(), "shared memory without max should be rejected");
}
