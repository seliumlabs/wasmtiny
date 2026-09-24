//! Compiler-side module and per-function environments that bind WebAssembly
//! modules to the wasmtiny runtime's calling convention and per-instance
//! context layout.
//!
//! # Calling convention
//!
//! Every compiled function, direct-call target, and indirect-call target uses
//! the same signature shape: a hidden `VMContext` pointer (passed as the first
//! argument) followed by the WebAssembly parameters, returning the WebAssembly
//! results. The `VMContext` points at per-instance state laid out as described
//! in [`VmCtxOffsets`].
//!
//! # Shared ABI (must match the runtime loader)
//!
//! The offsets below are *the* ABI contract between this compiler and the
//! runtime's `src/aot/` execution glue. The runtime crate does not link this
//! compiler, so the offsets are deliberately duplicated (and documented) on
//! the runtime side rather than shared as a type.
//!
//! # Wasm translation
//!
//! The standalone `cranelift-wasm` crate is discontinued (its last release,
//! 0.112, is version-incompatible with the current `cranelift-codegen`), so
//! this crate owns the wasm→CLIF translation: `wasmparser` validates and
//! parses, `crate::translate` drives the operator stream, and [`FuncEnv`]
//! lowers every runtime-facing construct (calls, memories, tables, globals,
//! libcalls) against the vmctx layout.
//!
//! Parts of `FuncEnv`'s lowering surface (notably `TableData` and its
//! `prepare_table_addr` bounds check, and the former `FuncEnvironment` hook
//! shapes) are derived from `cranelift-wasm` (Apache-2.0 WITH
//! LLVM-exception); those parts remain under that license — see the
//! attribution note at the top of `crate::translate`.

use std::collections::HashMap;

use cranelift_codegen::{
    cursor::FuncCursor,
    ir::immediates::Offset32,
    ir::{self, AbiParam, InstBuilder, Signature, Value, types},
    isa::CallConv,
};
use cranelift_entity::{EntityRef, PrimaryMap};
use cranelift_frontend::FunctionBuilder;
use wasmparser::{ValType, WasmFeatures};

use crate::{
    error::CompileError,
    types::{ConstExpr, Global, GlobalIndex, Memory, MemoryIndex, Table, TableIndex},
};

// Re-export the index types used across the crate (they are defined in
// `crate::types` because they pre-date the translation module split).
pub use crate::types::{DefinedFuncIndex, FuncIndex, TypeIndex};

/// Size in bytes of a single global cell.
pub const GLOBAL_CELL_SIZE: i32 = 8;
pub const USER_TRAP_BAD_SIGNATURE: u8 = 4;
pub const USER_TRAP_CALL_INDIRECT_NULL: u8 = 3;
pub const USER_TRAP_HOST: u8 = 6;
pub const USER_TRAP_MEMORY_LIMIT: u8 = 7;
pub const USER_TRAP_NULL_REFERENCE: u8 = 5;
pub const USER_TRAP_TABLE_OUT_OF_BOUNDS: u8 = 2;
/// User trap codes (encoded in `TrapCode::User(n)`), mapped to the artifact's
/// trap-code bytes by `artifact::trap_code_byte`. The built-in trap codes
/// (`STACK_OVERFLOW`, `HEAP_OUT_OF_BOUNDS`, `INTEGER_OVERFLOW`,
/// `INTEGER_DIVISION_BY_ZERO`, `BAD_CONVERSION_TO_INTEGER`) carry the rest of
/// the runtime's trap taxonomy.
pub const USER_TRAP_UNREACHABLE: u8 = 1;

/// Offset of the shared-memory base pointer inside the hidden context.
///
/// Layout of the `VmCtx` region (all offsets are in bytes, sizeof(ptr)=8):
///
/// ```text
///   0: memories    — pointer to array of `MemoryDesc` (see `MemoryDescOffsets`)
///   8: tables      — pointer to array of table slots (`TableSlotOffsets`):
///                    one pointer per table index to the shared cells holder
///                    (`TableCellsOffsets`); all instances that can reach a
///                    table share the same holder
///  16: globals     — pointer to array of global cells (8 bytes each)
///  24: funcs       — pointer to array of `FuncDesc` (module-local: imports first)
///  32: store_funcs — pointer to store-wide `FuncDesc` array (native handles)
///  40: type_ids    — pointer to canonical signature ids (per module type index)
///  48: libcalls    — pointer to the runtime libcall table
///  56: stack_limit — usize: lowest allowed stack pointer before trap
///  64: dispatch    — opaque per-instance pointer (owned by the runtime)
///  72: refs        — pointer to an array of store-native funcref handles,
///                    one `u32` per function index (imports first)
///  80: stack_pointer — u32: the shadow-stack pointer. Compiled
///                    `global.get`/`global.set` of the module's
///                    `__stack_pointer` global read/write this field directly
///                    instead of the globals array, so a concurrent invocation
///                    can give every thread its own stack by copying the
///                    context and adjusting only this field (see the runtime
///                    `VmCtx` docs for the full contract).
/// ```
pub struct VmCtxOffsets;

/// Slots in the runtime libcall table (all plain `extern "C"` fn pointers).
pub struct LibCallOffsets;

/// Layout of a single function descriptor (`FuncDesc`, size 24 bytes), a
/// "fat call target" carrying the callee's own vmctx so cross-module and
/// imported calls dispatch correctly.
///
/// ```text
///   0: entry    — pointer to the function's native entry (or trampoline)
///   8: vmctx    — pointer to the callee's own `VmCtx`
///  16: type_id  — canonical signature id (u32)
/// ```
pub struct FuncDescOffsets;

/// Layout of a single memory descriptor (size 24 bytes).
///
/// ```text
///   0: base      — pointer to the start of linear memory
///   8: len       — current accessible length in bytes
///  16: capacity  — reserved length in bytes (upper bound)
/// ```
pub struct MemoryDescOffsets;

/// Layout of one slot in the vmctx `tables` array (size 8 bytes): a pointer
/// to the shared [`TableCells`](crate::environment) holder for that table
/// index. All instances that can reach a table (defined or imported) share a
/// single holder, so a `table.grow` performed by any instance is observed by
/// every compiled caller.
pub struct TableSlotOffsets;

/// Layout of a shared table-cells holder (`TableCells`, size 16 bytes).
///
/// ```text
///   0: base — pointer to the element array (immutable after instantiation)
///   8: len  — current element count (u32; advances on table.grow)
/// ```
///
/// `base` is immutable because the cell storage is capacity-reserved at
/// creation and never reallocated; only `len` mutates. Loads of `len` must
/// not be marked readonly: `table.grow` can change it mid-function.
pub struct TableCellsOffsets;

/// A function import declaration.
pub struct FuncImport {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub field: String,
    /// Signature type index.
    pub type_index: TypeIndex,
}

/// A table import declaration.
pub struct TableImport {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub field: String,
    /// Table type.
    pub table: Table,
}

/// A memory import declaration.
pub struct MemoryImport {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub field: String,
    /// Memory type.
    pub memory: Memory,
}

/// A global import declaration.
pub struct GlobalImport {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub field: String,
    /// Global type.
    pub global: Global,
}

