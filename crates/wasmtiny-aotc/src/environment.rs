//! Cranelift `ModuleEnvironment` / `FuncEnvironment` implementations that bind
//! WebAssembly modules to the wasmtiny runtime's calling convention and
//! per-instance context layout.
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

use cranelift_codegen::{
    cursor::FuncCursor,
    ir::immediates::Offset32,
    ir::{self, AbiParam, InstBuilder, MemFlags, Signature, UserFuncName, Value, types},
    isa::{CallConv, TargetFrontendConfig},
};
use cranelift_entity::{EntityRef, PrimaryMap, SecondaryMap};
use cranelift_wasm::{
    ConstExpr, DataIndex, DefinedFuncIndex, ElemIndex, EngineOrModuleTypeIndex, FuncIndex,
    FuncTranslator, FunctionBuilder, Global, GlobalIndex, GlobalVariable, Heap, HeapData,
    HeapStyle, Memory, MemoryIndex, ModuleEnvironment, ModuleInternedTypeIndex, Table, TableData,
    TableIndex, TableSize, TargetEnvironment, TypeConvert, TypeIndex, WasmError, WasmFuncType,
    WasmHeapType, WasmResult, WasmValType,
};
use wasmparser::{FuncValidator, FunctionBody, UnpackedIndex, ValidatorResources, WasmFeatures};

use crate::error::CompileError;

/// Size in bytes of a single global cell.
pub const GLOBAL_CELL_SIZE: i32 = 8;

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
    /// Function indices (or `reserved` for null) stored in the segment.
    pub elements: Vec<FuncIndex>,
}

/// The module-level state gathered during translation.
pub struct ModuleInfo {
    /// Target description relevant to frontends producing Cranelift IR.
    pub config: TargetFrontendConfig,
    /// Calling convention used for all functions.
    pub call_conv: CallConv,
    /// Signatures indexed by wasm signature type index (no `vmctx` parameter).
    pub signatures: PrimaryMap<TypeIndex, Signature>,
    /// Original wasm function types, preserving reference-type distinctions
    /// that the CLIF signatures erase (funcref vs externref).
    pub wasm_types: PrimaryMap<TypeIndex, WasmFuncType>,
    /// Function type index for each function (imported first, then defined).
    pub functions: PrimaryMap<FuncIndex, TypeIndex>,
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
    pub function_bodies: PrimaryMap<DefinedFuncIndex, ir::Function>,
    /// Function exports.
    pub func_exports: Vec<(FuncIndex, String)>,
    /// Table exports.
    pub table_exports: Vec<(TableIndex, String)>,
    /// Memory exports.
    pub memory_exports: Vec<(MemoryIndex, String)>,
    /// Global exports.
    pub global_exports: Vec<(GlobalIndex, String)>,
    /// The optional start function.
    pub start_func: Option<FuncIndex>,
    /// Data segments in unified index order (active and passive interleaved).
    pub data_segments: Vec<DataSegRecord>,
    /// Element segments in unified index order (active, passive, declarative).
    pub elem_segments: Vec<ElemSegRecord>,
}

/// The compiler-side `ModuleEnvironment`, driving module-level translation and
/// accumulating everything needed to emit an artifact.
pub struct Translator {
    /// Module information gathered from declarations.
    pub info: ModuleInfo,
    /// Function body translator.
    pub trans: FuncTranslator,
}

/// Per-function translation environment, borrowing immutable module info while
/// accumulating function-local state (heaps, tables maps).
pub struct FuncEnv<'info> {
    mod_info: &'info ModuleInfo,
    heaps: PrimaryMap<Heap, HeapData>,
    tables: SecondaryMap<TableIndex, Option<TableData>>,
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
    pub fn new(config: TargetFrontendConfig, call_conv: CallConv) -> Self {
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
}

impl Translator {
    /// Creates a new translator.
    pub fn new(config: TargetFrontendConfig, call_conv: CallConv) -> Self {
        Self {
            info: ModuleInfo::new(config, call_conv),
            trans: FuncTranslator::new(),
        }
    }

    /// Returns a shared func-environment for translating one function.
    pub fn func_env(&self) -> FuncEnv<'_> {
        FuncEnv::new(&self.info)
    }
}

impl TypeConvert for Translator {
    fn lookup_heap_type(&self, index: UnpackedIndex) -> WasmHeapType {
        match index.as_module_index() {
            Some(idx) => {
                let idx = ModuleInternedTypeIndex::from_u32(idx);
                WasmHeapType::ConcreteFunc(EngineOrModuleTypeIndex::Module(idx))
            }
            None => WasmHeapType::Func,
        }
    }

