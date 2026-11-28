#![no_main]

//! Fuzz AOT artifact loading and instantiation.
//!
//! Arbitrary bytes must be refused by the artifact verifier or instantiate
//! without panicking; an untrusted artifact must never abort the host.

use libfuzzer_sys::fuzz_target;
use wasmtiny::aot::{AotInstance, AotLoader};

const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_ARTIFACT_BYTES {
        return;
    }
    if let Ok(module) = AotLoader::new().load(data) {
        let _ = AotInstance::new(&module);
    }
});