/// Placement mode for a data segment.
pub enum DataSegKind {
    /// Active segment initialising `memory_index` at instantiation.
    Active {
        /// Target memory index.
        memory_index: MemoryIndex,
        /// Offset base global, if any.
        base: Option<GlobalIndex>,
        /// Constant offset.
        offset: u64,
    },
    /// Passive segment (for `memory.init`).
    Passive,
}

/// A data segment, in its position within the unified segment index space.
pub struct DataSegRecord {
    /// Placement mode.
    pub kind: DataSegKind,
    /// Segment bytes.
    pub data: Vec<u8>,
}

/// Placement mode for an element segment.
pub enum ElemSegKind {
    /// Active segment initialising `table_index` at instantiation.
    Active {
        /// Target table index.
        table_index: TableIndex,
        /// Offset base global, if any.
        base: Option<GlobalIndex>,
        /// Constant offset.
        offset: u32,
    },
    /// Passive segment (for `table.init`).
    Passive,
    /// Declarative segment (validation-only).
    Declarative,
}

/// An element segment, in its position within the unified segment index space.
pub struct ElemSegRecord {
    /// Placement mode.
    pub kind: ElemSegKind,
    /// Function indices (or `FuncIndex::reserved_value()` for `ref.null`)
    /// stored in the segment.
    pub elements: Vec<crate::types::FuncIndex>,
}

/// The module-level state gathered during translation.
pub struct ModuleInfo {
    /// Target description relevant to frontends producing Cranelift IR.
    pub config: cranelift_codegen::isa::TargetFrontendConfig,
    /// Calling convention used for all functions.
    pub call_conv: CallConv,
    /// Signatures indexed by wasm signature type index (no `vmctx` parameter).
    pub signatures: PrimaryMap<TypeIndex, Signature>,
    /// Original wasm function types, preserving reference-type distinctions
    /// that the CLIF signatures erase (funcref vs externref).
    pub wasm_types: PrimaryMap<TypeIndex, wasmparser::FuncType>,
    /// Function type index for each function (imported first, then defined).
    pub functions: PrimaryMap<crate::types::FuncIndex, TypeIndex>,
    /// Imported functions.
    pub imported_funcs: Vec<FuncImport>,
    /// Imported tables.
    pub imported_tables: Vec<TableImport>,
    /// Imported memories.
    pub imported_memories: Vec<MemoryImport>,
    /// Imported globals.
    pub imported_globals: Vec<GlobalImport>,
    /// Tables (combined imported + defined index space).
    pub tables: PrimaryMap<TableIndex, Table>,
    /// Memories (combined imported + defined index space).
    pub memories: PrimaryMap<MemoryIndex, Memory>,
    /// Globals (combined imported + defined index space), with initialisers.
    /// Imports have `None`; defined globals carry their const initialiser.
    pub globals: PrimaryMap<GlobalIndex, (Global, Option<ConstExpr>)>,
    /// Translated machine-function bodies, indexed by defined function.
    pub function_bodies: PrimaryMap<crate::types::DefinedFuncIndex, ir::Function>,
    /// Function exports.
    pub func_exports: Vec<(crate::types::FuncIndex, String)>,
    /// Table exports.
    pub table_exports: Vec<(TableIndex, String)>,
    /// Memory exports.
    pub memory_exports: Vec<(MemoryIndex, String)>,
    /// Global exports.
    pub global_exports: Vec<(GlobalIndex, String)>,
    /// The optional start function.
    pub start_func: Option<crate::types::FuncIndex>,
    /// Data segments in unified index order (active and passive interleaved).
    pub data_segments: Vec<DataSegRecord>,
    /// Element segments in unified index order (active, passive, declarative).
    pub elem_segments: Vec<ElemSegRecord>,
}

/// The compiler-side module state, driving module-level translation and
/// accumulating everything needed to emit an artifact.
pub struct Translator {
    /// Module information gathered from declarations.
    pub info: ModuleInfo,
    /// Function body translator.
    pub trans: crate::translate::FuncTranslator,
    /// Global index to treat as the shadow-stack pointer even when the module
    /// does not export `__stack_pointer` (see
    /// [`CompilerConfig::shadow_stack_global`](crate::CompilerConfig)).
    pub stack_pointer_override: Option<GlobalIndex>,
}

/// A runtime-facing global variable: the base pointer plus per-cell offset.
#[derive(Clone, Copy)]
pub struct GlobalVar {
    /// CLIF value materialising the `vmctx.globals` base pointer.
    pub base: Value,
    /// Byte offset of this global's 8-byte cell.
    pub offset: Offset32,
    /// CLIF type of the cell.
    pub ty: ir::Type,
}

/// Size of a WebAssembly table, in elements.
#[derive(Clone)]
pub enum TableSize {
    /// Non-resizable table.
    Static {
        /// Non-resizable tables have a constant size known at compile time.
        bound: u32,
    },
    /// Resizable table.
    Dynamic {
        /// Resizable tables hold the current element count in a CLIF value.
        bound: Value,
    },
}

/// A runtime-facing table: the element-array base plus its bounds.
#[derive(Clone)]
pub struct TableData {
    /// CLIF value materialising the table's element-array base pointer.
    pub base: Value,
    /// The size of the table, in elements.
    pub bound: TableSize,
    /// The size of a table element, in bytes.
    pub element_size: u32,
}

/// Per-function translation environment, borrowing immutable module info while
/// accumulating function-scoped declarations (memoised signatures and call
/// targets, which are Cranelift function-level entities rather than SSA values).
///
/// Note: table and global *addresses* are deliberately not memoised here. The
/// vmctx-derived base pointers are ordinary CLIF `Value`s defined at the point
/// of first use, and reusing one across basic blocks would not be valid: the
/// defining block does not necessarily dominate later uses, producing a
/// Cranelift verifier dominance error. Each lowering re-materialises those
/// loads at its own insertion point instead.
pub struct FuncEnv<'info> {
    mod_info: &'info ModuleInfo,
    /// Global index to treat as the shadow-stack pointer even when the module
    /// does not export `__stack_pointer`.
    stack_pointer_override: Option<GlobalIndex>,
    indirect_sigs: HashMap<TypeIndex, (ir::SigRef, usize)>,
    direct_funcs: HashMap<FuncIndex, (ir::FuncRef, usize)>,
}

impl VmCtxOffsets {
    /// Pointer to the memories array.
    pub const MEMORIES: i32 = 0;
    /// Pointer to the tables array.
    pub const TABLES: i32 = 8;
    /// Pointer to the globals array.
    pub const GLOBALS: i32 = 16;
    /// Pointer to the module-local function-descriptor array.
    pub const FUNCS: i32 = 24;
    /// Pointer to the store-wide function-descriptor array.
    pub const STORE_FUNCS: i32 = 32;
    /// Pointer to the canonical signature-id array.
    pub const TYPE_IDS: i32 = 40;
    /// Pointer to the runtime libcall table.
    pub const LIBCALLS: i32 = 48;
    /// Stack limit (a `usize`).
    pub const STACK_LIMIT: i32 = 56;
    /// Opaque per-instance dispatch state (runtime-owned).
    pub const DISPATCH: i32 = 64;
    /// Pointer to the store-native funcref-handle array (u32 per func index).
    pub const REFS: i32 = 72;
    /// The shadow-stack pointer (u32), written per invocation by the runtime
    /// for modules with a `__stack_pointer` global (see the `VmCtxOffsets`
    /// layout docs).
    pub const STACK_POINTER: i32 = 80;
}

