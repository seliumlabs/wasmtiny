//! The `.aot` artifact binary format and writer.
//!
//! The format is versioned, self-describing, and append-only: a fixed-size
//! header followed by a sequence of length-prefixed sections. The last section
//! is always the integrity section (see [`write_artifact`]), whose digest
//! covers every byte preceding the digest itself.
//!
//! This module is the *writer* half of the format contract. The runtime's
//! `src/aot/loader.rs` is the reader half; the constants and encodings here are
//! duplicated there by design (the runtime does not link this compiler).
//!
//! ```text
//! header      magic "WTA0" | format_version | abi_version | endianness
//!             pointer_size | target triple | feature flags   (fixed size)
//! types       function signatures (value-type bytes)
//! imports     func/table/memory/global imports with inlined types
//! exports     name -> kind + index
//! memories    defined memories (limits)
//! tables      defined tables (elem type + limits)
//! globals     defined globals (type + const init expr)
//! data        data segments in unified index order (active + passive)
//! elems       element segments in unified index order (active/passive/declared)
//! func map    defined-function -> type index, code offset/len, trap records
//! code        the finish-linked machine-code image
//! start       optional start function index
//! stack ptr   shadow-stack pointer global index (u32::MAX = none)
//! integrity   scheme | key_id_len | key_id (v1: absent) | SHA512 digest
//!             over every byte preceding the digest
//! ```
//!
//! All integers are unsigned little-endian unless stated otherwise.

use cranelift_codegen::ir::TrapCode as ClifTrapCode;
use cranelift_entity::EntityRef;
use wasmparser::ValType;

use crate::{
    compile::CompiledModule,
    environment::{
        DataSegKind, ElemSegKind, ModuleInfo, USER_TRAP_BAD_SIGNATURE,
        USER_TRAP_CALL_INDIRECT_NULL, USER_TRAP_HOST, USER_TRAP_MEMORY_LIMIT,
        USER_TRAP_NULL_REFERENCE, USER_TRAP_TABLE_OUT_OF_BOUNDS, USER_TRAP_UNREACHABLE,
        wasm_features,
    },
    types::{self, ConstOp, FuncIndex, GlobalIndex, Memory, Table},
};

