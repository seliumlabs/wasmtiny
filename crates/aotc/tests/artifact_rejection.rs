//! Artifact-rejection security corpus: the loader is fail-closed against
//! tampered, unsigned, version-mismatched and ISA-mismatched artifacts.
//!
//! Header and section offsets are taken from the writer's own constants
//! (`wasmtiny_aotc::artifact`) rather than hardcoded byte positions, so a
//! format change cannot silently break these tests.

use wasmtiny::aot::AotLoader;
use wasmtiny_aotc::{
    CompilerConfig,
    artifact::{
        HEADER_ABI_VERSION_OFFSET, HEADER_ENDIANNESS_OFFSET, HEADER_FORMAT_VERSION_OFFSET,
        HEADER_MAGIC_OFFSET, HEADER_POINTER_SIZE_OFFSET, INTEGRITY_SHA512, SECTION_ELEMS,
        SHA512_LEN,
    },
    compile_artifact,
};

const ADD: &str = "(module (func (export \"add\") (param i32 i32) (result i32)
    (i32.add (local.get 0) (local.get 1))))";
/// Length of the trailing integrity section (section header + payload):
/// id u32, len u32, scheme u8, key_id_len u8, digest 64 bytes.
const INTEGRITY_SECTION_SIZE: usize = 8 + 1 + 1 + SHA512_LEN;

#[test]
fn abi_version_skew_is_refused() {
    let mut bytes = compile(&host_target());
    let at = HEADER_ABI_VERSION_OFFSET;
    bytes[at..at + 4].copy_from_slice(&99u32.to_le_bytes());
    let message = refuse(&bytes);
    assert!(message.contains("ABI version"), "got {message}");
}