impl LibCallOffsets {
    /// Called by import stubs: `(vmctx, ordinal, args: *mut u64, results: *mut u64) -> status u64`.
    pub const HOST_CALL: i32 = 0;
    /// `memory.atomic.notify`: `(vmctx, mem_idx, addr, count) -> packed u64`.
    pub const ATOMIC_NOTIFY: i32 = 8;
    /// `memory.atomic.wait32`: `(vmctx, mem_idx, addr, expected, timeout) -> packed u64`.
    pub const ATOMIC_WAIT32: i32 = 16;
    /// `memory.atomic.wait64`: `(vmctx, mem_idx, addr, expected, timeout) -> packed u64`.
    pub const ATOMIC_WAIT64: i32 = 24;
    /// `memory.size`: `(vmctx, mem_idx) -> pages`.
    pub const MEMORY_SIZE: i32 = 32;
    /// `memory.grow`: `(vmctx, mem_idx, delta) -> old pages | -1`.
    pub const MEMORY_GROW: i32 = 40;
    /// `memory.copy`: `(vmctx, dst, src, dst_addr, src_addr, len) -> status`.
    pub const MEMORY_COPY: i32 = 48;
    /// `memory.fill`: `(vmctx, mem_idx, dst, val, len) -> status`.
    pub const MEMORY_FILL: i32 = 56;
    /// `memory.init`: `(vmctx, mem_idx, seg_idx, dst, src, len) -> status`.
    pub const MEMORY_INIT: i32 = 64;
    /// `data.drop`: `(vmctx, seg_idx) -> status`.
    pub const DATA_DROP: i32 = 72;
    /// `table.size`: `(vmctx, table_idx) -> elem count`.
    pub const TABLE_SIZE: i32 = 80;
    /// `table.grow`: `(vmctx, table_idx, delta, init) -> old len | -1`.
    pub const TABLE_GROW: i32 = 88;
    /// `table.copy`: `(vmctx, dst, src, dst_idx, src_idx, len) -> status`.
    pub const TABLE_COPY: i32 = 96;
    /// `table.fill`: `(vmctx, table_idx, dst, val, len) -> status`.
    pub const TABLE_FILL: i32 = 104;
    /// `table.init`: `(vmctx, seg_idx, table_idx, dst, src, len) -> status`.
    pub const TABLE_INIT: i32 = 112;
    /// `elem.drop`: `(vmctx, seg_idx) -> status`.
    pub const ELEM_DROP: i32 = 120;
}

impl FuncDescOffsets {
    /// Byte size of one function descriptor.
    pub const SIZE: i32 = 24;
    /// Native entry pointer.
    pub const ENTRY: i32 = 0;
    /// Callee context pointer.
    pub const VMCTX: i32 = 8;
    /// Canonical signature id (u32).
    pub const TYPE_ID: i32 = 16;
}

impl MemoryDescOffsets {
    /// Byte size of one memory descriptor.
    pub const SIZE: i32 = 24;
    /// Start of the memory's data.
    pub const BASE: i32 = 0;
    /// Current accessible length in bytes.
    pub const LEN: i32 = 8;
    /// Reserved capacity in bytes.
    pub const CAPACITY: i32 = 16;
}

impl TableSlotOffsets {
    /// Byte size of one slot.
    pub const SIZE: i32 = 8;
}

impl TableCellsOffsets {
    /// Start of the element array.
    pub const BASE: i32 = 0;
    /// Current element count (u32).
    pub const LEN: i32 = 8;
}

impl ModuleInfo {
    /// Creates a new empty module info.
    pub fn new(config: cranelift_codegen::isa::TargetFrontendConfig, call_conv: CallConv) -> Self {
        Self {
            config,
            call_conv,
            signatures: PrimaryMap::new(),
            wasm_types: PrimaryMap::new(),
            functions: PrimaryMap::new(),
            imported_funcs: Vec::new(),
            imported_tables: Vec::new(),
            imported_memories: Vec::new(),
            imported_globals: Vec::new(),
            tables: PrimaryMap::new(),
            memories: PrimaryMap::new(),
            globals: PrimaryMap::new(),
            function_bodies: PrimaryMap::new(),
            func_exports: Vec::new(),
            table_exports: Vec::new(),
            memory_exports: Vec::new(),
            global_exports: Vec::new(),
            start_func: None,
            data_segments: Vec::new(),
            elem_segments: Vec::new(),
        }
    }

    /// Number of imported functions.
    pub fn imported_func_count(&self) -> usize {
        self.imported_funcs.len()
    }

    /// Number of imported tables.
    pub fn imported_table_count(&self) -> usize {
        self.imported_tables.len()
    }

    /// Number of imported memories.
    pub fn imported_memory_count(&self) -> usize {
        self.imported_memories.len()
    }

    /// Number of imported globals.
    pub fn imported_global_count(&self) -> usize {
        self.imported_globals.len()
    }

    /// Whether `index` is the module's shadow-stack pointer global (the
    /// `__stack_pointer` export). The compiler routes this global's
    /// `get`/`set` through the vmctx `stack_pointer` field instead of the
    /// globals array so the runtime can give each concurrent invocation its
    /// own stack (see [`VmCtxOffsets::STACK_POINTER`]).
    pub fn is_shadow_stack_pointer(&self, index: GlobalIndex) -> bool {
        self.global_exports
            .iter()
            .any(|(exported, name)| *exported == index && name == "__stack_pointer")
    }

    /// The module's shadow-stack pointer global index, when it exports
    /// `__stack_pointer`.
    pub fn shadow_stack_pointer(&self) -> Option<GlobalIndex> {
        self.global_exports
            .iter()
            .find(|(_, name)| name == "__stack_pointer")
            .map(|(index, _)| *index)
    }
}

impl Translator {
    /// Creates a new translator.
    pub fn new(config: cranelift_codegen::isa::TargetFrontendConfig, call_conv: CallConv) -> Self {
        Self {
            info: ModuleInfo::new(config, call_conv),
            trans: crate::translate::FuncTranslator::new(),
            stack_pointer_override: None,
        }
    }

    /// Returns a shared func-environment for translating one function.
    pub fn func_env(&self) -> FuncEnv<'_> {
        FuncEnv::new(&self.info, self.stack_pointer_override)
    }
}

impl TableSize {
    /// Get a CLIF value representing the current bounds of this table.
    pub fn bound(&self, mut pos: FuncCursor, index_ty: ir::Type) -> ir::Value {
        match *self {
            TableSize::Static { bound } => pos.ins().iconst(index_ty, i64::from(bound)),
            TableSize::Dynamic { bound } => bound,
        }
    }
}

