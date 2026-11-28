#![no_main]

//! Fuzz wasm module parsing, validation, and the embedder module probe.
//!
//! Arbitrary bytes must be rejected with an error or produce a valid module;
//! neither path may panic, abort, or allocate from an untrusted declaration.

use libfuzzer_sys::fuzz_target;
use wasmtiny::loader::{Parser, Validator};

const MAX_MODULE_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_MODULE_BYTES {
        return;
    }
    if let Ok(module) = Parser::new().parse(data) {
        let _ = Validator::new().validate(&module);
    }
    let _ = wasmtiny::probe(data);
});