/// The ABI bumped 2 -> 3 when the vmctx gained the `meter` field. A v2
/// artifact must be refused (and the error must name the version and the
/// remedy), so it can never be executed against the v3 layout.
#[test]
fn previous_abi_version_is_refused() {
    let mut bytes = compile(&host_target());
    let at = HEADER_ABI_VERSION_OFFSET;
    bytes[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    let message = refuse(&bytes);
    assert!(message.contains("ABI version 2"), "got {message}");
    assert!(
        message.contains("regenerate"),
        "the ABI error must name the remedy, got {message}"
    );
}

#[test]
fn bad_magic_is_refused() {
    let mut bytes = compile(&host_target());
    bytes[HEADER_MAGIC_OFFSET + 3] ^= 0xFF;
    let message = refuse(&bytes);
    assert!(message.contains("magic"), "got {message}");
}

fn compile(target: &str) -> Vec<u8> {
    let wasm = wat::parse_str(ADD).expect("wat parses");
    compile_artifact(&wasm, &CompilerConfig::for_target(target).unwrap()).expect("compiles")
}

/// A *crafted* artifact — its author recomputed the digest, so integrity
/// verification passes — must still be refused when its index spaces do not
/// hold together. SHA512 is corruption detection, not authentication; the
/// loader must not be able to panic on a self-hashed artifact.
#[test]
fn crafted_artifact_with_out_of_range_indices_is_refused() {
    let wasm = wat::parse_str(
        "(module (table 1 funcref) (func $f (result i32) (i32.const 1))
           (elem (i32.const 0) $f))",
    )
    .expect("wat parses");
    let mut bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compiles");

    // Walk to the element section and rewrite its single function index to
    // an out-of-range value, then re-digest: the artifact is otherwise
    // perfectly self-consistent.
    let header = wasmtiny_aotc::artifact::HEADER_SIZE;
    let mut cursor = header;
    let elem_at = loop {
        let id = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        if id == SECTION_ELEMS {
            break (cursor + 8, len);
        }
        cursor += 8 + len;
    };
    let (payload, section_len) = elem_at;
    // Section payload: segment count u32, then per segment:
    //   kind u8 | table_idx u32 | offset_len u32 | offset | elem_count u32
    //   | func_index u32 (repeated).
    // Parse rather than assume offsets: the offset expression has its own
    // recorded length.
    let mut position = payload + 4;
    assert_eq!(bytes[position], 0, "expected an active element segment");
    position += 1; // kind
    position += 4; // table_idx
    let offset_len = u32::from_le_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
    position += 4 + offset_len;
    let elem_count = u32::from_le_bytes(bytes[position..position + 4].try_into().unwrap());
    assert_eq!(elem_count, 1, "expected exactly one element entry");
    position += 4;
    let func_index_at = position;
    assert!(
        func_index_at + 4 <= payload + section_len,
        "element section layout changed; update the test offsets"
    );
    bytes[func_index_at..func_index_at + 4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

    // Re-digest over everything preceding the digest.
    let digest_start = bytes.len() - SHA512_LEN;
    let digest = wasmtiny_aotc::artifact::sha512(&bytes[..digest_start]);
    bytes[digest_start..].copy_from_slice(&digest);

    let message = refuse(&bytes);
    assert!(
        message.contains("function index space"),
        "a crafted out-of-range element index must be refused, got {message}"
    );
}

#[test]
fn endianness_mismatch_is_refused() {
    let mut bytes = compile(&host_target());
    let at = HEADER_ENDIANNESS_OFFSET;
    bytes[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
    let message = refuse(&bytes);
    assert!(message.contains("endianness"), "got {message}");
}

#[test]
fn format_version_skew_is_refused() {
    let mut bytes = compile(&host_target());
    let at = HEADER_FORMAT_VERSION_OFFSET;
    bytes[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    let message = refuse(&bytes);
    assert!(message.contains("format version"), "got {message}");
}

/// The host triple in the shape the compiler target parser accepts.
fn host_target() -> String {
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => other,
    };
    format!("{arch}-{os}")
}

/// A *correctly digested* artifact whose unkeyed SHA512 scheme declares a
/// key id must be refused: the key-id field is reserved for keyed schemes,
/// and an unkeyed scheme accepting one would let attackers smuggle
/// unverified key material past the verifier.
#[test]
fn keyed_unkeyed_scheme_is_refused() {
    let mut bytes = compile(&host_target());
    let payload_start = bytes.len() - (1 + 1 + SHA512_LEN);
    let len_field = payload_start - 4;
    // scheme byte, then key_id_len: declare one byte of key id.
    assert_eq!(bytes[payload_start], INTEGRITY_SHA512);
    bytes[payload_start + 1] = 1;
    bytes.insert(payload_start + 2, 0xAB); // the smuggled key-id byte

    // Recompute the digest so the payload is otherwise perfectly valid:
    // the digest covers every byte preceding it, so it must be rebuilt over
    // the modified prefix.
    let modified = wasmtiny_aotc::artifact::sha512(&bytes[..payload_start + 3]);
    bytes[payload_start + 3..payload_start + 3 + SHA512_LEN].copy_from_slice(&modified);

    // The section length must also grow by the key-id byte.
    let old_len = u32::from_le_bytes(bytes[len_field..len_field + 4].try_into().unwrap());
    bytes[len_field..len_field + 4].copy_from_slice(&(old_len + 1).to_le_bytes());

    let message = refuse(&bytes);
    assert!(
        message.contains("unkeyed"),
        "a keyed unkeyed-scheme artifact must be refused, got {message}"
    );
}

#[test]
fn mismatched_target_isa_is_refused() {
    let host = host_target();
    let alien = if host.starts_with("aarch64") {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-unknown-linux-gnu"
    };
    let bytes = compile(alien);
    let message = refuse(&bytes);
    assert!(
        message.contains("ISA") || message.contains("target"),
        "got {message}"
    );
}

#[test]
fn pointer_size_mismatch_is_refused() {
    let mut bytes = compile(&host_target());
    let at = HEADER_POINTER_SIZE_OFFSET;
    bytes[at..at + 4].copy_from_slice(&4u32.to_le_bytes());
    let message = refuse(&bytes);
    assert!(message.contains("pointer size"), "got {message}");
}

/// Loads `bytes`; returns the error message on refusal.
fn refuse(bytes: &[u8]) -> String {
    match AotLoader::new().load(bytes) {
        Ok(module) => panic!("expected refusal, but the artifact loaded: {module:?}"),
        Err(error) => format!("{error}"),
    }
}

#[test]
fn tampered_artifact_is_refused() {
    let mut bytes = compile(&host_target());
    // Flip a byte in the code image (well past the header).
    *bytes.last_mut().expect("non-empty") ^= 0xFF;
    let message = refuse(&bytes);
    // The digest no longer matches the bytes it covers.
    assert!(
        message.contains("integrity") || message.contains("digest"),
        "got {message}"
    );
}

#[test]
fn truncated_artifact_is_refused_without_panicking() {
    let bytes = compile(&host_target());
    for len in [0usize, 1, 3, 40, bytes.len() / 2, bytes.len() - 1] {
        let message = refuse(&bytes[..len]);
        assert!(!message.is_empty());
    }
}

#[test]
fn unsigned_artifact_is_refused() {
    let bytes = compile(&host_target());
    // Cut the integrity section off entirely — the loader must fail closed.
    let unsigned = &bytes[..bytes.len() - INTEGRITY_SECTION_SIZE];
    let message = refuse(unsigned);
    assert!(message.contains("integrity"), "got {message}");
}