    fn lookup_type_index(&self, index: UnpackedIndex) -> EngineOrModuleTypeIndex {
        match index.as_module_index() {
            Some(idx) => EngineOrModuleTypeIndex::Module(ModuleInternedTypeIndex::from_u32(idx)),
            None => EngineOrModuleTypeIndex::Module(ModuleInternedTypeIndex::from_u32(0)),
        }
    }
}

impl TargetEnvironment for Translator {
    fn target_config(&self) -> TargetFrontendConfig {
        self.info.config
    }

    fn heap_access_spectre_mitigation(&self) -> bool {
        true
    }

    fn proof_carrying_code(&self) -> bool {
        false
    }
}

impl<'data> ModuleEnvironment<'data> for Translator {
    fn declare_type_func(&mut self, wasm: WasmFuncType) -> WasmResult<()> {
        let mut sig = Signature::new(self.info.call_conv);
        sig.params.extend(
            wasm.params()
                .iter()
                .map(|ty| AbiParam::new(wasm_type_to_clif(ty))),
        );
        sig.returns.extend(
            wasm.returns()
                .iter()
                .map(|ty| AbiParam::new(wasm_type_to_clif(ty))),
        );
        self.info.wasm_types.push(wasm.clone());
        self.info.signatures.push(sig);
        Ok(())
    }

    fn declare_func_import(
        &mut self,
        index: TypeIndex,
        module: &'data str,
        field: &'data str,
    ) -> WasmResult<()> {
        self.info.functions.push(index);
        self.info.imported_funcs.push(FuncImport {
            module: module.to_string(),
            field: field.to_string(),
            type_index: index,
        });
        Ok(())
    }

    fn declare_table_import(
        &mut self,
        table: Table,
        module: &'data str,
        field: &'data str,
    ) -> WasmResult<()> {
        self.info.tables.push(table);
        self.info.imported_tables.push(TableImport {
            module: module.to_string(),
            field: field.to_string(),
            table,
        });
        Ok(())
    }

    fn declare_memory_import(
        &mut self,
        memory: Memory,
        module: &'data str,
        field: &'data str,
    ) -> WasmResult<()> {
        self.info.memories.push(memory);
        self.info.imported_memories.push(MemoryImport {
            module: module.to_string(),
            field: field.to_string(),
            memory,
        });
        Ok(())
    }

    fn declare_global_import(
        &mut self,
        global: Global,
        module: &'data str,
        field: &'data str,
    ) -> WasmResult<()> {
        self.info.globals.push((global, None));
        self.info.imported_globals.push(GlobalImport {
            module: module.to_string(),
            field: field.to_string(),
            global,
        });
        Ok(())
    }

    fn declare_func_type(&mut self, index: TypeIndex) -> WasmResult<()> {
        self.info.functions.push(index);
        Ok(())
    }

    fn declare_table(&mut self, table: Table) -> WasmResult<()> {
        self.info.tables.push(table);
        Ok(())
    }

    fn declare_memory(&mut self, memory: Memory) -> WasmResult<()> {
        self.info.memories.push(memory);
        Ok(())
    }

    fn declare_global(&mut self, global: Global, init: ConstExpr) -> WasmResult<()> {
        self.info.globals.push((global, Some(init)));
        Ok(())
    }

    fn declare_func_export(&mut self, func_index: FuncIndex, name: &'data str) -> WasmResult<()> {
        self.info.func_exports.push((func_index, name.to_string()));
        Ok(())
    }

    fn declare_table_export(
        &mut self,
        table_index: TableIndex,
        name: &'data str,
    ) -> WasmResult<()> {
        self.info
            .table_exports
            .push((table_index, name.to_string()));
        Ok(())
    }

    fn declare_memory_export(
        &mut self,
        memory_index: MemoryIndex,
        name: &'data str,
    ) -> WasmResult<()> {
        self.info
            .memory_exports
            .push((memory_index, name.to_string()));
        Ok(())
    }

    fn declare_global_export(
        &mut self,
        global_index: GlobalIndex,
        name: &'data str,
    ) -> WasmResult<()> {
        self.info
            .global_exports
            .push((global_index, name.to_string()));
        Ok(())
    }

    fn declare_start_func(&mut self, index: FuncIndex) -> WasmResult<()> {
        self.info.start_func = Some(index);
        Ok(())
    }