impl TableData {
    /// Return a CLIF value containing a native pointer to the beginning of the
    /// given index within this table, plus the flags to use for the access.
    ///
    /// The bounds check uses Spectre mitigation: an out-of-bounds index
    /// selects a null address and the subsequent access traps through the
    /// returned flags' trap code. A null address is used rather than an
    /// explicit branch so the speculative execution does not continue with
    /// the out-of-bounds address.
    pub fn prepare_table_addr(
        &self,
        pos: &mut FunctionBuilder,
        mut index: ir::Value,
        addr_ty: ir::Type,
        enable_table_access_spectre_mitigation: bool,
    ) -> (ir::Value, cranelift_codegen::ir::MemFlagsData) {
        let index_ty = pos.func.dfg.value_type(index);

        // Start with the bounds check. Trap if `index + 1 > bound`.
        let bound = self.bound.bound(pos.cursor(), index_ty);

        // `index > bound - 1` is the same as `index >= bound`.
        let oob = pos.ins().icmp(
            ir::condcodes::IntCC::UnsignedGreaterThanOrEqual,
            index,
            bound,
        );

        if !enable_table_access_spectre_mitigation {
            pos.ins()
                .trapnz(oob, user_trap(USER_TRAP_TABLE_OUT_OF_BOUNDS));
        }

        // Convert `index` to `addr_ty`.
        if index_ty != addr_ty {
            index = pos.ins().uextend(addr_ty, index);
        }

        // Add the table base address base
        let element_size = self.element_size;
        let offset = if element_size == 1 {
            index
        } else if element_size.is_power_of_two() {
            pos.ins()
                .ishl_imm_u(index, i64::from(element_size.trailing_zeros()))
        } else {
            pos.ins().imul_imm_u(index, element_size as i64)
        };

        let element_addr = pos.ins().iadd(self.base, offset);

        if enable_table_access_spectre_mitigation {
            // Short-circuit the computed table element address to a null
            // pointer when out-of-bounds. The consumer of this address will
            // trap when trying to access it.
            let zero = pos.ins().iconst(addr_ty, 0);
            (
                pos.ins().select_spectre_guard(oob, zero, element_addr),
                table_access_flags(),
            )
        } else {
            (element_addr, cranelift_codegen::ir::MemFlagsData::new())
        }
    }
}

impl<'info> FuncEnv<'info> {
    pub(crate) fn new(
        mod_info: &'info ModuleInfo,
        stack_pointer_override: Option<GlobalIndex>,
    ) -> Self {
        Self {
            mod_info,
            stack_pointer_override,
            indirect_sigs: HashMap::new(),
            direct_funcs: HashMap::new(),
        }
    }

    /// The pointer type of the target (always `I64`; 32-bit targets are
    /// rejected by `build_isa`).
    pub fn pointer_type(&self) -> ir::Type {
        match self.mod_info.config.pointer_width {
            target_lexicon::PointerWidth::U64 => types::I64,
            other => panic!("unsupported pointer width {other:?}"),
        }
    }

    /// The target's frontend configuration.
    pub fn frontend_config(&self) -> cranelift_codegen::isa::TargetFrontendConfig {
        self.mod_info.config
    }

    /// The immutable module info being translated.
    pub fn module_info(&self) -> &ModuleInfo {
        self.mod_info
    }

    /// The `vmctx` argument of the function being translated.
    pub fn vmctx_value(&self, func: &ir::Function) -> Value {
        func.special_param(ir::ArgumentPurpose::VMContext)
            .expect("missing vmctx parameter")
    }

    /// Build a signature with a `vmctx` parameter prepended.
    pub fn vmctx_sig(&self, sigidx: TypeIndex) -> Signature {
        let mut sig = self.mod_info.signatures[sigidx].clone();
        let mut params = Vec::with_capacity(sig.params.len() + 1);
        params.push(AbiParam::special(
            self.pointer_type(),
            ir::ArgumentPurpose::VMContext,
        ));
        params.extend(sig.params.iter().cloned());
        sig.params = params;
        sig
    }

    /// Loads `vmctx.<offset>`: a trusted, readonly field of the hidden
    /// context (the field never changes while the function executes).
    fn vmctx_load(&mut self, builder: &mut FunctionBuilder, offset: i32, ty: ir::Type) -> Value {
        let vmctx = self.vmctx_value(builder.func);
        let flags = cranelift_codegen::ir::MemFlagsData::trusted().with_readonly();
        builder.ins().load(ty, flags, vmctx, Offset32::new(offset))
    }

    /// Loads `vmctx.<offset>` from a `FuncCursor`.
    fn vmctx_load_cursor(&mut self, pos: &mut FuncCursor, offset: i32, ty: ir::Type) -> Value {
        let vmctx = pos
            .func
            .special_param(ir::ArgumentPurpose::VMContext)
            .expect("missing vmctx parameter");
        let flags = cranelift_codegen::ir::MemFlagsData::trusted().with_readonly();
        pos.ins().load(ty, flags, vmctx, Offset32::new(offset))
    }

    /// A chained load: `vmctx.<array_field>[elem_offset + field_offset]`
    /// (two memory indirections), for per-instance descriptor arrays.
    fn vmctx_slot(
        &mut self,
        builder: &mut FunctionBuilder,
        array_field: i32,
        elem_offset: i32,
        field_offset: i32,
        ty: ir::Type,
    ) -> Value {
        let array = self.vmctx_load(builder, array_field, self.pointer_type());
        let flags = cranelift_codegen::ir::MemFlagsData::trusted().with_readonly();
        builder
            .ins()
            .load(ty, flags, array, Offset32::new(elem_offset + field_offset))
    }

    /// Returns the `GlobalVar` for `index`.
    ///
    /// The base pointer is re-loaded at the current insertion point on every
    /// call. Caching the loaded `Value` across blocks would be invalid: the
    /// defining block does not necessarily dominate later uses.
    ///
    /// The module's shadow-stack pointer (`__stack_pointer`) is special: its
    /// `get`/`set` read/write the vmctx `stack_pointer` field directly
    /// instead of a globals-array cell. The runtime gives every concurrent
    /// invocation a private stack by copying the context and adjusting only
    /// that field; ordinary globals stay genuinely shared through the
    /// `vmctx.globals` array. Only i32 stack pointers are routed (memory64 is
    /// disabled); anything else keeps the plain-cell behaviour.
    pub fn make_global(
        &mut self,
        builder: &mut FunctionBuilder,
        index: GlobalIndex,
    ) -> crate::error::CompileResult<GlobalVar> {
        let ty = match self.mod_info.globals[index].0.wasm_ty {
            ValType::I32 => types::I32,
            ValType::I64 => types::I64,
            ValType::F32 => types::F32,
            ValType::F64 => types::F64,
            ValType::V128 => types::I8X16,
            ValType::Ref(_) => types::I32,
        };
        if ty == types::I32
            && (self.stack_pointer_override == Some(index)
                || self.mod_info.is_shadow_stack_pointer(index))
        {
            // The shadow-stack pointer lives in the vmctx itself; the
            // translator's `base + offset` load/store shape works unchanged.
            return Ok(GlobalVar {
                base: self.vmctx_value(builder.func),
                offset: Offset32::new(VmCtxOffsets::STACK_POINTER),
                ty: types::I32,
            });
        }
        // Global `index` lives in the 8-byte cell at
        // `vmctx.globals + index * 8`. The base is the loaded `vmctx.globals`
        // pointer; the translator then adds the per-cell offset and
        // loads/stores `ty`.
        let globals_base = self.vmctx_load(builder, VmCtxOffsets::GLOBALS, self.pointer_type());
        let var = GlobalVar {
            base: globals_base,
            offset: Offset32::new((index.index() as i32) * GLOBAL_CELL_SIZE),
            ty,
        };
        Ok(var)
    }

