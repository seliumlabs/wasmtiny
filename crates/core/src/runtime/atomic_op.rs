//! Spec-compliant atomic operation metadata for the WebAssembly threads proposal.
//!
//! The 0xFE prefix introduces atomic instructions whose subopcode values are
//! defined by the threads proposal. This module provides a single source of
//! truth for the subopcode encoding, operand types, result type, and access
//! width of each atomic instruction. It is shared by the validator, the
//! interpreter, and the block scanner's `skip_immediates` so that they cannot
//! disagree.

use super::{FunctionType, NumType, ValType};

/// All spec-encoded atomic operations, indexed by their 0xFE subopcode.
///
/// Spec subopcode assignments (WebAssembly threads proposal):
/// - `memory.atomic.notify`    = 0x00
/// - `memory.atomic.wait32`     = 0x01
/// - `memory.atomic.wait64`     = 0x02
/// - `atomic.fence`             = 0x03
/// - loads                      = 0x10–0x16
/// - stores                     = 0x17–0x1D
/// - RMW                        = 0x1E–0x4E (including cmpxchg 0x48–0x4E)
pub const ATOMIC_OPS: &[AtomicOpMeta] = &[
    // 0x00 — memory.atomic.notify
    // Pop order (spec): count (i32), address (i32). Push i32 (true count).
    AtomicOpMeta {
        subopcode: 0x00,
        kind: AtomicKind::Notify,
        operand_type: None,
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x01 — memory.atomic.wait32
    // Stack (bottom→top): address (i32), expected (i32), timeout (i64). Push i32.
    AtomicOpMeta {
        subopcode: 0x01,
        kind: AtomicKind::Wait32,
        operand_type: None,
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32, I64],
        results: &[I32],
    },
    // 0x02 — memory.atomic.wait64
    // Stack (bottom→top): address (i32), expected (i64), timeout (i64). Push i32.
    AtomicOpMeta {
        subopcode: 0x02,
        kind: AtomicKind::Wait64,
        operand_type: None,
        result_type: Some(NumType::I32),
        access_width: 8,
        params: &[I32, I64, I64],
        results: &[I32],
    },
    // 0x03 — atomic.fence
    // No operands, no result. One reserved 0x00 immediate byte.
    AtomicOpMeta {
        subopcode: 0x03,
        kind: AtomicKind::Fence,
        operand_type: None,
        result_type: None,
        access_width: 0,
        params: &[],
        results: &[],
    },
    // === Loads 0x10–0x16 ===
    AtomicOpMeta {
        subopcode: 0x10,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32],
        results: &[I32],
    },
    AtomicOpMeta {
        subopcode: 0x11,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32],
        results: &[I64],
    },
    AtomicOpMeta {
        subopcode: 0x12,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32],
        results: &[I32],
    },
    AtomicOpMeta {
        subopcode: 0x13,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32],
        results: &[I32],
    },
    AtomicOpMeta {
        subopcode: 0x14,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32],
        results: &[I64],
    },
    AtomicOpMeta {
        subopcode: 0x15,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32],
        results: &[I64],
    },
    AtomicOpMeta {
        subopcode: 0x16,
        kind: AtomicKind::Load,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32],
        results: &[I64],
    },
    // === Stores 0x17–0x1D ===
    AtomicOpMeta {
        subopcode: 0x17,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I32),
        result_type: None,
        access_width: 4,
        params: &[I32, I32],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x18,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I64),
        result_type: None,
        access_width: 8,
        params: &[I32, I64],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x19,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I32),
        result_type: None,
        access_width: 1,
        params: &[I32, I32],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x1A,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I32),
        result_type: None,
        access_width: 2,
        params: &[I32, I32],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x1B,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I64),
        result_type: None,
        access_width: 1,
        params: &[I32, I64],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x1C,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I64),
        result_type: None,
        access_width: 2,
        params: &[I32, I64],
        results: &[],
    },
    AtomicOpMeta {
        subopcode: 0x1D,
        kind: AtomicKind::Store,
        operand_type: Some(NumType::I64),
        result_type: None,
        access_width: 4,
        params: &[I32, I64],
        results: &[],
    },
    // === RMW 0x1E–0x4E ===
    // 0x1E — i32.atomic.rmw.add
    AtomicOpMeta {
        subopcode: 0x1E,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x1F — i64.atomic.rmw.add
    AtomicOpMeta {
        subopcode: 0x1F,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x20 — i32.atomic.rmw8.add_u
    AtomicOpMeta {
        subopcode: 0x20,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x21 — i32.atomic.rmw16.add_u
    AtomicOpMeta {
        subopcode: 0x21,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x22 — i64.atomic.rmw8.add_u
    AtomicOpMeta {
        subopcode: 0x22,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x23 — i64.atomic.rmw16.add_u
    AtomicOpMeta {
        subopcode: 0x23,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x24 — i64.atomic.rmw32.add_u
    AtomicOpMeta {
        subopcode: 0x24,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x25 — i32.atomic.rmw.sub
    AtomicOpMeta {
        subopcode: 0x25,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x26 — i64.atomic.rmw.sub
    AtomicOpMeta {
        subopcode: 0x26,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x27 — i32.atomic.rmw8.sub_u
    AtomicOpMeta {
        subopcode: 0x27,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x28 — i32.atomic.rmw16.sub_u
    AtomicOpMeta {
        subopcode: 0x28,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x29 — i64.atomic.rmw8.sub_u
    AtomicOpMeta {
        subopcode: 0x29,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x2A — i64.atomic.rmw16.sub_u
    AtomicOpMeta {
        subopcode: 0x2A,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x2B — i64.atomic.rmw32.sub_u
    AtomicOpMeta {
        subopcode: 0x2B,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x2C — i32.atomic.rmw.and
    AtomicOpMeta {
        subopcode: 0x2C,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x2D — i64.atomic.rmw.and
    AtomicOpMeta {
        subopcode: 0x2D,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x2E — i32.atomic.rmw8.and_u
    AtomicOpMeta {
        subopcode: 0x2E,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x2F — i32.atomic.rmw16.and_u
    AtomicOpMeta {
        subopcode: 0x2F,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x30 — i64.atomic.rmw8.and_u
    AtomicOpMeta {
        subopcode: 0x30,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x31 — i64.atomic.rmw16.and_u
    AtomicOpMeta {
        subopcode: 0x31,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x32 — i64.atomic.rmw32.and_u
    AtomicOpMeta {
        subopcode: 0x32,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x33 — i32.atomic.rmw.or
    AtomicOpMeta {
        subopcode: 0x33,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x34 — i64.atomic.rmw.or
    AtomicOpMeta {
        subopcode: 0x34,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x35 — i32.atomic.rmw8.or_u
    AtomicOpMeta {
        subopcode: 0x35,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x36 — i32.atomic.rmw16.or_u
    AtomicOpMeta {
        subopcode: 0x36,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x37 — i64.atomic.rmw8.or_u
    AtomicOpMeta {
        subopcode: 0x37,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x38 — i64.atomic.rmw16.or_u
    AtomicOpMeta {
        subopcode: 0x38,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x39 — i64.atomic.rmw32.or_u
    AtomicOpMeta {
        subopcode: 0x39,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x3A — i32.atomic.rmw.xor
    AtomicOpMeta {
        subopcode: 0x3A,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x3B — i64.atomic.rmw.xor
    AtomicOpMeta {
        subopcode: 0x3B,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x3C — i32.atomic.rmw8.xor_u
    AtomicOpMeta {
        subopcode: 0x3C,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x3D — i32.atomic.rmw16.xor_u
    AtomicOpMeta {
        subopcode: 0x3D,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x3E — i64.atomic.rmw8.xor_u
    AtomicOpMeta {
        subopcode: 0x3E,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x3F — i64.atomic.rmw16.xor_u
    AtomicOpMeta {
        subopcode: 0x3F,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x40 — i64.atomic.rmw32.xor_u
    AtomicOpMeta {
        subopcode: 0x40,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x41 — i32.atomic.rmw.xchg
    AtomicOpMeta {
        subopcode: 0x41,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x42 — i64.atomic.rmw.xchg
    AtomicOpMeta {
        subopcode: 0x42,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x43 — i32.atomic.rmw8.xchg_u
    AtomicOpMeta {
        subopcode: 0x43,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x44 — i32.atomic.rmw16.xchg_u
    AtomicOpMeta {
        subopcode: 0x44,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32],
        results: &[I32],
    },
    // 0x45 — i64.atomic.rmw8.xchg_u
    AtomicOpMeta {
        subopcode: 0x45,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x46 — i64.atomic.rmw16.xchg_u
    AtomicOpMeta {
        subopcode: 0x46,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64],
        results: &[I64],
    },
    // 0x47 — i64.atomic.rmw32.xchg_u
    AtomicOpMeta {
        subopcode: 0x47,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64],
        results: &[I64],
    },
    // === cmpxchg 0x48–0x4E ===
    // 0x48 — i32.atomic.rmw.cmpxchg
    AtomicOpMeta {
        subopcode: 0x48,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 4,
        params: &[I32, I32, I32],
        results: &[I32],
    },
    // 0x49 — i64.atomic.rmw.cmpxchg
    AtomicOpMeta {
        subopcode: 0x49,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 8,
        params: &[I32, I64, I64],
        results: &[I64],
    },
    // 0x4A — i32.atomic.rmw8.cmpxchg_u
    AtomicOpMeta {
        subopcode: 0x4A,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 1,
        params: &[I32, I32, I32],
        results: &[I32],
    },
    // 0x4B — i32.atomic.rmw16.cmpxchg_u
    AtomicOpMeta {
        subopcode: 0x4B,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I32),
        result_type: Some(NumType::I32),
        access_width: 2,
        params: &[I32, I32, I32],
        results: &[I32],
    },
    // 0x4C — i64.atomic.rmw8.cmpxchg_u
    AtomicOpMeta {
        subopcode: 0x4C,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 1,
        params: &[I32, I64, I64],
        results: &[I64],
    },
    // 0x4D — i64.atomic.rmw16.cmpxchg_u
    AtomicOpMeta {
        subopcode: 0x4D,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 2,
        params: &[I32, I64, I64],
        results: &[I64],
    },
    // 0x4E — i64.atomic.rmw32.cmpxchg_u
    AtomicOpMeta {
        subopcode: 0x4E,
        kind: AtomicKind::Rmw,
        operand_type: Some(NumType::I64),
        result_type: Some(NumType::I64),
        access_width: 4,
        params: &[I32, I64, I64],
        results: &[I64],
    },
];
const I32: ValType = ValType::Num(NumType::I32);
const I64: ValType = ValType::Num(NumType::I64);

/// The kind of atomic operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicKind {
    /// `memory.atomic.notify` — pops (count, address), returns i32.
    Notify,
    /// `memory.atomic.wait32` — pops (timeout, expected_i32, address), returns i32.
    Wait32,
    /// `memory.atomic.wait64` — pops (timeout, expected_i64, address), returns i32.
    Wait64,
    /// `atomic.fence` — no operands, no result; one reserved 0x00 immediate byte.
    Fence,
    /// An atomic load — pops address, pushes result.
    Load,
    /// An atomic store — pops (value, address), no result.
    Store,
    /// A read-modify-write — pops (operand, address), pushes old value.
    Rmw,
}

/// Per-operation metadata for a spec-encoded atomic instruction.
#[derive(Debug, Clone, Copy)]
pub struct AtomicOpMeta {
    /// The spec subopcode byte (the byte after the 0xFE prefix).
    pub subopcode: u8,
    /// The kind of operation.
    pub kind: AtomicKind,
    /// The operand type (the type of the value loaded/stored/rmw'd).
    /// `None` for notify/wait/fence.
    pub operand_type: Option<NumType>,
    /// The result type pushed on the stack.
    /// `None` for stores and fence.
    pub result_type: Option<NumType>,
    /// The access width in bytes (1, 2, 4, or 8). `0` for fence.
    pub access_width: u32,
    /// Parameter types in stack order (top-of-stack last so the validator can
    /// pop in reverse by iterating front-to-back).
    pub params: &'static [ValType],
    /// Result types pushed on the stack.
    pub results: &'static [ValType],
}

impl AtomicOpMeta {
    /// Returns the number of stack operands consumed.
    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    /// Returns whether this op has a result (pushes a value).
    pub fn has_result(&self) -> bool {
        !self.results.is_empty()
    }

    /// Returns a freshly-allocated `FunctionType` for this operation.
    pub fn func_type(&self) -> FunctionType {
        FunctionType::new(self.params.to_vec(), self.results.to_vec())
    }
}

/// Returns the number of immediate bytes that follow the subopcode for this
/// operation.
/// All atomic memory instructions have one memarg (2 LEB128 values: align +
/// offset). `atomic.fence` has one reserved 0x00 byte.
pub fn immediate_count(subopcode: u8) -> Option<usize> {
    let op = lookup(subopcode)?;
    match op.kind {
        AtomicKind::Fence => Some(1), // reserved 0x00 byte
        _ => Some(2),                 // align LEB128 + offset LEB128
    }
}

/// Looks up the atomic operation metadata for a given 0xFE subopcode.
pub fn lookup(subopcode: u8) -> Option<&'static AtomicOpMeta> {
    ATOMIC_OPS.iter().find(|op| op.subopcode == subopcode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notify_encoding() {
        let op = lookup(0x00).unwrap();
        assert_eq!(op.kind, AtomicKind::Notify);
        assert_eq!(op.access_width, 4);
        assert_eq!(op.param_count(), 2);
        assert!(op.has_result());
    }

    #[test]
    fn test_wait32_encoding() {
        let op = lookup(0x01).unwrap();
        assert_eq!(op.kind, AtomicKind::Wait32);
        // Stack (bottom→top): address (i32), expected (i32), timeout (i64)
        assert_eq!(op.param_count(), 3);
        assert_eq!(op.params[0], I32);
        assert_eq!(op.params[1], I32);
        assert_eq!(op.params[2], I64);
    }

    #[test]
    fn test_wait64_encoding() {
        let op = lookup(0x02).unwrap();
        assert_eq!(op.kind, AtomicKind::Wait64);
        // Stack (bottom→top): address (i32), expected (i64), timeout (i64)
        assert_eq!(op.param_count(), 3);
        assert_eq!(op.params[1], I64);
    }

    #[test]
    fn test_fence_encoding() {
        let op = lookup(0x03).unwrap();
        assert_eq!(op.kind, AtomicKind::Fence);
        assert_eq!(op.access_width, 0);
        assert!(!op.has_result());
        assert_eq!(immediate_count(0x03), Some(1));
    }

    #[test]
    fn test_loads() {
        // i32.atomic.load = 0x10
        let op = lookup(0x10).unwrap();
        assert_eq!(op.kind, AtomicKind::Load);
        assert_eq!(op.result_type, Some(NumType::I32));
        assert_eq!(op.access_width, 4);

        // i64.atomic.load8_u = 0x14
        let op = lookup(0x14).unwrap();
        assert_eq!(op.result_type, Some(NumType::I64));
        assert_eq!(op.access_width, 1);
    }

    #[test]
    fn test_stores() {
        // i64.atomic.store32 = 0x1D
        let op = lookup(0x1D).unwrap();
        assert_eq!(op.kind, AtomicKind::Store);
        assert_eq!(op.operand_type, Some(NumType::I64));
        assert_eq!(op.access_width, 4);
        assert!(!op.has_result());
    }

    #[test]
    fn test_rmw_add() {
        // i32.atomic.rmw.add = 0x1E
        let op = lookup(0x1E).unwrap();
        assert_eq!(op.kind, AtomicKind::Rmw);
        assert_eq!(op.operand_type, Some(NumType::I32));
        assert_eq!(op.access_width, 4);
    }

    #[test]
    fn test_cmpxchg() {
        // i32.atomic.rmw.cmpxchg = 0x48
        let op = lookup(0x48).unwrap();
        assert_eq!(op.kind, AtomicKind::Rmw);
        assert_eq!(op.param_count(), 3); // replacement, expected, address
        assert!(op.has_result());

        // i64.atomic.rmw.cmpxchg = 0x49
        let op = lookup(0x49).unwrap();
        assert_eq!(op.operand_type, Some(NumType::I64));
    }

    #[test]
    fn test_unknown_subopcode() {
        assert!(lookup(0x05).is_none());
        assert!(lookup(0x4F).is_none());
    }

    #[test]
    fn test_immediate_count() {
        assert_eq!(immediate_count(0x03), Some(1)); // fence
        assert_eq!(immediate_count(0x10), Some(2)); // load
        assert_eq!(immediate_count(0x00), Some(2)); // notify
    }
}