    fn declare_table_elements(
        &mut self,
        table_index: TableIndex,
        base: Option<GlobalIndex>,
        offset: u32,
        elements: Box<[FuncIndex]>,
    ) -> WasmResult<()> {
        self.info.elem_segments.push(ElemSegRecord {
            kind: ElemSegKind::Active {
                table_index,
                base,
                offset,
            },
            elements: elements.to_vec(),
        });
        Ok(())
    }

    fn declare_passive_element(
        &mut self,
        _index: ElemIndex,
        elements: Box<[FuncIndex]>,
    ) -> WasmResult<()> {
        self.info.elem_segments.push(ElemSegRecord {
            kind: ElemSegKind::Passive,
            elements: elements.to_vec(),
        });
        Ok(())
    }

    fn declare_elements(&mut self, elements: Box<[FuncIndex]>) -> WasmResult<()> {
        self.info.elem_segments.push(ElemSegRecord {
            kind: ElemSegKind::Declarative,
            elements: elements.to_vec(),
        });
        Ok(())
    }

    fn declare_passive_data(
        &mut self,
        _data_index: DataIndex,
        data: &'data [u8],
    ) -> WasmResult<()> {
        self.info.data_segments.push(DataSegRecord {
            kind: DataSegKind::Passive,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn define_function_body(
        &mut self,
        mut validator: FuncValidator<ValidatorResources>,
        body: FunctionBody<'data>,
    ) -> WasmResult<()> {
        let func_index = FuncIndex::from_u32(
            (self.info.imported_func_count() + self.info.function_bodies.len()) as u32,
        );
        let type_index = self.info.functions[func_index];
        let sig = FuncEnv::new(&self.info).vmctx_sig(type_index);
        let defined_index = self.info.function_bodies.len();
        let mut func =
            ir::Function::with_name_signature(UserFuncName::user(0, defined_index as u32), sig);

        let mut func_env = FuncEnv::new(&self.info);
        self.trans
            .translate_body(&mut validator, body, &mut func, &mut func_env)?;
        self.info.function_bodies.push(func);
        Ok(())
    }

    fn declare_data_initialization(
        &mut self,
        memory_index: MemoryIndex,
        base: Option<GlobalIndex>,
        offset: u64,
        data: &'data [u8],
    ) -> WasmResult<()> {
        self.info.data_segments.push(DataSegRecord {
            kind: DataSegKind::Active {
                memory_index,
                base,
                offset,
            },
            data: data.to_vec(),
        });
        Ok(())
    }

    fn wasm_features(&self) -> WasmFeatures {
        wasm_features()
    }
}

impl<'info> FuncEnv<'info> {
    fn new(mod_info: &'info ModuleInfo) -> Self {
        Self {
            mod_info,
            heaps: PrimaryMap::new(),
            tables: SecondaryMap::new(),
        }
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

    fn vmctx_global(&self, func: &mut ir::Function, offset: i32, ty: ir::Type) -> ir::GlobalValue {
        let vmctx = func.create_global_value(ir::GlobalValueData::VMContext);
        func.create_global_value(ir::GlobalValueData::Load {
            base: vmctx,
            offset: Offset32::new(offset),
            global_type: ty,
            flags: MemFlags::trusted().with_readonly(),
        })
    }

    /// A chained load: `vmctx.<array_field>[elem_offset + field_offset]`
    /// (two memory indirections), for per-instance descriptor arrays.
    fn vmctx_slot(
        &self,
        func: &mut ir::Function,
        array_field: i32,
        elem_offset: i32,
        field_offset: i32,
        ty: ir::Type,
    ) -> ir::GlobalValue {
        let array = self.vmctx_global(func, array_field, self.pointer_type());
        func.create_global_value(ir::GlobalValueData::Load {
            base: array,
            offset: Offset32::new(elem_offset + field_offset),
            global_type: ty,
            flags: MemFlags::trusted().with_readonly(),
        })
    }

    fn ensure_table(&mut self, func: &mut ir::Function, index: TableIndex) {
        if self.tables[index].is_some() {
            return;
        }

        let table = self.mod_info.tables[index];
        // `vmctx.tables[i]` holds a pointer to the shared cells holder; the
        // holder pointer itself is fixed at instantiation, so the slot load
        // may be readonly.
        let slot_offset = (index.index() as i32) * TableSlotOffsets::SIZE;
        let holder = self.vmctx_slot(
            func,
            VmCtxOffsets::TABLES,
            slot_offset,
            0,
            self.pointer_type(),
        );
        // The element base is immutable after instantiation (the cell storage
        // is capacity-reserved and never reallocated), so this load may be
        // readonly too.
        let base_gv = func.create_global_value(ir::GlobalValueData::Load {
            base: holder,
            offset: Offset32::new(TableCellsOffsets::BASE),
            global_type: self.pointer_type(),
            flags: MemFlags::trusted().with_readonly(),
        });
        let bound = if table.maximum == Some(table.minimum) {
            TableSize::Static {
                bound: table.minimum,
            }
        } else {
            // The element count advances on `table.grow`; the load must not
            // be readonly or the optimizer could hoist it out of loops and
            // use a stale bound.
            TableSize::Dynamic {
                bound_gv: func.create_global_value(ir::GlobalValueData::Load {
                    base: holder,
                    offset: Offset32::new(TableCellsOffsets::LEN),
                    global_type: types::I32,
                    flags: MemFlags::trusted(),
                }),
            }
        };

        self.tables[index] = Some(TableData {
            base_gv,
            bound,
            element_size: 4,
        });
    }
}

impl TypeConvert for FuncEnv<'_> {
    fn lookup_heap_type(&self, index: UnpackedIndex) -> WasmHeapType {
        match index.as_module_index() {
            Some(idx) => {
                let idx = ModuleInternedTypeIndex::from_u32(idx);
                WasmHeapType::ConcreteFunc(EngineOrModuleTypeIndex::Module(idx))
            }
            None => WasmHeapType::Func,
        }
    }

    fn lookup_type_index(&self, index: UnpackedIndex) -> EngineOrModuleTypeIndex {
        match index.as_module_index() {
            Some(idx) => EngineOrModuleTypeIndex::Module(ModuleInternedTypeIndex::from_u32(idx)),
            None => EngineOrModuleTypeIndex::Module(ModuleInternedTypeIndex::from_u32(0)),
        }
    }
}

impl TargetEnvironment for FuncEnv<'_> {
    fn target_config(&self) -> TargetFrontendConfig {
        self.mod_info.config
    }

    fn heap_access_spectre_mitigation(&self) -> bool {
        true
    }

    fn proof_carrying_code(&self) -> bool {
        false
    }

    fn reference_type(&self, _ty: WasmHeapType) -> ir::Type {
        // References are raw `u32` handles; see `wasm_type_to_clif`.
        types::I32
    }
}

impl cranelift_wasm::FuncEnvironment for FuncEnv<'_> {
    fn make_global(
        &mut self,
        func: &mut ir::Function,
        index: GlobalIndex,
    ) -> WasmResult<GlobalVariable> {
        let ty = match self.mod_info.globals[index].0.wasm_ty {
            WasmValType::I32 => types::I32,
            WasmValType::I64 => types::I64,
            WasmValType::F32 => types::F32,
            WasmValType::F64 => types::F64,
            WasmValType::V128 => types::I8X16,
            WasmValType::Ref(_) => types::I32,
        };
        // Global `index` lives in the 8-byte cell at
        // `vmctx.globals + index * 8`. The base global value is a pointer load
        // of `vmctx.globals`; cranelift then adds the per-cell offset and
        // loads/stores `ty`.
        let globals_base = self.vmctx_global(func, VmCtxOffsets::GLOBALS, self.pointer_type());
        Ok(GlobalVariable::Memory {
            gv: globals_base,
            offset: Offset32::new((index.index() as i32) * GLOBAL_CELL_SIZE),
            ty,
        })
    }

    fn heaps(&self) -> &PrimaryMap<Heap, HeapData> {
        &self.heaps
    }

    fn make_heap(&mut self, func: &mut ir::Function, index: MemoryIndex) -> WasmResult<Heap> {
        let memory = self.mod_info.memories[index];
        let base = index.index() as i32 * MemoryDescOffsets::SIZE;
        let base_gv = self.vmctx_slot(
            func,
            VmCtxOffsets::MEMORIES,
            base,
            MemoryDescOffsets::BASE,
            self.pointer_type(),
        );
        // Bound the heap against the reservation *capacity*, not the current
        // length: shared regions are mapped top-down inside the same
        // reservation, so the whole reservation is addressable. Accessing the
        // grown-but-unmapped gap or a read-only shared page faults and is
        // mapped to `MemoryOutOfBounds` by the runtime trap handler (D9).
        let bound_gv = self.vmctx_slot(
            func,
            VmCtxOffsets::MEMORIES,
            base,
            MemoryDescOffsets::CAPACITY,
            self.pointer_type(),
        );
        let max_size = memory.maximum_byte_size().unwrap_or(u64::from(u32::MAX));

        Ok(self.heaps.push(HeapData {
            base: base_gv,
            min_size: 0,
            max_size: Some(max_size),
            offset_guard_size: 0x1_0000_0000,
            style: HeapStyle::Dynamic { bound_gv },
            index_type: types::I32,
            memory_type: None,
            page_size_log2: memory.page_size_log2,
        }))
    }

    fn make_indirect_sig(
        &mut self,
        func: &mut ir::Function,
        index: TypeIndex,
    ) -> WasmResult<ir::SigRef> {
        Ok(func.import_signature(self.vmctx_sig(index)))
    }

    fn make_direct_func(
        &mut self,
        func: &mut ir::Function,
        index: FuncIndex,
    ) -> WasmResult<ir::FuncRef> {
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
        Ok(func.import_function(ir::ExtFuncData {
            name,
            signature,
            colocated: true,
        }))
    }

    fn translate_call(
        &mut self,
        builder: &mut FunctionBuilder,
        callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[Value],
    ) -> WasmResult<ir::Inst> {
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
            let funcs_base_gv =
                self.vmctx_global(builder.func, VmCtxOffsets::FUNCS, self.pointer_type());
            let funcs_base = builder
                .ins()
                .global_value(self.pointer_type(), funcs_base_gv);
            let slot_ptr = builder.ins().iadd_imm(
                funcs_base,
                i64::from((callee_index.index() as i32) * FuncDescOffsets::SIZE),
            );
            let callee_ptr = builder.ins().load(
                self.pointer_type(),
                MemFlags::trusted(),
                slot_ptr,
                FuncDescOffsets::ENTRY,
            );
            let callee_vmctx = builder.ins().load(
                self.pointer_type(),
                MemFlags::trusted(),
                slot_ptr,
                FuncDescOffsets::VMCTX,
            );
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

    fn translate_return_call(
        &mut self,
        builder: &mut FunctionBuilder,
        _callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[Value],
    ) -> WasmResult<()> {
        let vmctx = builder
            .func
            .special_param(ir::ArgumentPurpose::VMContext)
            .expect("missing vmctx parameter");
        let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
        args.push(vmctx);
        args.extend_from_slice(call_args);
        builder.ins().return_call(callee, &args);
        Ok(())
    }

    fn translate_call_indirect(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        sig_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: Value,
        call_args: &[Value],
    ) -> WasmResult<Option<ir::Inst>> {
        // Bounds-check the table index, then load the 4-byte funcref handle.
        self.ensure_table(builder.func, table_index);
        let table = self.tables[table_index].as_ref().unwrap();
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, callee, pointer_type, true);
        let handle = builder.ins().load(types::I32, flags, table_entry_addr, 0);

        // Null entry traps.
        let is_null = builder
            .ins()
            .icmp_imm(ir::condcodes::IntCC::Equal, handle, 0);
        builder
            .ins()
            .trapnz(is_null, ir::TrapCode::IndirectCallToNull);

        // Resolve the store-native handle to its fat call target in the
        // store-wide descriptor table; the callee's own vmctx rides along so
        // cross-module and imported calls dispatch correctly.
        let funcs_gv = self.vmctx_global(builder.func, VmCtxOffsets::STORE_FUNCS, pointer_type);
        let funcs_base = builder.ins().global_value(pointer_type, funcs_gv);
        let handle_ext = builder.ins().uextend(pointer_type, handle);
        let slot_offset = builder
            .ins()
            .imul_imm(handle_ext, i64::from(FuncDescOffsets::SIZE));
        let slot_ptr = builder.ins().iadd(funcs_base, slot_offset);
        let func_ptr = builder.ins().load(
            pointer_type,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::ENTRY,
        );
        let callee_vmctx = builder.ins().load(
            pointer_type,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::VMCTX,
        );

        // Dynamic signature check: `store_funcs[handle].type_id` must equal the
        // canonical id of the expected signature (from `vmctx.type_ids`).
        let actual_type_id = builder.ins().load(
            types::I32,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::TYPE_ID,
        );
        let type_ids_gv = self.vmctx_global(builder.func, VmCtxOffsets::TYPE_IDS, pointer_type);
        let type_ids_base = builder.ins().global_value(pointer_type, type_ids_gv);
        let expected_slot = builder
            .ins()
            .iadd_imm(type_ids_base, i64::from(sig_index.index() as i32) * 4);
        let expected_type_id =
            builder
                .ins()
                .load(types::I32, MemFlags::trusted(), expected_slot, 0);
        let mismatch = builder.ins().icmp(
            ir::condcodes::IntCC::NotEqual,
            actual_type_id,
            expected_type_id,
        );
        builder.ins().trapnz(mismatch, ir::TrapCode::BadSignature);

        let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
        args.push(callee_vmctx);
        args.extend_from_slice(call_args);

        Ok(Some(builder.ins().call_indirect(sig_ref, func_ptr, &args)))
    }

    fn translate_return_call_indirect(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        sig_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: Value,
        call_args: &[Value],
    ) -> WasmResult<()> {
        self.ensure_table(builder.func, table_index);
        let table = self.tables[table_index].as_ref().unwrap();
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, callee, pointer_type, true);
        let handle = builder.ins().load(types::I32, flags, table_entry_addr, 0);

        let is_null = builder
            .ins()
            .icmp_imm(ir::condcodes::IntCC::Equal, handle, 0);
        builder
            .ins()
            .trapnz(is_null, ir::TrapCode::IndirectCallToNull);

        let funcs_gv = self.vmctx_global(builder.func, VmCtxOffsets::STORE_FUNCS, pointer_type);
        let funcs_base = builder.ins().global_value(pointer_type, funcs_gv);
        let handle_ext = builder.ins().uextend(pointer_type, handle);
        let slot_offset = builder
            .ins()
            .imul_imm(handle_ext, i64::from(FuncDescOffsets::SIZE));
        let slot_ptr = builder.ins().iadd(funcs_base, slot_offset);
        let func_ptr = builder.ins().load(
            pointer_type,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::ENTRY,
        );
        let callee_vmctx = builder.ins().load(
            pointer_type,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::VMCTX,
        );

        // Dynamic signature check, identical to `translate_call_indirect`:
        // `store_funcs[handle].type_id` must equal the canonical id of the
        // expected signature. Tail calls are outside the v1 feature gate, so
        // this path is currently unreachable — but it must not become a
        // type-confusion hole the day they are enabled.
        let actual_type_id = builder.ins().load(
            types::I32,
            MemFlags::trusted(),
            slot_ptr,
            FuncDescOffsets::TYPE_ID,
        );
        let type_ids_gv = self.vmctx_global(builder.func, VmCtxOffsets::TYPE_IDS, pointer_type);
        let type_ids_base = builder.ins().global_value(pointer_type, type_ids_gv);
        let expected_slot = builder
            .ins()
            .iadd_imm(type_ids_base, i64::from(sig_index.index() as i32) * 4);
        let expected_type_id =
            builder
                .ins()
                .load(types::I32, MemFlags::trusted(), expected_slot, 0);
        let mismatch = builder.ins().icmp(
            ir::condcodes::IntCC::NotEqual,
            actual_type_id,
            expected_type_id,
        );
        builder.ins().trapnz(mismatch, ir::TrapCode::BadSignature);

        let mut args: Vec<Value> = Vec::with_capacity(call_args.len() + 1);
        args.push(callee_vmctx);
        args.extend_from_slice(call_args);

        builder.ins().return_call_indirect(sig_ref, func_ptr, &args);
        Ok(())
    }