    /// Returns the runtime-facing table data for `index`, re-materialising the
    /// vmctx-derived base and bound loads at the current insertion point.
    ///
    /// The loaded values are ordinary SSA `Value`s and must not be cached
    /// across basic blocks (the defining block would not necessarily dominate
    /// later uses), so every caller lowers a fresh copy here.
    pub fn get_table(
        &mut self,
        builder: &mut FunctionBuilder,
        index: TableIndex,
    ) -> crate::error::CompileResult<TableData> {
        let table = self.mod_info.tables[index];
        // `vmctx.tables[i]` holds a pointer to the shared cells holder; the
        // holder pointer itself is fixed at instantiation, so the slot load
        // may be readonly.
        let slot_offset = (index.index() as i32) * TableSlotOffsets::SIZE;
        let holder = self.vmctx_slot(
            builder,
            VmCtxOffsets::TABLES,
            slot_offset,
            0,
            self.pointer_type(),
        );
        // The element base is immutable after instantiation (the cell storage
        // is capacity-reserved and never reallocated), so this load may be
        // readonly too.
        let readonly_flags = cranelift_codegen::ir::MemFlagsData::trusted().with_readonly();
        let base = builder.ins().load(
            self.pointer_type(),
            readonly_flags,
            holder,
            Offset32::new(TableCellsOffsets::BASE),
        );
        let bound = if table.maximum == Some(table.minimum) {
            TableSize::Static {
                bound: table.minimum,
            }
        } else {
            // The element count advances on `table.grow`; the load must not
            // be readonly or the optimizer could hoist it out of loops and
            // use a stale bound. It is also re-loaded on every access (rather
            // than memoised) so a `table.grow` earlier in the same function is
            // observed by subsequent accesses.
            let trusted_flags = cranelift_codegen::ir::MemFlagsData::trusted();
            TableSize::Dynamic {
                bound: builder.ins().load(
                    types::I32,
                    trusted_flags,
                    holder,
                    Offset32::new(TableCellsOffsets::LEN),
                ),
            }
        };

        Ok(TableData {
            base,
            bound,
            element_size: 4,
        })
    }

    /// Returns the memoised indirect-call signature for `index` and the
    /// number of wasm parameters it carries.
    pub fn make_indirect_sig(
        &mut self,
        func: &mut ir::Function,
        index: TypeIndex,
    ) -> crate::error::CompileResult<(ir::SigRef, usize)> {
        if let Some(sig) = self.indirect_sigs.get(&index) {
            return Ok(*sig);
        }
        let sig_ref = func.import_signature(self.vmctx_sig(index));
        let num_wasm_params = self.mod_info.wasm_types[index].params().len();
        let sig = (sig_ref, num_wasm_params);
        self.indirect_sigs.insert(index, sig);
        Ok(sig)
    }

    /// Returns the memoised direct-call target for `index` and the number of
    /// wasm parameters it carries.
    pub fn make_direct_func(
        &mut self,
        func: &mut ir::Function,
        index: FuncIndex,
    ) -> crate::error::CompileResult<(ir::FuncRef, usize)> {
        if let Some(fref) = self.direct_funcs.get(&index) {
            return Ok(*fref);
        }
        let sigidx = self.mod_info.functions[index];
        let signature = func.import_signature(self.vmctx_sig(sigidx));
        // `index` here is the defined-function ordinal (imported functions are
        // never direct-called: `translate_call` routes them through the hidden
        // context's entry array). Finish-linking resolves this ordinal against
        // the code-image layout.
        let ordinal = index
            .as_u32()
            .saturating_sub(self.mod_info.imported_func_count() as u32);
        let name =
            ir::ExternalName::User(func.declare_imported_user_function(ir::UserExternalName {
                namespace: 0,
                index: ordinal,
            }));
        let fref = func.import_function(ir::ExtFuncData {
            name,
            signature,
            colocated: true,
            patchable: false,
        });
        let num_wasm_params = self.mod_info.wasm_types[sigidx].params().len();
        let entry = (fref, num_wasm_params);
        self.direct_funcs.insert(index, entry);
        Ok(entry)
    }

    /// Prepares a bounds-checked wasm linear-memory address.
    ///
    /// Bounds-checkes the effective byte range `index + offset + access_size`
    /// against the memory's reserved *capacity* (not its current length:
    /// shared regions are mapped top-down inside the same reservation, so the
    /// whole reservation is addressable; accessing the grown-but-unmapped gap
    /// faults and is mapped to `MemoryOutOfBounds` by the runtime trap
    /// handler, D9). With Spectre mitigations enabled the out-of-bounds
    /// address is clamped to null and the subsequent access traps through the
    /// load/store's trap record; otherwise an explicit `trapnz` is emitted.
    ///
    /// Returns `(address, flags)` ready for a load or store with a zero
    /// offset immediate — the memarg offset is folded into the address.
    pub fn memory_addr(
        &mut self,
        builder: &mut FunctionBuilder,
        mem_index: MemoryIndex,
        index: Value,
        offset: u64,
        access_size: u32,
    ) -> crate::error::CompileResult<(Value, cranelift_codegen::ir::MemFlagsData)> {
        let base = mem_index.index() as i32 * MemoryDescOffsets::SIZE;
        let base_ptr = self.vmctx_slot(
            builder,
            VmCtxOffsets::MEMORIES,
            base,
            MemoryDescOffsets::BASE,
            self.pointer_type(),
        );
        let bound = self.vmctx_slot(
            builder,
            VmCtxOffsets::MEMORIES,
            base,
            MemoryDescOffsets::CAPACITY,
            self.pointer_type(),
        );

        let pointer_type = self.pointer_type();
        let index64 = builder.ins().uextend(pointer_type, index);

        // `index64 + offset + access_size > bound` ⟺ out of bounds. The
        // addition cannot overflow: `index64 < 2^32` (memory64 is disabled)
        // and `offset + access_size <= 2^32 + 8`.
        let limit = builder
            .ins()
            .iadd_imm_s(index64, (offset + u64::from(access_size)) as i64);
        let oob = builder
            .ins()
            .icmp(ir::condcodes::IntCC::UnsignedGreaterThan, limit, bound);

        let addr = builder.ins().iadd(base_ptr, index64);
        let addr = if offset == 0 {
            addr
        } else {
            builder.ins().iadd_imm_s(addr, offset as i64)
        };

        let flags = mem_access_flags();
        if self.mod_info.config.pointer_width == target_lexicon::PointerWidth::U64 {
            let null = builder.ins().iconst(pointer_type, 0);
            let addr = builder.ins().select_spectre_guard(oob, null, addr);
            Ok((addr, flags))
        } else {
            // 32-bit pointers are rejected earlier, but keep the explicit
            // check as a defensive fallback.
            builder.ins().trapnz(oob, ir::TrapCode::HEAP_OUT_OF_BOUNDS);
            Ok((addr, flags))
        }
    }

    /// Returns the number of wasm parameters of a function type index.
    pub fn wasm_param_count(&self, sigidx: TypeIndex) -> usize {
        self.mod_info.wasm_types[sigidx].params().len()
    }