/// ABI version written (and accepted) by this compiler.
///
/// v2: the vmctx gained a `stack_pointer` field, so the globals-cell layout
/// and shadow-stack routing changed (see `environment::VmCtxOffsets`); the
/// artifact also records the shadow-stack pointer global index in
/// [`SECTION_STACK_POINTER`].
pub const ABI_VERSION: u32 = 2;
/// Endianness marker: little-endian.
pub const ENDIANNESS_LITTLE: u32 = 0;
/// Export kinds (wasm external-kind values).
pub const EXPORT_FUNC: u32 = 0;
pub const EXPORT_GLOBAL: u32 = 3;
pub const EXPORT_MEMORY: u32 = 2;
pub const EXPORT_TABLE: u32 = 1;
/// Feature-flag bits recorded in the header.
pub const FEATURE_ATOMICS: u32 = 1 << 0;
pub const FEATURE_BULK_MEMORY: u32 = 1 << 2;
pub const FEATURE_FLOATS: u32 = 1 << 8;
pub const FEATURE_FUNCTION_REFERENCES: u32 = 1 << 9;
pub const FEATURE_MULTI_VALUE: u32 = 1 << 4;
pub const FEATURE_MUTABLE_GLOBAL: u32 = 1 << 7;
pub const FEATURE_REFERENCE_TYPES: u32 = 1 << 3;
pub const FEATURE_SATURATING_FLOAT_TO_INT: u32 = 1 << 6;
pub const FEATURE_SIGN_EXTENSION: u32 = 1 << 5;
pub const FEATURE_THREADS: u32 = 1 << 1;
/// Artifact format version written (and accepted) by this compiler.
pub const FORMAT_VERSION: u32 = 1;
/// Header offset of the ABI version.
pub const HEADER_ABI_VERSION_OFFSET: usize = 8;
/// Header offset of the endianness marker.
pub const HEADER_ENDIANNESS_OFFSET: usize = 12;
/// Header offset of the format version.
pub const HEADER_FORMAT_VERSION_OFFSET: usize = 4;
/// Header offset of the artifact magic.
pub const HEADER_MAGIC_OFFSET: usize = 0;
/// Header offset of the pointer size.
pub const HEADER_POINTER_SIZE_OFFSET: usize = 16;
/// Total header size in bytes.
pub const HEADER_SIZE: usize = 4 + 4 + 4 + 4 + 4 + TRIPLE_FIELD_SIZE + 4;
/// Header offset of the target triple.
pub const HEADER_TRIPLE_OFFSET: usize = 20;
/// Import kinds.
pub const IMPORT_FUNC: u32 = 0;
pub const IMPORT_GLOBAL: u32 = 3;
pub const IMPORT_MEMORY: u32 = 2;
pub const IMPORT_TABLE: u32 = 1;
/// Integrity scheme identifier for SHA512.
pub const INTEGRITY_SHA512: u8 = 0x01;
/// Artifact magic bytes.
pub const MAGIC: [u8; 4] = *b"WTA0";
/// Pointer size in bytes supported by this ABI.
pub const POINTER_SIZE: u32 = 8;
pub const SECTION_CODE: u32 = 10;
pub const SECTION_DATA: u32 = 7;
pub const SECTION_ELEMS: u32 = 8;
pub const SECTION_EXPORTS: u32 = 3;
pub const SECTION_EXTRA_TRAPS: u32 = 11;
pub const SECTION_FUNCTION_MAP: u32 = 9;
pub const SECTION_GLOBALS: u32 = 6;
pub const SECTION_IMPORTS: u32 = 2;
pub const SECTION_INTEGRITY: u32 = 13;
pub const SECTION_MEMORIES: u32 = 4;
/// The shadow-stack pointer global index (`u32::MAX` when the module has
/// none). Added in ABI v2.
pub const SECTION_STACK_POINTER: u32 = 14;
pub const SECTION_START: u32 = 12;
pub const SECTION_TABLES: u32 = 5;
/// Section identifiers.
pub const SECTION_TYPES: u32 = 1;
/// Byte length of a SHA512 digest.
pub const SHA512_LEN: usize = 64;
pub const TRAP_CALL_INDIRECT_NULL: u8 = 5;
pub const TRAP_HOST: u8 = 11;
pub const TRAP_INDIRECT_CALL_TYPE_MISMATCH: u8 = 4;
pub const TRAP_INTEGER_DIVISION_BY_ZERO: u8 = 8;
pub const TRAP_INTEGER_OVERFLOW: u8 = 7;
/// Trap-code byte encodings, shared with the runtime loader.
pub const TRAP_INVALID: u8 = 0;
pub const TRAP_INVALID_CONVERSION_TO_INT: u8 = 9;
pub const TRAP_MEMORY_LIMIT_EXCEEDED: u8 = 12;
pub const TRAP_MEMORY_OUT_OF_BOUNDS: u8 = 2;
pub const TRAP_NULL_REFERENCE: u8 = 10;
pub const TRAP_STACK_OVERFLOW: u8 = 6;
pub const TRAP_TABLE_OUT_OF_BOUNDS: u8 = 3;
pub const TRAP_UNREACHABLE: u8 = 1;
/// Fixed size of the target-triple field in the header.
pub const TRIPLE_FIELD_SIZE: usize = 64;

/// SHA512 over the provided bytes.
pub fn sha512(bytes: &[u8]) -> [u8; SHA512_LEN] {
    use sha2::{Digest, Sha512};
    let mut hasher = Sha512::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; SHA512_LEN];
    out.copy_from_slice(&digest);
    out
}

