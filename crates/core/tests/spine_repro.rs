//! Regression tests for the interpreter and instance-lifetime bugs found
//! while bringing up the Selium spine: nested-call `return` semantics, i64
//! shift operand types, bulk-memory instructions, per-invocation memory
//! sharing, and guest-native (call_indirect) dispatch staying in the
//! caller's instance.

use std::sync::atomic::{AtomicUsize, Ordering};

use wasmtiny::{
    FunctionType, NumType, ValType, WasmApplication, WasmValue,
    runtime::{HostCaller, HostFunc},
};

static HOST_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Registers a host import that counts invocations and echoes a fixed i64.
struct CountingHostFunc {
    counter: &'static AtomicUsize,
}

impl HostFunc for CountingHostFunc {
    fn call(
        &self,
        _caller: &mut HostCaller<'_>,
        args: &[WasmValue],
    ) -> wasmtiny::runtime::Result<Vec<WasmValue>> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(if args.len() == 2 {
            vec![WasmValue::I64(0)]
        } else {
            vec![]
        })
    }

    fn function_type(&self) -> Option<&FunctionType> {
        None
    }
}

/// Bulk-memory instructions validate and execute (rustc-emitted memcpy).
#[test]
fn bulk_memory_instructions_execute() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "run") (result i32)
            ;; memory.fill: 4 bytes of 9 at address 0
            i32.const 0
            i32.const 9
            i32.const 4
            memory.fill
            ;; memory.copy: copy to address 8
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

    let results = app.call_function(module_idx, "run", &[]).expect("run");
    assert_eq!(results, vec![WasmValue::I32(9)]);
}

/// A module that calls a host import through a direct import call and via
/// `call_indirect` through a funcref table, then returns from a nested call.
#[test]
fn hostcalls_and_guest_natives_share_one_instance() {
    let wat = r#"
    (module
        (import "env" "host" (func $host (param i32 i32) (result i64)))
        (memory 1)
        (table 1 funcref)
        (elem (i32.const 0) $nested)
        (func $nested (result i32)
            i32.const 41
            return)
        (func (export "run") (result i32)
            ;; direct import call: host(0, 0), result dropped
            i32.const 0
            i32.const 0
            call $host
            drop
            ;; guest-native call through the table: $nested()
            i32.const 0
            call_indirect (type 0)
            ;; nested call returning a value, then add 1
            i32.const 1
            i32.add)
        (type $v_i (func (result i32))))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.register_host_function(
        module_idx,
        "env",
        "host",
        Box::new(CountingHostFunc {
            counter: &HOST_CALLS,
        }),
        FunctionType::new(
            vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I64)],
        ),
    )
    .expect("register host import");
    app.instantiate(module_idx).expect("instantiate");

    HOST_CALLS.store(0, Ordering::SeqCst);
    let results = app.call_function(module_idx, "run", &[]).expect("call run");

    // call_indirect reached $nested in the caller's instance (fresh-instance
    // dispatch would lose shared memory state); `return` resumed the caller.
    assert_eq!(results, vec![WasmValue::I32(42)]);
    assert_eq!(HOST_CALLS.load(Ordering::SeqCst), 1);
}

/// i64 shifts take the shift count as an i64 operand.
#[test]
fn i64_shifts_accept_i64_count() {
    let wat = r#"
    (module
        (func (export "run") (result i64)
            i64.const 32
            i64.const 2
            i64.shr_u))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let results = app.call_function(module_idx, "run", &[]).expect("run");
    assert_eq!(results, vec![WasmValue::I64(8)]);
}

/// `memory.grow` inside one invocation must remain visible to the next.
#[test]
fn memory_growth_persists_across_invocations() {
    let wat = r#"
    (module
        (memory 1)
        (func (export "grow_and_write") (result i32)
            ;; grow by 1 page, then write a byte into the new page
            i32.const 1
            memory.grow
            drop
            i32.const 65540
            i32.const 7
            i32.store8
            i32.const 0)
        (func (export "read") (result i32)
            i32.const 65540
            i32.load8_u))
    "#;
    let module = wat::parse_str(wat).expect("compile wat");

    let mut app = WasmApplication::new();
    let module_idx = app.load_module_from_memory(&module).expect("load module");
    app.instantiate(module_idx).expect("instantiate");

    let grown = app
        .call_function(module_idx, "grow_and_write", &[])
        .expect("grow and write");
    assert_eq!(grown, vec![WasmValue::I32(0)]);

    // A separate invocation of the same module must see both the growth and
    // the data written by the earlier invocation.
    let read = app
        .call_function(module_idx, "read", &[])
        .expect("read back");
    assert_eq!(read, vec![WasmValue::I32(7)]);
}