    /// Translates a direct `call`.
    pub fn translate_call(
        &mut self,
        builder: &mut FunctionBuilder,
        callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[Value],
    ) -> crate::error::CompileResult<ir::Inst> {
        let vmctx = builder
            .func
            .special_param(ir::ArgumentPurpose::VMContext)
            .expect("missing vmctx parameter");

        if (callee_index.index() as u32) < self.mod_info.imported_func_count() as u32 {
            // Imported function: load its fat call target (entry + callee
            // vmctx) from the hidden context's per-module function array and
            // perform an indirect call with the *callee's* vmctx.
            let sigidx = self.mod_info.functions[callee_index];
            let sig_ref = builder.func.import_signature(self.vmctx_sig(sigidx));
            let funcs_base = self.vmctx_load(builder, VmCtxOffsets::FUNCS, self.pointer_type());
            let slot_ptr = builder.ins().iadd_imm_s(
                funcs_base,
                i64::from((callee_index.index() as i32) * FuncDescOffsets::SIZE),
            );
            let flags = cranelift_codegen::ir::MemFlagsData::trusted();
            let callee_ptr =
                builder
                    .ins()
                    .load(self.pointer_type(), flags, slot_ptr, FuncDescOffsets::ENTRY);
            let callee_vmctx =
                builder
                    .ins()
                    .load(self.pointer_type(), flags, slot_ptr, FuncDescOffsets::VMCTX);
            let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
            args.push(callee_vmctx);
            args.extend_from_slice(call_args);
            return Ok(builder.ins().call_indirect(sig_ref, callee_ptr, &args));
        }

        let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
        args.push(vmctx);
        args.extend_from_slice(call_args);
        Ok(builder.ins().call(callee, &args))
    }

    /// Translates a `call_indirect`.
    ///
    /// Returns `None` when the call is in unreachable code and no
    /// instructions may be emitted.
    pub fn translate_call_indirect(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        sig_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: Value,
        call_args: &[Value],
    ) -> crate::error::CompileResult<Option<ir::Inst>> {
        // Bounds-check the table index, then load the 4-byte funcref handle.
        let table = self.get_table(builder, table_index)?;
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, callee, pointer_type, true);
        let handle = builder.ins().load(types::I32, flags, table_entry_addr, 0);

        // Null entry traps.
        let is_null = builder
            .ins()
            .icmp_imm_u(ir::condcodes::IntCC::Equal, handle, 0);
        builder
            .ins()
            .trapnz(is_null, user_trap(USER_TRAP_CALL_INDIRECT_NULL));

        // Resolve the store-native handle to its fat call target in the
        // store-wide descriptor table; the callee's own vmctx rides along so
        // cross-module and imported calls dispatch correctly.
        let funcs_base = self.vmctx_load(builder, VmCtxOffsets::STORE_FUNCS, pointer_type);
        let handle_ext = builder.ins().uextend(pointer_type, handle);
        let slot_offset = builder
            .ins()
            .imul_imm_u(handle_ext, i64::from(FuncDescOffsets::SIZE));
        let slot_ptr = builder.ins().iadd(funcs_base, slot_offset);
        let flags = cranelift_codegen::ir::MemFlagsData::trusted();
        let func_ptr = builder
            .ins()
            .load(pointer_type, flags, slot_ptr, FuncDescOffsets::ENTRY);
        let callee_vmctx =
            builder
                .ins()
                .load(pointer_type, flags, slot_ptr, FuncDescOffsets::VMCTX);

        // Dynamic signature check: `store_funcs[handle].type_id` must equal the
        // canonical id of the expected signature (from `vmctx.type_ids`).
        let actual_type_id =
            builder
                .ins()
                .load(types::I32, flags, slot_ptr, FuncDescOffsets::TYPE_ID);
        let type_ids_base = self.vmctx_load(builder, VmCtxOffsets::TYPE_IDS, pointer_type);
        let expected_slot = builder
            .ins()
            .iadd_imm_s(type_ids_base, i64::from(sig_index.index() as i32) * 4);
        let expected_type_id = builder.ins().load(types::I32, flags, expected_slot, 0);
        let mismatch = builder.ins().icmp(
            ir::condcodes::IntCC::NotEqual,
            actual_type_id,
            expected_type_id,
        );
        builder
            .ins()
            .trapnz(mismatch, user_trap(USER_TRAP_BAD_SIGNATURE));

        let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
        args.push(callee_vmctx);
        args.extend_from_slice(call_args);

