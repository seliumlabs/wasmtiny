//! Index types and WebAssembly type mirrors owned by the compiler.
//!
//! The wasm→CLIF translation is self-contained (the standalone
//! `cranelift-wasm` crate is discontinued and version-incompatible with the
//! current `cranelift-codegen`), so the compiler defines its own entity index
//! types — implementing `cranelift_entity::EntityRef` so `PrimaryMap` /
//! `SecondaryMap` work — plus small owned mirrors of the wasm type-system
//! values that the artifact writer and trampoline builder need after the
//! borrowed `wasmparser` views have been dropped.

use cranelift_codegen::ir;
use cranelift_entity::entity_impl;
use wasmparser::{GlobalType, HeapType, MemoryType, RefType, TableType, ValType};

/// A single linear-memory declaration, converted from [`wasmparser::MemoryType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// Initial size in wasm pages.
    pub minimum: u64,
    /// Optional maximum size in wasm pages.
    pub maximum: Option<u64>,
    /// Whether this is a shared memory (threads proposal).
    pub shared: bool,
    /// Log2 of the custom page size (16 for the default 64KiB pages).
    pub page_size_log2: u32,
}

/// A single table declaration, converted from [`wasmparser::TableType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    /// The table's element reference type.
    pub wasm_ty: RefType,
    /// Initial size in elements.
    pub minimum: u32,
    /// Optional maximum size in elements.
    pub maximum: Option<u32>,
}

/// A single global declaration, converted from [`wasmparser::GlobalType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Global {
    /// The global's value type.
    pub wasm_ty: ValType,
    /// Whether the global is mutable.
    pub mutability: bool,
}

/// One operation of a constant initialiser expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstOp {
    /// `i32.const`
    I32Const(i32),
    /// `i64.const`
    I64Const(i64),
    /// `f32.const` (bit pattern).
    F32Const(u32),
    /// `f64.const` (bit pattern).
    F64Const(u64),
    /// `global.get`
    GlobalGet(u32),
    /// `ref.null` with the heap-type byte.
    RefNull(u8),
    /// `ref.func`
    RefFunc(u32),
    /// `v128.const` (bit pattern).
    V128Const(u128),
    /// `ref.i31`
    RefI31(i32),
}

/// An owned constant initialiser expression, parsed from a
/// [`wasmparser::ConstExpr`] so the artifact writer does not borrow the input
/// wasm bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstExpr {
    /// The operations of the expression (without the trailing `end`).
    pub ops: Vec<ConstOp>,
}

impl Memory {
    /// Maximum size in bytes, or `None` if unbounded.
    pub fn maximum_byte_size(&self) -> Option<u64> {
        self.maximum.map(|pages| pages << self.page_size_log2)
    }
}

impl From<MemoryType> for Memory {
    fn from(ty: MemoryType) -> Self {
        Self {
            minimum: ty.initial,
            maximum: ty.maximum,
            shared: ty.shared,
            page_size_log2: ty.page_size_log2.unwrap_or(16),
        }
    }
}

impl From<TableType> for Table {
    fn from(ty: TableType) -> Self {
        Self {
            wasm_ty: ty.element_type,
            minimum: ty.initial as u32,
            maximum: ty.maximum.map(|max| max as u32),
        }
    }
}

impl From<GlobalType> for Global {
    fn from(ty: GlobalType) -> Self {
        Self {
            wasm_ty: ty.content_type,
            mutability: ty.mutable,
        }
    }
}

impl ConstExpr {
    /// Parses a borrowed `wasmparser` const expression into an owned one.
    ///
    /// Only the operators reachable in the v1 feature set are accepted
    /// (`extended_const` is disabled, so arity-2 constants such as
    /// `i32.add` are rejected by validation before this is ever called).
    pub fn parse(expr: &wasmparser::ConstExpr) -> Result<Self, String> {
        use wasmparser::Operator;
        let mut ops = Vec::new();
        for op in expr.get_operators_reader() {
            let op = op.map_err(|e| e.to_string())?;
            match op {
                Operator::I32Const { value } => ops.push(ConstOp::I32Const(value)),
                Operator::I64Const { value } => ops.push(ConstOp::I64Const(value)),
                Operator::F32Const { value } => ops.push(ConstOp::F32Const(value.bits())),
                Operator::F64Const { value } => ops.push(ConstOp::F64Const(value.bits())),
                Operator::GlobalGet { global_index } => {
                    ops.push(ConstOp::GlobalGet(global_index));
                }
                Operator::RefNull { hty } => {
                    ops.push(ConstOp::RefNull(ref_heap_type_byte(hty)));
                }
                Operator::RefFunc { function_index } => {
                    ops.push(ConstOp::RefFunc(function_index));
                }
                Operator::RefI31 { .. } => ops.push(ConstOp::RefI31(0)),
                Operator::End => break,
                other => {
                    return Err(format!(
                        "unsupported constant expression operator {other:?}"
                    ));
                }
            }
        }
        Ok(Self { ops })
    }
}

macro_rules! index_type {
    ($name:ident) => {
        #[doc = concat!("A wasm index of kind `", stringify!($name), "`.")]
        #[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u32);

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        entity_impl!($name);
    };
}

index_type!(FuncIndex);

index_type!(TypeIndex);

index_type!(TableIndex);

index_type!(MemoryIndex);

index_type!(GlobalIndex);

index_type!(DefinedFuncIndex);

/// Encodes a wasm heap type as its standard byte.
pub fn ref_heap_type_byte(hty: HeapType) -> u8 {
    match hty {
        HeapType::Abstract { ty, .. } => match ty {
            wasmparser::AbstractHeapType::Extern => 0x6F,
            wasmparser::AbstractHeapType::Func => 0x70,
            // Any other abstract heap type (any, eq, i31, ...) is GC-only
            // and rejected by validation before reaching the writer.
            _other => 0x70,
        },
        // Concrete and exact heap types belong to the function-references /
        // custom-descriptors proposals, both rejected before translation.
        HeapType::Concrete(_) | HeapType::Exact(_) => 0x70,
    }
}

/// Encodes a wasm reference type as its standard heap-type byte.
pub fn ref_type_byte(ty: RefType) -> u8 {
    ref_heap_type_byte(ty.heap_type())
}

/// Encodes a wasm value type as its standard type byte.
pub fn valtype_byte(ty: ValType) -> u8 {
    match ty {
        ValType::I32 => 0x7F,
        ValType::I64 => 0x7E,
        ValType::F32 => 0x7D,
        ValType::F64 => 0x7C,
        ValType::V128 => 0x7B,
        ValType::Ref(reference) => ref_type_byte(reference),
    }
}

/// The CLIF type used to represent a wasm value type.
pub fn valtype_to_clif(ty: ValType) -> ir::Type {
    match ty {
        ValType::I32 => ir::types::I32,
        ValType::I64 => ir::types::I64,
        ValType::F32 => ir::types::F32,
        ValType::F64 => ir::types::F64,
        ValType::V128 => ir::types::I8X16,
        // Reference values are raw `u32` handles in wasmtiny.
        ValType::Ref(_) => ir::types::I32,
    }
}