/// Maps a Cranelift trap code to the artifact's trap code byte.
pub fn trap_code_byte(code: ClifTrapCode) -> u8 {
    // The built-in trap codes map to their runtime bytes; the remaining
    // wasm-level traps are carried in `TrapCode::User(n)` codes defined in
    // `environment`.
    if code == ClifTrapCode::STACK_OVERFLOW {
        return TRAP_STACK_OVERFLOW;
    }
    if code == ClifTrapCode::HEAP_OUT_OF_BOUNDS {
        return TRAP_MEMORY_OUT_OF_BOUNDS;
    }
    if code == ClifTrapCode::INTEGER_OVERFLOW {
        return TRAP_INTEGER_OVERFLOW;
    }
    if code == ClifTrapCode::INTEGER_DIVISION_BY_ZERO {
        return TRAP_INTEGER_DIVISION_BY_ZERO;
    }
    if code == ClifTrapCode::BAD_CONVERSION_TO_INTEGER {
        return TRAP_INVALID_CONVERSION_TO_INT;
    }
    match code.as_raw().get() {
        USER_TRAP_UNREACHABLE => TRAP_UNREACHABLE,
        USER_TRAP_TABLE_OUT_OF_BOUNDS => TRAP_TABLE_OUT_OF_BOUNDS,
        USER_TRAP_CALL_INDIRECT_NULL => TRAP_CALL_INDIRECT_NULL,
        USER_TRAP_BAD_SIGNATURE => TRAP_INDIRECT_CALL_TYPE_MISMATCH,
        USER_TRAP_NULL_REFERENCE => TRAP_NULL_REFERENCE,
        USER_TRAP_HOST => TRAP_HOST,
        USER_TRAP_MEMORY_LIMIT => TRAP_MEMORY_LIMIT_EXCEEDED,
        _ => TRAP_HOST,
    }
}

