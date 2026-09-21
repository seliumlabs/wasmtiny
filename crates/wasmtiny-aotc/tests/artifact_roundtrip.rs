//! Round-trip: compile `.wasm` → `.aot` → load + instantiate via the runtime
//! AOT path, with no wasm parsing involved on the runtime side.
//!
//! Living in the compiler crate keeps the runtime's dependency tree free of
//! Cranelift; it is the runtime's `aot::AotLoader` under test here.

use wasmtiny::aot::{AotLoader, ExecutableCode};
use wasmtiny::runtime::{Instance, WasmValue};
use wasmtiny_aotc::{CompilerConfig, artifact::SHA512_LEN, compile_artifact};

const MODULE: &str = r#"(module
    (memory (export "mem") 1)
    (data (i32.const 0) "AB")
    (global $g (export "g") (mut i32) (i32.const 42))
    (table (export "tab") 4 funcref)
    (func $add (export "add") (param i32 i32) (result i32)
      (i32.add (local.get 0) (local.get 1)))
    (elem (i32.const 0) $add))"#;

fn from_source(source: &str) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds")
}

#[test]
fn loads_and_instantiates_without_wasm_parsing() {
    let bytes = from_source(MODULE);

    let loader = AotLoader::new();
    let module = loader.load(&bytes).expect("artifact loads");

    // Metadata arrived intact.
    assert_eq!(module.types.len(), 1);
    assert_eq!(module.memories.len(), 1);
    assert_eq!(module.tables.len(), 1);
    assert_eq!(module.globals.len(), 1);
    assert_eq!(module.data.len(), 1);
    assert_eq!(module.elems.len(), 1);
    assert_eq!(module.functions.len(), 1);
    assert!(module.code_image.len() >= module.functions[0].code_len as usize);

    // Instantiate via the reused runtime machinery.
    let runtime_module = module.into_module();
    let instance = Instance::new(std::sync::Arc::new(runtime_module)).expect("instantiation");

    // Memory export with data segment replayed.
    let memory = instance
        .memory(0)
        .expect("has memory")
        .lock()
        .expect("memory lock");
    assert_eq!(memory.size(), 1);
    assert_eq!(memory.read_u8(0).unwrap(), b'A');
    assert_eq!(memory.read_u8(1).unwrap(), b'B');
    drop(memory);

    // Global export with its initialiser applied.
    let global = instance.global(0).expect("has global");
    assert_eq!(
        global.lock().expect("global lock").get(),
        WasmValue::I32(42)
    );

    // Table export with element segment replayed (a funcref handle).
    let table = instance.table(0).expect("has table");
    assert_eq!(table.lock().expect("table lock").size(), 4);
    let first = table.lock().expect("table lock").get(0).unwrap();
    assert!(
        matches!(first, WasmValue::FuncRef(_)),
        "elem replayed: {first:?}"
    );

    // Exports resolve without any wasm parsing.
    assert!(matches!(
        instance.export("mem"),
        Some(wasmtiny::runtime::Extern::Memory(_))
    ));
    assert!(matches!(
        instance.export("g"),
        Some(wasmtiny::runtime::Extern::Global(_))
    ));
    assert!(matches!(
        instance.export("add"),
        Some(wasmtiny::runtime::Extern::Func(_))
    ));
    assert!(matches!(
        instance.export("tab"),
        Some(wasmtiny::runtime::Extern::Table(_))
    ));
}

#[test]
fn tampered_artifact_is_refused() {
    let mut bytes = from_source(MODULE);
    // Flip a byte in the header region (magic-independent, covered by digest).
    let flip = bytes.len() - 10;
    bytes[flip] ^= 0xFF;

    let loader = AotLoader::new();
    let err = loader
        .load(&bytes)
        .expect_err("tampered artifact must be refused");
    assert!(
        format!("{err}").contains("integrity"),
        "expected integrity error, got {err}"
    );
}

#[test]
fn unsigned_artifact_is_refused() {
    // Remove the entire trailing integrity section (id + length + scheme +
    // key_id_len + digest).
    let mut bytes = from_source(MODULE);
    let integrity_section_len = 8 + 1 + 1 + SHA512_LEN;
    bytes.truncate(bytes.len() - integrity_section_len);

    let loader = AotLoader::new();
    let err = loader
        .load(&bytes)
        .expect_err("unsigned artifact must be refused");
    assert!(
        format!("{err}").contains("integrity"),
        "expected integrity error, got {err}"
    );
}

#[test]
fn truncated_artifact_errors_without_panicking() {
    let bytes = from_source(MODULE);
    let loader = AotLoader::new();
    for cut in [4usize, 16, 40, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            loader.load(&bytes[..cut]).is_err(),
            "truncation at {cut} must be refused"
        );
    }
}

#[test]
fn wrong_abi_version_is_refused() {
    let mut bytes = from_source(MODULE);
    // ABI version field is the second u32 (offset 4 within the header, + magic 4).
    let abi_offset = 8;
    bytes[abi_offset..abi_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    let loader = AotLoader::new();
    let err = loader
        .load(&bytes)
        .expect_err("mismatched ABI must be refused");
    assert!(format!("{err}").contains("ABI"), "got {err}");
}

#[test]
fn code_image_maps_executable() {
    let bytes = from_source("(module (func (export \"f\") (result i32) (i32.const 7)))");
    let loader = AotLoader::new();
    let module = loader.load(&bytes).expect("artifact loads");
    let code = ExecutableCode::from_bytes(&module.code_image).expect("code maps executable");
    assert_eq!(code.len(), module.code_image.len());
    // The mapping is at minimum non-empty and executable (the machine code for
    // the exported function). Execution itself is exercised by the runtime
    // trampoline path in later stages.
    assert!(!code.is_empty());
}