    fn translate_return_call_ref(
        &mut self,
        _builder: &mut FunctionBuilder,
        _sig_ref: ir::SigRef,
        _callee: Value,
        _call_args: &[Value],
    ) -> WasmResult<()> {
        Err(WasmError::Unsupported("return_call_ref".to_string()))
    }

    fn translate_call_ref(
        &mut self,
        _builder: &mut FunctionBuilder,
        _sig_ref: ir::SigRef,
        _callee: Value,
        _call_args: &[Value],
    ) -> WasmResult<ir::Inst> {
        Err(WasmError::Unsupported("call_ref".to_string()))
    }

    fn translate_memory_grow(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
        val: Value,
    ) -> WasmResult<Value> {
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
        Ok(unpack_plain_result(&mut pos, raw))
    }

    fn translate_memory_size(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
    ) -> WasmResult<Value> {
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

    fn translate_memory_copy(
        &mut self,
        mut pos: FuncCursor,
        src_index: MemoryIndex,
        _src_heap: Heap,
        dst_index: MemoryIndex,
        _dst_heap: Heap,
        dst: Value,
        src: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HeapOutOfBounds);
        Ok(())
    }

    fn translate_memory_fill(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
        dst: Value,
        val: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HeapOutOfBounds);
        Ok(())
    }

    fn translate_memory_init(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
        seg_index: u32,
        dst: Value,
        src: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::HeapOutOfBounds);
        Ok(())
    }

    fn translate_data_drop(&mut self, mut pos: FuncCursor, seg_index: u32) -> WasmResult<()> {
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

    fn translate_table_size(
        &mut self,
        mut pos: FuncCursor,
        index: TableIndex,
    ) -> WasmResult<Value> {
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

    fn translate_table_grow(
        &mut self,
        mut pos: FuncCursor,
        table_index: TableIndex,
        delta: Value,
        init_value: Value,
    ) -> WasmResult<Value> {
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

    fn translate_table_get(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        index: Value,
    ) -> WasmResult<Value> {
        self.ensure_table(builder.func, table_index);
        let table = self.tables[table_index].as_ref().unwrap();
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, index, pointer_type, true);
        Ok(builder.ins().load(types::I32, flags, table_entry_addr, 0))
    }

    fn translate_table_set(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        value: Value,
        index: Value,
    ) -> WasmResult<()> {
        self.ensure_table(builder.func, table_index);
        let table = self.tables[table_index].as_ref().unwrap();
        let pointer_type = self.pointer_type();
        let (table_entry_addr, flags) =
            table.prepare_table_addr(builder, index, pointer_type, true);
        builder.ins().store(flags, value, table_entry_addr, 0);
        Ok(())
    }

    fn translate_table_copy(
        &mut self,
        mut pos: FuncCursor,
        dst_table_index: TableIndex,
        src_table_index: TableIndex,
        dst: Value,
        src: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::TableOutOfBounds);
        Ok(())
    }

    fn translate_table_fill(
        &mut self,
        mut pos: FuncCursor,
        table_index: TableIndex,
        dst: Value,
        val: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::TableOutOfBounds);
        Ok(())
    }

    fn translate_table_init(
        &mut self,
        mut pos: FuncCursor,
        seg_index: u32,
        table_index: TableIndex,
        dst: Value,
        src: Value,
        len: Value,
    ) -> WasmResult<()> {
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
        unpack_libcall_result(&mut pos, raw, ir::TrapCode::TableOutOfBounds);
        Ok(())
    }

    fn translate_elem_drop(&mut self, mut pos: FuncCursor, seg_index: u32) -> WasmResult<()> {
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

    fn translate_ref_null(&mut self, mut pos: FuncCursor, _ty: WasmHeapType) -> WasmResult<Value> {
        Ok(pos.ins().iconst(types::I32, 0))
    }

    fn translate_ref_is_null(&mut self, mut pos: FuncCursor, value: Value) -> WasmResult<Value> {
        let is_null = pos.ins().icmp_imm(ir::condcodes::IntCC::Equal, value, 0);
        Ok(pos.ins().uextend(types::I32, is_null))
    }

    fn translate_ref_func(
        &mut self,
        mut pos: FuncCursor,
        func_index: FuncIndex,
    ) -> WasmResult<Value> {
        // A funcref is a store-native handle, looked up from the per-module
        // `refs` array (one u32 per function index).
        let vmctx = pos.func.create_global_value(ir::GlobalValueData::VMContext);
        let refs_gv = pos.func.create_global_value(ir::GlobalValueData::Load {
            base: vmctx,
            offset: Offset32::new(VmCtxOffsets::REFS),
            global_type: self.pointer_type(),
            flags: MemFlags::trusted().with_readonly(),
        });
        let refs_base = pos.ins().global_value(self.pointer_type(), refs_gv);
        let slot = pos
            .ins()
            .iadd_imm(refs_base, i64::from(func_index.index() as i32) * 4);
        Ok(pos.ins().load(types::I32, MemFlags::trusted(), slot, 0))
    }

    fn translate_custom_global_get(
        &mut self,
        _pos: FuncCursor,
        _global_index: GlobalIndex,
    ) -> WasmResult<Value> {
        Err(unsupported_runtime_op("custom global.get"))
    }

    fn translate_custom_global_set(
        &mut self,
        _pos: FuncCursor,
        _global_index: GlobalIndex,
        _val: Value,
    ) -> WasmResult<()> {
        Err(unsupported_runtime_op("custom global.set"))
    }

    fn translate_atomic_wait(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
        addr: Value,
        expected: Value,
        timeout: Value,
    ) -> WasmResult<Value> {
        let pointer_type = self.pointer_type();
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
            ir::TrapCode::HeapOutOfBounds,
        ))
    }

    fn translate_atomic_notify(
        &mut self,
        mut pos: FuncCursor,
        index: MemoryIndex,
        _heap: Heap,
        addr: Value,
        count: Value,
    ) -> WasmResult<Value> {
        let pointer_type = self.pointer_type();
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
            ir::TrapCode::HeapOutOfBounds,
        ))
    }

    fn translate_ref_i31(&mut self, _pos: FuncCursor, _val: Value) -> WasmResult<Value> {
        Err(unsupported_runtime_op("ref.i31"))
    }

    fn before_translate_function(
        &mut self,
        builder: &mut FunctionBuilder,
        _state: &cranelift_wasm::FuncTranslationState,
    ) -> WasmResult<()> {
        // Entry-time stack-overflow check: trap if the stack pointer has
        // dropped below the limit stored in the hidden context. This bounds
        // wasm recursion before the host stack is exhausted, and with a
        // dedicated signal alt-stack the trap is always recoverable.
        let pointer_type = self.pointer_type();
        let vmctx = builder
            .func
            .create_global_value(ir::GlobalValueData::VMContext);
        let limit_gv = builder.func.create_global_value(ir::GlobalValueData::Load {
            base: vmctx,
            offset: Offset32::new(VmCtxOffsets::STACK_LIMIT),
            global_type: pointer_type,
            flags: MemFlags::trusted().with_readonly(),
        });
        let limit = builder.ins().global_value(pointer_type, limit_gv);
        let stack_pointer = builder.ins().get_stack_pointer(pointer_type);
        let overflow =
            builder
                .ins()
                .icmp(ir::condcodes::IntCC::UnsignedLessThan, stack_pointer, limit);
        builder.ins().trapnz(overflow, ir::TrapCode::StackOverflow);
        Ok(())
    }

    fn translate_i31_get_s(&mut self, _pos: FuncCursor, _i31ref: Value) -> WasmResult<Value> {
        Err(unsupported_runtime_op("i31.get_s"))
    }

    fn translate_i31_get_u(&mut self, _pos: FuncCursor, _i31ref: Value) -> WasmResult<Value> {
        Err(unsupported_runtime_op("i31.get_u"))
    }
}