/// Serialises the compiled module into a complete `.aot` artifact, including
/// the trailing integrity section.
pub fn write_artifact(compiled: &CompiledModule) -> Vec<u8> {
    let mut out = Vec::with_capacity(compiled.code_image.len() + 1024);
    write_header(&mut out, compiled);
    write_section(
        &mut out,
        SECTION_TYPES,
        &section_types(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_IMPORTS,
        &section_imports(
            &compiled.translator.info,
            &compiled.func_import_stub_offsets,
        ),
    );
    write_section(
        &mut out,
        SECTION_EXPORTS,
        &section_exports(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_MEMORIES,
        &section_memories(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_TABLES,
        &section_tables(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_GLOBALS,
        &section_globals(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_DATA,
        &section_data(&compiled.translator.info),
    );
    write_section(
        &mut out,
        SECTION_ELEMS,
        &section_elems(&compiled.translator.info),
    );

    let function_map = section_function_map(compiled);
    write_section(&mut out, SECTION_FUNCTION_MAP, &function_map);
    write_section(&mut out, SECTION_CODE, &compiled.code_image);
    write_section(
        &mut out,
        SECTION_EXTRA_TRAPS,
        &section_extra_traps(compiled),
    );
    write_section(&mut out, SECTION_START, &section_start(compiled));
    write_section(
        &mut out,
        SECTION_STACK_POINTER,
        &section_stack_pointer(compiled),
    );

    // Integrity section: `scheme | key_id_len | key_id | digest`. The digest
    // covers *every* byte preceding the digest itself — the whole artifact
    // header, all sections, the integrity section's own header, and the
    // scheme/key-id bytes — so the section is self-describing and a future
    // signature scheme can cover the same range without a format change.
    // v1 writes no key id (`key_id_len = 0`); the field is the reserved
    // extension point for PKI signing.
    push_u32(&mut out, SECTION_INTEGRITY);
    push_u32(&mut out, (1 + 1 + SHA512_LEN) as u32);
    out.push(INTEGRITY_SHA512);
    out.push(0); // key_id_len: no key id in v1
    let digest = sha512(&out);
    out.extend_from_slice(&digest);

    out
}

/// Serialises a [`crate::types::ConstExpr`] back into wasm constant-expression
/// bytes that the runtime's existing constant-expression evaluator can replay.
fn const_expr_bytes(init: &crate::types::ConstExpr) -> Vec<u8> {
    let mut out = Vec::new();
    for op in &init.ops {
        match op {
            ConstOp::I32Const(value) => {
                out.push(0x41);
                push_sleb128(&mut out, i64::from(*value));
            }
            ConstOp::I64Const(value) => {
                out.push(0x42);
                push_sleb128(&mut out, *value);
            }
            ConstOp::F32Const(bits) => {
                out.push(0x43);
                out.extend_from_slice(&bits.to_le_bytes());
            }
            ConstOp::F64Const(bits) => {
                out.push(0x44);
                out.extend_from_slice(&bits.to_le_bytes());
            }
            ConstOp::GlobalGet(index) => {
                out.push(0x23);
                push_uleb128(&mut out, u64::from(*index));
            }
            ConstOp::RefFunc(index) => {
                out.push(0xD2);
                push_uleb128(&mut out, u64::from(*index));
            }
            ConstOp::RefNull(byte) => {
                out.push(0xD0);
                out.push(*byte);
            }
            ConstOp::V128Const(_) | ConstOp::RefI31(_) => {
                // Unreachable: SIMD/GC are rejected before translation.
                debug_assert!(false, "SIMD/GC const expression reached the writer");
            }
        }
    }
    out.push(0x0B); // end
    out
}

/// Returns the feature-flag bitmask for the fixed v1 feature set.
fn feature_flags() -> u32 {
    let features = wasm_features();
    let mut flags = 0u32;
    if features.threads() {
        flags |= FEATURE_THREADS | FEATURE_ATOMICS;
    }
    if features.bulk_memory() {
        flags |= FEATURE_BULK_MEMORY;
    }
    if features.reference_types() {
        flags |= FEATURE_REFERENCE_TYPES;
    }
    if features.multi_value() {
        flags |= FEATURE_MULTI_VALUE;
    }
    if features.sign_extension() {
        flags |= FEATURE_SIGN_EXTENSION;
    }
    if features.saturating_float_to_int() {
        flags |= FEATURE_SATURATING_FLOAT_TO_INT;
    }
    if features.mutable_global() {
        flags |= FEATURE_MUTABLE_GLOBAL;
    }
    if features.floats() {
        flags |= FEATURE_FLOATS;
    }
    if features.function_references() {
        flags |= FEATURE_FUNCTION_REFERENCES;
    }
    flags
}

/// Serialises an active-segment constant offset as a length-prefixed wasm
/// const expression.
fn push_len_prefixed_const_offset(out: &mut Vec<u8>, base: Option<GlobalIndex>, offset: u64) {
    let mut offset_bytes = Vec::new();
    match base {
        Some(global) => {
            offset_bytes.push(0x23); // global.get
            push_uleb128(&mut offset_bytes, global.index() as u64);
            offset_bytes.push(0x0B); // end
        }
        None => {
            offset_bytes.push(0x41); // i32.const
            push_sleb128(&mut offset_bytes, offset as i32 as i64);
            offset_bytes.push(0x0B); // end
        }
    }
    push_u32(out, offset_bytes.len() as u32);
    out.extend_from_slice(&offset_bytes);
}

fn push_memory_type(out: &mut Vec<u8>, memory: &Memory) {
    push_u32(out, memory.minimum as u32);
    match memory.maximum {
        Some(max) => {
            push_u32(out, 1);
            push_u32(out, max as u32);
        }
        None => push_u32(out, 0),
    }
    out.push(u8::from(memory.shared));
}

fn push_sleb128(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let mut byte = (value as u8) & 0x7F;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if !done {
            byte |= 0x80;
        }
        out.push(byte);
        if done {
            break;
        }
    }
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    push_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn push_table_type(out: &mut Vec<u8>, table: &Table) {
    out.push(types::ref_type_byte(table.wasm_ty));
    out.push(u8::from(table.wasm_ty.is_nullable()));
    push_u32(out, table.minimum);
    match table.maximum {
        Some(max) => {
            push_u32(out, 1);
            push_u32(out, max);
        }
        None => push_u32(out, 0),
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_uleb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value as u8) & 0x7F;
        value >>= 7;
        if value != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            break;
        }
    }
}

fn section_data(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, info.data_segments.len() as u32);
    for segment in &info.data_segments {
        match &segment.kind {
            DataSegKind::Active {
                memory_index,
                base,
                offset,
            } => {
                out.push(0); // active
                push_u32(&mut out, memory_index.as_u32());
                push_len_prefixed_const_offset(&mut out, *base, *offset);
                push_u32(&mut out, segment.data.len() as u32);
                out.extend_from_slice(&segment.data);
            }
            DataSegKind::Passive => {
                out.push(1); // passive
                push_u32(&mut out, segment.data.len() as u32);
                out.extend_from_slice(&segment.data);
            }
        }
    }
    out
}

fn section_elems(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, info.elem_segments.len() as u32);
    for segment in &info.elem_segments {
        match &segment.kind {
            ElemSegKind::Active {
                table_index,
                base,
                offset,
            } => {
                out.push(0); // active
                push_u32(&mut out, table_index.as_u32());
                push_len_prefixed_const_offset(&mut out, *base, u64::from(*offset));
                push_u32(&mut out, segment.elements.len() as u32);
                for elem in &segment.elements {
                    push_u32(&mut out, elem.as_u32());
                }
            }
            ElemSegKind::Passive => {
                out.push(1); // passive
                push_u32(&mut out, segment.elements.len() as u32);
                for elem in &segment.elements {
                    push_u32(&mut out, elem.as_u32());
                }
            }
            ElemSegKind::Declarative => {
                out.push(2); // declarative
                push_u32(&mut out, segment.elements.len() as u32);
                for elem in &segment.elements {
                    push_u32(&mut out, elem.as_u32());
                }
            }
        }
    }
    out
}

fn section_exports(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    let count = info.func_exports.len()
        + info.table_exports.len()
        + info.memory_exports.len()
        + info.global_exports.len();
    push_u32(&mut out, count as u32);

    for (idx, name) in &info.func_exports {
        push_string(&mut out, name);
        push_u32(&mut out, EXPORT_FUNC);
        push_u32(&mut out, idx.as_u32());
    }
    for (idx, name) in &info.table_exports {
        push_string(&mut out, name);
        push_u32(&mut out, EXPORT_TABLE);
        push_u32(&mut out, idx.as_u32());
    }
    for (idx, name) in &info.memory_exports {
        push_string(&mut out, name);
        push_u32(&mut out, EXPORT_MEMORY);
        push_u32(&mut out, idx.as_u32());
    }
    for (idx, name) in &info.global_exports {
        push_string(&mut out, name);
        push_u32(&mut out, EXPORT_GLOBAL);
        push_u32(&mut out, idx.as_u32());
    }

    out
}

fn section_extra_traps(compiled: &CompiledModule) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, compiled.extra_traps.len() as u32);
    for (offset, code) in &compiled.extra_traps {
        push_u32(&mut out, *offset);
        out.push(trap_code_byte(*code));
    }
    out
}