        Ok(Some(builder.ins().call_indirect(sig_ref, func_ptr, &args)))
    }

    /// Entry-time stack-overflow check: trap if the stack pointer has dropped
    /// below the limit stored in the hidden context. This bounds wasm
    /// recursion before the host stack is exhausted, and with a dedicated
    /// signal alt-stack the trap is always recoverable.
    pub fn before_translate_function(
        &mut self,
        builder: &mut FunctionBuilder,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let limit = self.vmctx_load(builder, VmCtxOffsets::STACK_LIMIT, pointer_type);
        let stack_pointer = builder.ins().get_stack_pointer(pointer_type);
        let overflow =
            builder
                .ins()
                .icmp(ir::condcodes::IntCC::UnsignedLessThan, stack_pointer, limit);
        builder.ins().trapnz(overflow, ir::TrapCode::STACK_OVERFLOW);
        Ok(())
    }

    /// Translates `memory.grow`.
    ///
    /// The libcall reports failure two ways: `u32::MAX` (-1) for a grow the
    /// declaration rejects, and the packed trap sentinel for a runtime
    /// memory-budget overrun (`MemoryLimitExceeded`, enforced atomically with
    /// the growth commit inside the runtime's grow critical section).
    pub fn translate_memory_grow(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        val: Value,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let delta = widen_u32(&mut pos, val, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::MEMORY_GROW,
            &[mem_idx, delta],
        );
        Ok(unpack_libcall_result(
            &mut pos,
            raw,
            user_trap(USER_TRAP_MEMORY_LIMIT),
        ))
    }

    /// Translates `memory.size`.
    pub fn translate_memory_size(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::MEMORY_SIZE,
            &[mem_idx],
        );
        Ok(unpack_plain_result(&mut pos, raw))
    }

    /// Translates `memory.copy`.
    pub fn translate_memory_copy(
        &mut self,
        mut pos: FuncCursor,
        src_index: MemoryIndex,
        dst_index: MemoryIndex,
        dst: Value,
        src: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let dst_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(dst_index.as_u32()));
        let src_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(src_index.as_u32()));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let src64 = widen_u32(&mut pos, src, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::MEMORY_COPY,
            &[dst_idx, src_idx, dst64, src64, len64],
        );
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HEAP_OUT_OF_BOUNDS);
        Ok(())
    }

    /// Translates `memory.fill`.
    pub fn translate_memory_fill(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        dst: Value,
        val: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let val64 = widen_u32(&mut pos, val, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::MEMORY_FILL,
            &[mem_idx, dst64, val64, len64],
        );
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HEAP_OUT_OF_BOUNDS);
        Ok(())
    }

    /// Translates `memory.init`.
    pub fn translate_memory_init(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        seg_index: u32,
        dst: Value,
        src: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let seg_idx = pos.ins().iconst(pointer_type, i64::from(seg_index));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let src64 = widen_u32(&mut pos, src, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::MEMORY_INIT,
            &[mem_idx, seg_idx, dst64, src64, len64],
        );
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HEAP_OUT_OF_BOUNDS);
        Ok(())
    }

    /// Translates `data.drop`.
    pub fn translate_data_drop(
        &mut self,
        mut pos: FuncCursor,
        seg_index: u32,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let seg_idx = pos.ins().iconst(pointer_type, i64::from(seg_index));
        let _ = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::DATA_DROP,
            &[seg_idx],
        );
        Ok(())
    }

    /// Translates `table.size`.
    pub fn translate_table_size(
        &mut self,
        mut pos: FuncCursor,
        index: TableIndex,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        let table_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::TABLE_SIZE,
            &[table_idx],
        );
        Ok(unpack_plain_result(&mut pos, raw))
    }

    /// Translates `table.grow`.
    pub fn translate_table_grow(
        &mut self,
        mut pos: FuncCursor,
        table_index: TableIndex,
        delta: Value,
        init_value: Value,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        let table_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(table_index.as_u32()));
        let delta64 = widen_u32(&mut pos, delta, pointer_type);
        let init64 = widen_u32(&mut pos, init_value, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::TABLE_GROW,
            &[table_idx, delta64, init64],
        );
        Ok(unpack_plain_result(&mut pos, raw))
    }

    /// Translates `table.get`.
    pub fn translate_table_get(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        index: Value,
    ) -> crate::error::CompileResult<Value> {
        let table = self.get_table(builder, table_index)?;
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, index, pointer_type, true);
        Ok(builder.ins().load(types::I32, flags, table_entry_addr, 0))
    }

    /// Translates `table.set`.
    pub fn translate_table_set(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        value: Value,
        index: Value,
    ) -> crate::error::CompileResult<()> {
        let table = self.get_table(builder, table_index)?;
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, index, pointer_type, true);
        builder.ins().store(flags, value, table_entry_addr, 0);
        Ok(())
    }

    /// Translates `table.copy`.
    pub fn translate_table_copy(
        &mut self,
        mut pos: FuncCursor,
        dst_table_index: TableIndex,
        src_table_index: TableIndex,
        dst: Value,
        src: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let dst_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(dst_table_index.as_u32()));
        let src_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(src_table_index.as_u32()));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let src64 = widen_u32(&mut pos, src, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::TABLE_COPY,
            &[dst_idx, src_idx, dst64, src64, len64],
        );
        unpack_libcall_result(&mut pos, raw, user_trap(USER_TRAP_TABLE_OUT_OF_BOUNDS));
        Ok(())
    }

    /// Translates `table.fill`.
    pub fn translate_table_fill(
        &mut self,
        mut pos: FuncCursor,
        table_index: TableIndex,
        dst: Value,
        val: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let table_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(table_index.as_u32()));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let val64 = widen_u32(&mut pos, val, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::TABLE_FILL,
            &[table_idx, dst64, val64, len64],
        );
        unpack_libcall_result(&mut pos, raw, user_trap(USER_TRAP_TABLE_OUT_OF_BOUNDS));
        Ok(())
    }

    /// Translates `table.init`.
    pub fn translate_table_init(
        &mut self,
        mut pos: FuncCursor,
        seg_index: u32,
        table_index: TableIndex,
        dst: Value,
        src: Value,
        len: Value,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let seg_idx = pos.ins().iconst(pointer_type, i64::from(seg_index));
        let table_idx = pos
            .ins()
            .iconst(pointer_type, i64::from(table_index.as_u32()));
        let dst64 = widen_u32(&mut pos, dst, pointer_type);
        let src64 = widen_u32(&mut pos, src, pointer_type);
        let len64 = widen_u32(&mut pos, len, pointer_type);
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::TABLE_INIT,
            &[seg_idx, table_idx, dst64, src64, len64],
        );
        unpack_libcall_result(&mut pos, raw, user_trap(USER_TRAP_TABLE_OUT_OF_BOUNDS));
        Ok(())
    }

    /// Translates `elem.drop`.
    pub fn translate_elem_drop(
        &mut self,
        mut pos: FuncCursor,
        seg_index: u32,
    ) -> crate::error::CompileResult<()> {
        let pointer_type = self.pointer_type();
        let seg_idx = pos.ins().iconst(pointer_type, i64::from(seg_index));
        let _ = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::ELEM_DROP,
            &[seg_idx],
        );
        Ok(())
    }

    /// Translates `ref.null`.
    ///
    /// References are raw `u32` handles; the null handle is zero regardless of
    /// the heap type.
    pub fn translate_ref_null(&mut self, mut pos: FuncCursor, _hty: wasmparser::HeapType) -> Value {
        pos.ins().iconst(types::I32, 0)
    }

    /// Translates `ref.is_null`.
    pub fn translate_ref_is_null(&mut self, mut pos: FuncCursor, value: Value) -> Value {
        let is_null = pos.ins().icmp_imm_u(ir::condcodes::IntCC::Equal, value, 0);
        pos.ins().uextend(types::I32, is_null)
    }

    /// Translates `ref.func`.
    ///
    /// A funcref is a store-native handle, looked up from the per-module
    /// `refs` array (one u32 per function index).
    pub fn translate_ref_func(
        &mut self,
        mut pos: FuncCursor,
        func_index: FuncIndex,
    ) -> crate::error::CompileResult<Value> {
        let refs_base = self.vmctx_load_cursor(&mut pos, VmCtxOffsets::REFS, self.pointer_type());
        let slot = pos
            .ins()
            .iadd_imm_s(refs_base, i64::from(func_index.index() as i32) * 4);
        let flags = cranelift_codegen::ir::MemFlagsData::trusted();
        Ok(pos.ins().load(types::I32, flags, slot, 0))
    }

    /// Translates `memory.atomic.wait32/64` (runtime libcall).
    pub fn translate_atomic_wait(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        addr: Value,
        offset: u64,
        expected: Value,
        timeout: Value,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        // Fold the memarg offset into the effective address with a wrapping
        // i32 add (memory32 semantics), exactly as loads/stores do — the
        // runtime compares and registers the waiter against this address.
        let addr = pos.ins().iadd_imm_s(addr, offset as i64);
        // The translator leaves the expected value at its native width: i32 for
        // `wait32`, i64 for `wait64`. The runtime compares sign-extended i32s,
        // so sign-extend the narrower value before the libcall.
        let is_64 = pos.func.dfg.value_type(expected) == types::I64;
        let (slot, expected64) = if is_64 {
            (LibCallOffsets::ATOMIC_WAIT64, expected)
        } else {
            (
                LibCallOffsets::ATOMIC_WAIT32,
                pos.ins().sextend(pointer_type, expected),
            )
        };
        let addr64 = pos.ins().uextend(pointer_type, addr);
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            slot,
            &[mem_idx, addr64, expected64, timeout],
        );
        Ok(unpack_libcall_result(
            &mut pos,
            raw,
            ir::TrapCode::HEAP_OUT_OF_BOUNDS,
        ))
    }

    /// Translates `memory.atomic.notify` (runtime libcall).
    pub fn translate_atomic_notify(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        addr: Value,
        offset: u64,
        count: Value,
    ) -> crate::error::CompileResult<Value> {
        let pointer_type = self.pointer_type();
        // Fold the memarg offset into the effective address (see
        // `translate_atomic_wait`).
        let addr = pos.ins().iadd_imm_s(addr, offset as i64);
        let addr64 = pos.ins().uextend(pointer_type, addr);
        let count64 = pos.ins().uextend(pointer_type, count);
        let mem_idx = pos.ins().iconst(pointer_type, i64::from(index.as_u32()));
        let raw = emit_libcall(
            &mut pos,
            self.mod_info.call_conv,
            pointer_type,
            LibCallOffsets::ATOMIC_NOTIFY,
            &[mem_idx, addr64, count64],
        );
        Ok(unpack_libcall_result(
            &mut pos,
            raw,
            ir::TrapCode::HEAP_OUT_OF_BOUNDS,
        ))
    }
}

