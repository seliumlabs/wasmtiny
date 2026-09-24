//! The runtime (reader) half of the `.aot` artifact format.
//!
//! These constants and encodings MUST match the writer in the
//! `wasmtiny-aotc` crate (`artifact.rs`); the two are duplicated by design so
//! the runtime never links the compiler. See that module for the byte-level
//! layout documentation.

use crate::runtime::TrapCode;

/// ABI version accepted by this loader.
///
/// v2: the vmctx gained a `stack_pointer` field and the artifact carries the
/// shadow-stack pointer global index in [`SECTION_STACK_POINTER`].
///
/// v3: the vmctx gained a `meter` field (`*const MeterCells`) and compiled
/// code charges size-weighted fuel at function entry and each loop back-edge.
/// v2 artifacts are refused; regenerate them with the matching compiler.
pub const ABI_VERSION: u32 = 3;
/// Little-endian marker.
pub const ENDIANNESS_LITTLE: u32 = 0;
/// Export kinds (wasm external-kind values).
pub const EXPORT_FUNC: u32 = 0;
pub const EXPORT_GLOBAL: u32 = 3;
pub const EXPORT_MEMORY: u32 = 2;
pub const EXPORT_TABLE: u32 = 1;
/// Format version accepted by this loader.
pub const FORMAT_VERSION: u32 = 1;
/// Total header size in bytes.
pub const HEADER_SIZE: usize = 4 + 4 + 4 + 4 + 4 + TRIPLE_FIELD_SIZE + 4;
/// Import kinds.
pub const IMPORT_FUNC: u32 = 0;
pub const IMPORT_GLOBAL: u32 = 3;
pub const IMPORT_MEMORY: u32 = 2;
pub const IMPORT_TABLE: u32 = 1;
/// Integrity scheme: SHA512.
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
/// Shadow-stack pointer global index (`u32::MAX` = none). ABI v2.
pub const SECTION_STACK_POINTER: u32 = 14;
pub const SECTION_START: u32 = 12;
pub const SECTION_TABLES: u32 = 5;
/// Section identifiers.
pub const SECTION_TYPES: u32 = 1;
/// Byte length of a SHA512 digest.
pub const SHA512_LEN: usize = 64;
pub const TRAP_CALL_INDIRECT_NULL: u8 = 5;
pub const TRAP_EXECUTION_BUDGET_EXCEEDED: u8 = 13;
pub const TRAP_HOST: u8 = 11;
pub const TRAP_INDIRECT_CALL_TYPE_MISMATCH: u8 = 4;
pub const TRAP_INTEGER_DIVISION_BY_ZERO: u8 = 8;
pub const TRAP_INTEGER_OVERFLOW: u8 = 7;
/// Trap-code byte encodings written by the compiler.
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

/// Maps an artifact trap-code byte to the runtime [`TrapCode`].
pub fn trap_code_from_byte(byte: u8) -> Option<TrapCode> {
    Some(match byte {
        TRAP_UNREACHABLE => TrapCode::Unreachable,
        TRAP_MEMORY_OUT_OF_BOUNDS => TrapCode::MemoryOutOfBounds,
        TRAP_TABLE_OUT_OF_BOUNDS => TrapCode::TableOutOfBounds,
        TRAP_INDIRECT_CALL_TYPE_MISMATCH => TrapCode::IndirectCallTypeMismatch,
        TRAP_CALL_INDIRECT_NULL => TrapCode::CallIndirectNull,
        TRAP_STACK_OVERFLOW => TrapCode::StackOverflow,
        TRAP_INTEGER_OVERFLOW => TrapCode::IntegerOverflow,
        TRAP_INTEGER_DIVISION_BY_ZERO => TrapCode::IntegerDivisionByZero,
        TRAP_INVALID_CONVERSION_TO_INT => TrapCode::InvalidConversionToInt,
        TRAP_NULL_REFERENCE => TrapCode::NullReference,
        TRAP_MEMORY_LIMIT_EXCEEDED => TrapCode::MemoryLimitExceeded,
        TRAP_EXECUTION_BUDGET_EXCEEDED => TrapCode::ExecutionBudgetExceeded,
        TRAP_HOST => TrapCode::HostTrap,
        TRAP_INVALID => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_trap_byte_maps_to_the_budget_trap_code() {
        // Byte 13 (written by the compiler's inline fuel charge) loads as the
        // distinct budget-exhausted trap.
        assert_eq!(
            trap_code_from_byte(TRAP_EXECUTION_BUDGET_EXCEEDED),
            Some(TrapCode::ExecutionBudgetExceeded)
        );
        // It must not collide with the memory-limit trap.
        assert_eq!(
            trap_code_from_byte(TRAP_MEMORY_LIMIT_EXCEEDED),
            Some(TrapCode::MemoryLimitExceeded)
        );
    }

    #[test]
    fn unknown_trap_byte_is_refused() {
        assert_eq!(trap_code_from_byte(14), None);
        assert_eq!(trap_code_from_byte(TRAP_INVALID), None);
    }
}