/// Convert a cranelift-wasm translation failure into a compiler error.
pub fn wasm_error_to_compile(err: WasmError) -> CompileError {
    match err {
        WasmError::Unsupported(msg) => CompileError::Unsupported(msg),
        other => CompileError::Translate(other.to_string()),
    }
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
/// check could not tell them apart. `extended_const` remains disabled
/// because the pinned `cranelift-wasm` cannot lower those const-expr
/// operators.
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
    features
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
    let vmctx_gv = pos.func.create_global_value(ir::GlobalValueData::VMContext);
    let libcalls_gv = pos.func.create_global_value(ir::GlobalValueData::Load {
        base: vmctx_gv,
        offset: Offset32::new(VmCtxOffsets::LIBCALLS),
        global_type: pointer_type,
        flags: MemFlags::trusted().with_readonly(),
    });
    let base = pos.ins().global_value(pointer_type, libcalls_gv);
    let slot_ptr = pos.ins().iadd_imm(base, i64::from(slot));
    let fn_ptr = pos
        .ins()
        .load(pointer_type, MemFlags::trusted(), slot_ptr, 0);

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
    let trap_word = pos.ins().ushr_imm(raw, 32);
    let is_trap = pos
        .ins()
        .icmp_imm(ir::condcodes::IntCC::NotEqual, trap_word, 0);
    pos.ins().trapnz(is_trap, trap);
    pos.ins().ireduce(types::I32, raw)
}

/// Unpacks a non-trapping libcall result: the low 32 bits as a signed `i32`.
fn unpack_plain_result(pos: &mut FuncCursor, raw: Value) -> Value {
    pos.ins().ireduce(types::I32, raw)
}

fn unsupported_runtime_op(name: &str) -> WasmError {
    WasmError::Unsupported(format!(
        "{name} lowering is not yet implemented in the AOT compiler"
    ))
}

fn wasm_type_to_clif(ty: &WasmValType) -> ir::Type {
    match ty {
        WasmValType::I32 => types::I32,
        WasmValType::I64 => types::I64,
        WasmValType::F32 => types::F32,
        WasmValType::F64 => types::F64,
        WasmValType::V128 => types::I8X16,
        // Reference values are represented as raw `u32` handles in wasmtiny.
        WasmValType::Ref(_) => types::I32,
    }
}

/// Widens an `i32` operand to the pointer width for a libcall argument.
fn widen_u32(pos: &mut FuncCursor, value: Value, pointer_type: ir::Type) -> Value {
    if pos.func.dfg.value_type(value) == types::I32 {
        pos.ins().uextend(pointer_type, value)
    } else {
        value
    }
}