/// The `MemFlagsData` for a wasm linear-memory access: traps
/// `HEAP_OUT_OF_BOUNDS`. WebAssembly alignment is a hint, not a guarantee, so
/// the access is never marked aligned; the explicit bounds check
/// (spectre-guarded null address) traps before the access, and a fault here is
/// still mapped to the same trap by the runtime's signal handler.
pub fn mem_access_flags() -> cranelift_codegen::ir::MemFlagsData {
    cranelift_codegen::ir::MemFlagsData::new()
        .with_trap_code(Some(ir::TrapCode::HEAP_OUT_OF_BOUNDS))
}

/// The `MemFlagsData` for a wasm table access: traps `TABLE_OUT_OF_BOUNDS`.
pub fn table_access_flags() -> cranelift_codegen::ir::MemFlagsData {
    cranelift_codegen::ir::MemFlagsData::new()
        .with_trap_code(Some(user_trap(USER_TRAP_TABLE_OUT_OF_BOUNDS)))
}

/// Convert a translation failure into a compiler error.
pub fn translate_error(err: crate::translate::TranslateError) -> CompileError {
    match err {
        crate::translate::TranslateError::Unsupported(msg) => CompileError::Unsupported(msg),
        other => CompileError::Translate(other.to_string()),
    }
}

/// Creates the `TrapCode` for a user trap constant.
pub fn user_trap(code: u8) -> ir::TrapCode {
    ir::TrapCode::unwrap_user(code)
}

/// The WebAssembly features enabled for this compiler.
///
/// This is the v1 feature gate: core spec plus bulk memory, reference types,
/// multi-value, sign extension, saturating float-to-int, mutable globals,
/// floats, and threads. SIMD, relaxed SIMD, GC, exception handling, tail
/// calls, extended-const, multi-memory, memory64, **typed function
/// references** and the component model are disabled, so modules using them
/// are rejected by validation with an explicit unsupported-feature error.
///
/// `function_references` is deliberately disabled: it matches the
/// interpreter's coverage (whose parser accepts only the shorthand
/// `funcref`/`externref` reference types) and keeps typed references out of
/// the artifact's type section — typed and untyped funcref signatures would
/// otherwise serialise identically and `call_indirect`'s dynamic signature
/// check could not tell them apart. `extended_const` remains disabled because
/// the compiler's const-expression writer does not lower those operators.
pub fn wasm_features() -> WasmFeatures {
    let mut features = WasmFeatures::empty();
    features.set(WasmFeatures::MUTABLE_GLOBAL, true);
    features.set(WasmFeatures::SATURATING_FLOAT_TO_INT, true);
    features.set(WasmFeatures::SIGN_EXTENSION, true);
    features.set(WasmFeatures::REFERENCE_TYPES, true);
    features.set(WasmFeatures::MULTI_VALUE, true);
    features.set(WasmFeatures::BULK_MEMORY, true);
    features.set(WasmFeatures::THREADS, true);
    features.set(WasmFeatures::FLOATS, true);
    // `gc_types` only admits the `externref`/`funcref` *types* that the
    // reference-types proposal already provides (the `gc` feature, which gates
    // GC *operators*, remains disabled). Without it, wasmparser 0.259 rejects
    // `funcref`/`externref` outright.
    features.set(WasmFeatures::GC_TYPES, true);
    features
}

/// Converts a wasm function type into a `vmctx`-augmented CLIF signature
/// (used by the trampoline builder).
pub fn wasm_func_type_to_sig(call_conv: CallConv, ty: &wasmparser::FuncType) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.extend(
        ty.params()
            .iter()
            .map(|ty| AbiParam::new(crate::types::valtype_to_clif(*ty))),
    );
    sig.returns.extend(
        ty.results()
            .iter()
            .map(|ty| AbiParam::new(crate::types::valtype_to_clif(*ty))),
    );
    sig
}

/// The native CLIF type used for each wasm value type (used by the trampoline
/// builder too).
pub fn wasm_type_to_clif(ty: ValType) -> ir::Type {
    crate::types::valtype_to_clif(ty)
}

/// Loads a runtime libcall `(vmctx: ptr, ...u64) -> u64` from the context's
/// libcall table and invokes it, returning the raw packed result value.
fn emit_libcall(
    pos: &mut FuncCursor,
    call_conv: CallConv,
    pointer_type: ir::Type,
    slot: i32,
    args: &[Value],
) -> Value {
    let vmctx = pos
        .func
        .special_param(ir::ArgumentPurpose::VMContext)
        .expect("missing vmctx parameter");
    let readonly_flags = cranelift_codegen::ir::MemFlagsData::trusted().with_readonly();
    let base = pos.ins().load(
        pointer_type,
        readonly_flags,
        vmctx,
        Offset32::new(VmCtxOffsets::LIBCALLS),
    );
    let slot_ptr = pos.ins().iadd_imm_s(base, i64::from(slot));
    let fn_ptr = pos.ins().load(
        pointer_type,
        cranelift_codegen::ir::MemFlagsData::trusted(),
        slot_ptr,
        0,
    );

    let mut sig = ir::Signature::new(call_conv);
    sig.params.push(AbiParam::new(pointer_type));
    for _ in args {
        sig.params.push(AbiParam::new(pointer_type));
    }
    sig.returns.push(AbiParam::new(pointer_type));
    let sig_ref = pos.func.import_signature(sig);

    let mut actual = Vec::with_capacity(args.len() + 1);
    actual.push(vmctx);
    actual.extend_from_slice(args);
    let call = pos.ins().call_indirect(sig_ref, fn_ptr, &actual);
    pos.func.dfg.first_result(call)
}

/// Unpacks a libcall's packed `(trap:u32 << 32) | (result:u32)` return value,
/// trapping with `trap` when the high word is non-zero.
fn unpack_libcall_result(pos: &mut FuncCursor, raw: Value, trap: ir::TrapCode) -> Value {
    let trap_word = pos.ins().ushr_imm_u(raw, 32);
    let is_trap = pos
        .ins()
        .icmp_imm_u(ir::condcodes::IntCC::NotEqual, trap_word, 0);
    pos.ins().trapnz(is_trap, trap);
    pos.ins().ireduce(types::I32, raw)
}

/// Unpacks a non-trapping libcall result: the low 32 bits as a signed `i32`.
fn unpack_plain_result(pos: &mut FuncCursor, raw: Value) -> Value {
    pos.ins().ireduce(types::I32, raw)
}

/// Widens an `i32` operand to the pointer width for a libcall argument.
fn widen_u32(pos: &mut FuncCursor, value: Value, pointer_type: ir::Type) -> Value {
    if pos.func.dfg.value_type(value) == types::I32 {
        pos.ins().uextend(pointer_type, value)
    } else {
        value
    }
}