fn section_function_map(compiled: &CompiledModule) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, compiled.functions.len() as u32);
    for function in &compiled.functions {
        let type_idx = compiled.translator.info.functions[FuncIndex::from_u32(function.func_index)];
        push_u32(&mut out, function.func_index);
        push_u32(&mut out, type_idx.as_u32());
        push_u32(&mut out, function.trampoline_offset);
        push_u32(&mut out, function.code_offset);
        push_u32(&mut out, function.code.len() as u32);
        push_u32(&mut out, function.traps.len() as u32);
        for (offset, code) in &function.traps {
            push_u32(&mut out, *offset);
            out.push(trap_code_byte(*code));
        }
    }
    out
}

fn section_globals(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    let defined: Vec<_> = info
        .globals
        .values()
        .skip(info.imported_global_count())
        .collect();
    push_u32(&mut out, defined.len() as u32);
    for (global, init) in defined {
        out.push(valtype_byte(global.wasm_ty));
        out.push(u8::from(global.mutability));
        let init_bytes =
            const_expr_bytes(init.as_ref().expect("defined global has an initialiser"));
        push_u32(&mut out, init_bytes.len() as u32);
        out.extend_from_slice(&init_bytes);
    }
    out
}

fn section_imports(info: &ModuleInfo, func_import_stub_offsets: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let count = info.imported_funcs.len()
        + info.imported_tables.len()
        + info.imported_memories.len()
        + info.imported_globals.len();
    push_u32(&mut out, count as u32);

    for (func_ordinal, imp) in info.imported_funcs.iter().enumerate() {
        push_u32(&mut out, IMPORT_FUNC);
        push_string(&mut out, &imp.module);
        push_string(&mut out, &imp.field);
        push_u32(&mut out, imp.type_index.as_u32());
        push_u32(&mut out, func_import_stub_offsets[func_ordinal]);
    }
    for imp in &info.imported_tables {
        push_u32(&mut out, IMPORT_TABLE);
        push_string(&mut out, &imp.module);
        push_string(&mut out, &imp.field);
        push_table_type(&mut out, &imp.table);
    }
    for imp in &info.imported_memories {
        push_u32(&mut out, IMPORT_MEMORY);
        push_string(&mut out, &imp.module);
        push_string(&mut out, &imp.field);
        push_memory_type(&mut out, &imp.memory);
    }
    for imp in &info.imported_globals {
        push_u32(&mut out, IMPORT_GLOBAL);
        push_string(&mut out, &imp.module);
        push_string(&mut out, &imp.field);
        out.push(valtype_byte(imp.global.wasm_ty));
        out.push(u8::from(imp.global.mutability));
    }

    out
}

