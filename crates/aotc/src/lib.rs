//! Ahead-of-time compiler for the wasmtiny WebAssembly runtime.
//!
//! This crate compiles WebAssembly binaries (`.wasm`) into ahead-of-time
//! (`.aot`) artifacts. The runtime crate never links this compiler; the
//! library is used by the thin CLI and by in-process tests.
//!
//! # Pipeline
//!
//! 1. `wasmparser` validation with a curated feature set (no SIMD, GC,
//!    exception handling, tail calls, multi-memory, or memory64).
//! 2. Self-contained wasm→CLIF translation (`translate`, a private module)
//!    guided by a [`environment::FuncEnv`] that binds every function to a
//!    hidden context pointer plus the wasmtiny memory/table/global layout.
//!    (The standalone `cranelift-wasm` crate is discontinued and incompatible
//!    with the current `cranelift-codegen`, so the translation lives here —
//!    derived from that crate's final 0.112 sources, which are licensed under
//!    Apache-2.0 WITH LLVM-exception; see the attribution note atop
//!    the module.)
//! 3. `cranelift-codegen` machine-code generation.
//! 4. Finish-linking of intra-module calls against the final code image.

pub use artifact::write_artifact;
pub use compile::CompiledModule;
pub use compile::compile_module;
pub use config::CompilerConfig;
pub use error::{CompileError, CompileResult};

pub mod artifact;
pub mod compile;
pub mod config;
pub mod environment;
pub mod error;
mod trampoline;
mod translate;
pub mod types;

/// Compiles a WebAssembly binary into a finish-linked module.
///
/// This is the primary library entry point; the thin CLI wraps it.
pub fn compile(wasm: &[u8], config: &CompilerConfig) -> CompileResult<CompiledModule> {
    compile_module(wasm, config)
}

/// Compiles a WebAssembly binary straight into a serialised `.aot` artifact.
pub fn compile_artifact(wasm: &[u8], config: &CompilerConfig) -> CompileResult<Vec<u8>> {
    let compiled = compile_module(wasm, config)?;
    Ok(write_artifact(&compiled))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADD_MODULE: &str = "(module (func (export \"add\") (param i32 i32) (result i32)
        (i32.add (local.get 0) (local.get 1))))";

    #[test]
    fn trivial_module_translates_to_clif() {
        let wasm = wat::parse_str(ADD_MODULE).expect("wat parses");
        let compiled = compile(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
        // One defined function, one signature.
        assert_eq!(compiled.translator.info.function_bodies.len(), 1);
        assert_eq!(compiled.translator.info.signatures.len(), 1);
        // The type is (i32, i32) -> (i32).
        let sig = &compiled.translator.info.signatures
            [compiled.translator.info.functions[crate::types::FuncIndex::from_u32(0)]];
        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.returns.len(), 1);
    }

    #[test]
    fn trivial_module_emits_machine_code() {
        let wasm = wat::parse_str(ADD_MODULE).expect("wat parses");
        let compiled = compile(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
        assert_eq!(compiled.functions.len(), 1);
        let function = &compiled.functions[0];
        assert!(!function.code.is_empty());
        // The code image contains the function body plus its entry trampoline.
        assert!(compiled.code_image.len() >= function.code.len());
        assert_eq!(function.code_offset, 0);
        assert!(
            function.trampoline_offset >= function.code_offset + function.code.len() as u32,
            "trampoline is placed after the function body"
        );
    }

    #[test]
    fn direct_calls_are_finish_linked() {
        // `twice` calls `inc` twice; both calls must be resolved against the
        // final code layout with no relocation records left over.
        let wasm = wat::parse_str(
            "(module
               (func $inc (param i32) (result i32)
                 (i32.add (local.get 0) (i32.const 1)))
               (func (export \"twice\") (param i32) (result i32)
                 (call $inc (call $inc (local.get 0)))))",
        )
        .expect("wat parses");
        let compiled = compile(&wasm, &CompilerConfig::host()).expect("compilation succeeds");

        assert_eq!(compiled.functions.len(), 2);
        // `twice` is the second defined function (imports: none).
        let twice = compiled
            .functions
            .iter()
            .find(|f| f.func_index == 1)
            .expect("twice is compiled");
        assert_eq!(
            twice.linked_relocations, 2,
            "both intra-module calls are resolved"
        );
        assert_ne!(twice.code, compiled.functions[0].code);
    }

    #[test]
    fn branch_tables_are_position_independent() {
        // Jump tables must lower to PC-relative references (no absolute
        // relocations), so the linker accepts them for an arbitrary code-image
        // base address.
        let wasm = wat::parse_str(
            "(module
               (func (export \"switch\") (param i32) (result i32)
                 (block $default
                   (block $case2
                     (block $case1
                       (local.get 0)
                       (br_table $case1 $case2 $default))
                     (return (i32.const 1)))
                   (return (i32.const 2)))
                 (return (i32.const 0))))",
        )
        .expect("wat parses");
        let compiled = compile(&wasm, &CompilerConfig::host())
            .expect("branch-table module compiles and links");
        assert!(!compiled.functions.is_empty());
        assert!(!compiled.code_image.is_empty());
    }

    const GOLDEN_MODULE: &str = "(module (func (export \"add\") (param i32 i32) (result i32)
        (i32.add (local.get 0) (local.get 1))))";

    /// Byte-for-byte golden for `GOLDEN_MODULE` compiled for
    /// `x86_64-unknown-linux-gnu`. Pinning a fixed target makes the golden
    /// independent of the host ISA.
    #[test]
    fn golden_byte_comparison() {
        let wasm = wat::parse_str(GOLDEN_MODULE).unwrap();
        let bytes = compile_artifact(
            &wasm,
            &CompilerConfig::for_target("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap();

        let golden_hex = concat!(
            "57544130010000000300000000000000080000007838365f36342d756e6b6e6f",
            "776e2d6c696e75782d676e750000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000ff010000010000000f000000",
            "0100000002000000010000007f7f7f0200000004000000000000000300000013",
            "0000000100000003000000616464000000000000000004000000040000000000",
            "0000050000000400000000000000060000000400000000000000070000000400",
            "000000000000080000000400000000000000090000003a000000010000000000",
            "000000000000800000000000000078000000060000001a000000023300000002",
            "40000000025a000000026c000000066e0000000d0a000000b1000000554889e5",
            "4889e0483b47380f825b0000004c8b5758b904000000f0490fc10a488d790448",
            "3bf90f831f00000049c7c0ffffffff498b024989c14d39c84d0f43c8f04d0fb1",
            "0a0f85ebffffff4c8d4104483bf94c0f4205160000004d3b42080f870a000000",
            "8d04164889ec5dc30f0b0f0bffffffffffffffffcccccccccccccccc554889e5",
            "4883ec104c8924244889f04989cc488b32488b5208ffd0448bc04c89e14c8901",
            "4c8b24244883c4104889ec5dc30b00000004000000000000000c000000010000",
            "00000e00000004000000ffffffff0d000000420000000100b5faf5b5fc34a58e",
            "559fddd737503a5becb122b9ed9efdb5d650a9243e536b8f17fb8d59541a3921",
            "a56e0017be810d26673c6444054a367e57a84a27bbeb06c3"
        );

        let golden: Vec<u8> = (0..golden_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&golden_hex[i..i + 2], 16).unwrap())
            .collect();

        assert_eq!(bytes, golden);
    }

    /// Determinism: compiling the same input twice yields byte-identical
    /// artifacts (also covers the integrity digest, which covers all bytes).
    #[test]
    fn deterministic_emission() {
        let wasm = wat::parse_str(GOLDEN_MODULE).unwrap();
        let config = CompilerConfig::for_target("x86_64-unknown-linux-gnu").unwrap();
        let first = compile_artifact(&wasm, &config).unwrap();
        let second = compile_artifact(&wasm, &config).unwrap();
        assert_eq!(first, second);
    }

    /// The integrity section is always emitted: scheme-tagged, with the
    /// reserved key-id field, and a digest covering every byte preceding the
    /// digest itself.
    #[test]
    fn every_artifact_has_matching_integrity_section() {
        let wasm = wat::parse_str(GOLDEN_MODULE).unwrap();
        let bytes = compile_artifact(&wasm, &CompilerConfig::host()).unwrap();

        // Walk sections; the last must be the integrity section.
        let mut cursor = crate::artifact::HEADER_SIZE;
        let mut last_id = 0u32;
        while cursor + 8 <= bytes.len() {
            let id = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
            let len =
                u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
            let payload_start = cursor + 8;
            let payload_end = payload_start + len;
            if id == crate::artifact::SECTION_INTEGRITY {
                // Payload: scheme | key_id_len | key_id | digest. v1 emits
                // no key id, so the digest starts at offset 2.
                assert_eq!(bytes[payload_start], crate::artifact::INTEGRITY_SHA512);
                let key_id_len = bytes[payload_start + 1] as usize;
                assert_eq!(key_id_len, 0, "v1 writes no key id");
                let digest_start = payload_start + 2 + key_id_len;
                let digest = &bytes[digest_start..payload_end];
                assert_eq!(digest.len(), crate::artifact::SHA512_LEN);
                // The digest covers every byte preceding it, including the
                // scheme and key-id fields.
                let expected = crate::artifact::sha512(&bytes[..digest_start]);
                assert_eq!(digest, &expected[..]);
                assert_eq!(payload_end, bytes.len(), "integrity section is last");
                last_id = id;
            }
            cursor = payload_end;
        }
        assert_eq!(last_id, crate::artifact::SECTION_INTEGRITY);
    }

    /// Big-endian and 32-bit-pointer targets must be refused: the artifact
    /// header and runtime glue assume little-endian, 8-byte pointers.
    #[test]
    fn big_endian_and_32_bit_targets_are_rejected() {
        // mips64: big-endian with 64-bit pointers — isolates the
        // endianness check.
        let mips64 = CompilerConfig::for_target("mips64-unknown-linux-gnu").unwrap();
        match crate::compile::build_isa(&mips64) {
            Err(CompileError::Isa(message)) => {
                assert!(message.to_lowercase().contains("endian"));
            }
            _ => panic!("big-endian target must be rejected with an ISA error"),
        }
        let i686 = CompilerConfig::for_target("i686-unknown-linux-gnu").unwrap();
        match crate::compile::build_isa(&i686) {
            Err(CompileError::Isa(message)) => {
                assert!(message.to_lowercase().contains("pointer"));
            }
            _ => panic!("32-bit target must be rejected with an ISA error"),
        }
    }

    fn assert_unsupported(source: &str, needle: &str) {
        let wasm = wat::parse_str(source).expect("wat parses");
        match compile_artifact(&wasm, &CompilerConfig::host()) {
            Err(CompileError::Unsupported(message)) => {
                assert!(
                    message
                        .to_ascii_lowercase()
                        .contains(&needle.to_ascii_lowercase()),
                    "expected unsupported message to contain {needle:?}, got {message:?}"
                );
            }
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected rejection, but compilation succeeded"),
        }
    }

    #[test]
    fn simd_modules_are_rejected() {
        assert_unsupported("(module (func (param v128)))", "SIMD");
    }

    #[test]
    fn gc_modules_are_rejected() {
        assert_unsupported("(module (type (struct)))", "gc");
    }

    #[test]
    fn exception_modules_are_rejected() {
        assert_unsupported("(module (tag (param i32)))", "exception");
    }

    #[test]
    fn memory64_modules_are_rejected() {
        assert_unsupported("(module (memory i64 1))", "memory64");
    }

    /// Multi-memory must classify as an *unsupported feature* (counted
    /// structurally), not a generic validation error.
    #[test]
    fn multi_memory_modules_are_rejected_as_unsupported() {
        assert_unsupported("(module (memory 1) (memory 1))", "multiple memories");
        assert_unsupported(
            "(module (import \"a\" \"m\" (memory 1)) (memory 1))",
            "multiple memories",
        );
    }

    /// Typed function references are outside the interpreter's coverage and
    /// therefore outside the compiler's: enabling them would let typed and
    /// untyped funcref signatures serialise identically in the artifact's
    /// type section.
    #[test]
    fn typed_function_references_are_rejected() {
        assert_unsupported(
            "(module (type $t (func)) (func (param (ref null $t))))",
            "function references",
        );
        assert_unsupported(
            "(module (type $t (func)) (table 1 (ref null $t)))",
            "function references",
        );
    }

    /// Tail calls are outside the v1 feature set and must classify as an
    /// explicit unsupported-feature error.
    #[test]
    fn tail_call_modules_are_rejected() {
        assert_unsupported(
            "(module (func $f (param i32) (result i32) (local.get 0))
               (func (param i32) (result i32) (return_call $f (local.get 0))))",
            "tail",
        );
    }
}