fn section_memories(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    let defined: Vec<Memory> = info
        .memories
        .values()
        .skip(info.imported_memory_count())
        .copied()
        .collect();
    push_u32(&mut out, defined.len() as u32);
    for memory in defined {
        push_memory_type(&mut out, &memory);
    }
    out
}

/// The optional start-function section: a presence byte then, when present,
/// the function index.
fn section_start(compiled: &CompiledModule) -> Vec<u8> {
    let mut out = Vec::new();
    match compiled.translator.info.start_func {
        Some(index) => {
            out.push(1);
            push_u32(&mut out, index.as_u32());
        }
        None => out.push(0),
    }
    out
}

/// The shadow-stack pointer global index: `u32::MAX` when the module has no
/// shadow stack. The runtime gives each concurrent invocation a private
/// stack slot only when this is present (see the `VmCtx` docs).
fn section_stack_pointer(compiled: &CompiledModule) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, compiled.stack_pointer_global.unwrap_or(u32::MAX));
    out
}

fn section_tables(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    let defined: Vec<Table> = info
        .tables
        .values()
        .skip(info.imported_table_count())
        .copied()
        .collect();
    push_u32(&mut out, defined.len() as u32);
    for table in defined {
        push_table_type(&mut out, &table);
    }
    out
}

fn section_types(info: &ModuleInfo) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, info.wasm_types.len() as u32);
    for (_, ty) in info.wasm_types.iter() {
        push_u32(&mut out, ty.params().len() as u32);
        push_u32(&mut out, ty.results().len() as u32);
        for param in ty.params().iter() {
            out.push(valtype_byte(*param));
        }
        for result in ty.results().iter() {
            out.push(valtype_byte(*result));
        }
    }
    out
}

/// Encodes a wasm value type as its standard byte.
fn valtype_byte(ty: ValType) -> u8 {
    types::valtype_byte(ty)
}

/// Writes a fixed-size header.
fn write_header(out: &mut Vec<u8>, compiled: &CompiledModule) {
    out.extend_from_slice(&MAGIC);
    push_u32(out, FORMAT_VERSION);
    push_u32(out, ABI_VERSION);
    push_u32(out, ENDIANNESS_LITTLE);
    push_u32(out, POINTER_SIZE);

    let mut triple = [0u8; TRIPLE_FIELD_SIZE];
    let bytes = compiled.target.to_string().into_bytes();
    let len = bytes.len().min(TRIPLE_FIELD_SIZE);
    triple[..len].copy_from_slice(&bytes[..len]);
    out.extend_from_slice(&triple);

    push_u32(out, feature_flags());
}

fn write_section(out: &mut Vec<u8>, id: u32, payload: &[u8]) {
    push_u32(out, id);
    push_u32(out, payload.len() as u32);
    out.extend_from_slice(payload);
}
