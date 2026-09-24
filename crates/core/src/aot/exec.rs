//! Native execution: invoking a finish-linked function through its entry
//! trampoline, dispatching imported host functions, and wiring the shared
//! store-wide function/table state used by `call_indirect`.

use std::cell::Cell;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use super::{
    code::ExecutableCode,
    context::{FuncDesc, MemoryDesc, TableCells, VmCtx},
    loader::{AotFunction, AotModule},
    store::{AotExtern, AotStore, AotTable, SharedAotStore},
    traps,
};
use parking_lot::Mutex as ParkingMutex;

use crate::{
    memory::RegionProt,
    runtime::{
        DataKind, ElemKind, ExportKind, ExportType, FunctionType, Global, GlobalType, HostCaller,
        HostFunc, ImportKind, InstanceMeter, InstanceStats, Memory, MeterCells, NumType, RefType,
        Result, SharedMemoryRegistry, SharedRegionId, Store, TrapCode, ValType, WasmError,
        WasmValue, evaluate_const_expr,
    },
};

/// The fixed shape of an array-call entry trampoline:
/// `(vmctx, callee, args: *const u64, results: *mut u64) -> ()`.
type ArrayCall = unsafe extern "C" fn(*const VmCtx, *const u8, *const u64, *mut u64);
type SharedTableTag = Arc<Mutex<AotTable>>;

/// Byte size of one wasm global cell in the vmctx globals array.
const GLOBAL_CELL_SIZE: usize = 8;
/// Packed trap sentinel shifted into the high word of a libcall result.
const LIBCALL_TRAP: u64 = 1 << 32;
const MAX_STACK_SIZE: usize = 16 * 1024 * 1024;
/// Budget of host stack (in bytes) granted to wasm recursion before the
/// entry-time stack check traps.
const MAX_WASM_STACK: usize = 256 * 1024;
/// Floor and ceiling for a per-invocation shadow-stack slot (see
/// `AotInstance::invoke_shared`).
const MIN_STACK_SIZE: usize = 64 * 1024;
/// Guard between the initial shadow-stack pointer and the top of its slot, so
/// an access at the entry stack pointer stays inside the memory's addressable
/// bound.
const STACK_TOP_GUARD: u32 = 4096;

/// Per-instance options affecting shadow-stack support.
#[derive(Clone, Copy, Debug, Default)]
pub struct InstanceOptions {
    /// Overrides the per-invocation shadow-stack slot size in bytes
    /// (clamped to [`MIN_STACK_SIZE`]..[`MAX_STACK_SIZE`]). When unset, the
    /// slot size is derived from the module's `__stack_pointer` initial value
    /// minus the exported `__heap_base` (the stack region the linker
    /// reserved), falling back to the bare initial value.
    pub stack_size: Option<usize>,
}

/// A loaded, instantiated artifact ready for native invocation.
pub struct AotInstance {
    image: Arc<ExecutableCode>,
    functions: Vec<AotFunction>,
    types: Vec<FunctionType>,
    /// The module's export directory, for name-based lookup.
    exports: Vec<ExportType>,
    ctx: Box<VmCtx>,
    _funcs: Vec<FuncDesc>,
    _type_ids: Vec<u32>,
    _refs: Vec<u32>,
    _globals: Vec<u8>,
    _global_types: Vec<GlobalType>,
    _tables: Vec<SharedTableTag>,
    /// Per-instance table slots (pointers into the shared holders owned by
    /// the tables in `_tables`); must outlive every native call.
    _table_slots: Vec<*const TableCells>,
    _memories: Vec<Arc<Mutex<Memory>>>,
    _memory_descs: Vec<MemoryDesc>,
    /// Index of the module's shadow-stack global (`__stack_pointer`), resolved
    /// at instantiation from the artifact record (falling back to the export
    /// directory). `None` when the module declares no stack-pointer global,
    /// in which case `invoke_shared` keeps the shared-globals behaviour (and
    /// the module must not be entered concurrently if it uses a shadow stack).
    stack_pointer_global: Option<usize>,
    /// Per-invocation shadow-stack slot size in bytes (usable bytes, excluding
    /// the guard page), derived from the module's `__stack_pointer`/`__heap_base`
    /// values (0 when there is none).
    stack_size: usize,
    /// Recycled shadow-stack slot base addresses (byte offsets into memory 0).
    /// Guarded so concurrent `invoke_shared` calls never share a slot.
    stack_slots: Mutex<Vec<u32>>,
    _libcalls: Box<LibcallTable>,
    _dispatch: Box<AotDispatchState>,
    _store: SharedAotStore,
    /// The instance's shared-memory registry, shared with the store so host
    /// and guest wait/notify interoperate (see
    /// [`AotInstance::allocate_shared_region`]).
    _shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
    /// Shared regions currently attached to this instance's memory, detached
    /// on drop to keep the registry's attachment counts accurate.
    _attached_regions: Mutex<Vec<SharedRegionId>>,
    _registered: traps::RegisteredCode,
}

/// The runtime libcall table, reached by compiled code through `vmctx.libcalls`.
#[repr(C)]
struct LibcallTable {
    host_call: unsafe extern "C" fn(*const u8, u64, *mut u64, *mut u64) -> u64,
    atomic_notify: unsafe extern "C" fn(*const u8, u64, u64, u64) -> u64,
    atomic_wait32: unsafe extern "C" fn(*const u8, u64, u64, u64, u64) -> u64,
    atomic_wait64: unsafe extern "C" fn(*const u8, u64, u64, u64, u64) -> u64,
    memory_size: unsafe extern "C" fn(*const u8, u64) -> u64,
    memory_grow: unsafe extern "C" fn(*const u8, u64, u64) -> u64,
    memory_copy: unsafe extern "C" fn(*const u8, u64, u64, u64, u64, u64) -> u64,
    memory_fill: unsafe extern "C" fn(*const u8, u64, u64, u64, u64) -> u64,
    memory_init: unsafe extern "C" fn(*const u8, u64, u64, u64, u64, u64) -> u64,
    data_drop: unsafe extern "C" fn(*const u8, u64) -> u64,
    table_size: unsafe extern "C" fn(*const u8, u64) -> u64,
    table_grow: unsafe extern "C" fn(*const u8, u64, u64, u64) -> u64,
    table_copy: unsafe extern "C" fn(*const u8, u64, u64, u64, u64, u64) -> u64,
    table_fill: unsafe extern "C" fn(*const u8, u64, u64, u64, u64) -> u64,
    table_init: unsafe extern "C" fn(*const u8, u64, u64, u64, u64, u64) -> u64,
    elem_drop: unsafe extern "C" fn(*const u8, u64) -> u64,
}

/// Per-instance state the `host_call` libcall dispatches through.
struct AotDispatchState {
    host_funcs: Vec<Arc<dyn HostFunc>>,
    host_types: Vec<FunctionType>,
    store: Arc<Mutex<Store>>,
    memories: Vec<Arc<Mutex<crate::runtime::Memory>>>,
    tables: Vec<Arc<Mutex<AotTable>>>,
    /// Passive data-segment bytes, indexed by unified data-segment index.
    data_segments: Vec<Vec<u8>>,
    /// Whether each data segment is still available to `memory.init`. Stored
    /// as atomics so `data.drop` and `memory.init` from concurrent
    /// invocations never alias the dispatch state mutably.
    data_available: Vec<AtomicBool>,
    /// Element-segment store handles (resolved from `refs`), indexed by
    /// unified element-segment index.
    elem_segments: Vec<Vec<u32>>,
    /// Whether each element segment is still available to `table.init`. See
    /// `data_available` for the concurrency contract.
    elem_available: Vec<AtomicBool>,
    /// Per-instance meter. The memory-page budget is enforced inside the
    /// `memory.grow` critical section so concurrent grows cannot exceed it.
    ///
    /// The execution budget is enforced by the inline fuel charge emitted at
    /// function entry and each loop back-edge. That charge writes to an
    /// *invocation-local* cell — a per-invocation copy of `vmctx.meter`, so
    /// concurrent invocations never ping-pong one shared cache line — whose
    /// budget field is seeded from this meter's remaining allowance. The
    /// runtime drains the local cell back into this meter at flush points
    /// (host-call boundaries and invocation end); see
    /// `AotInstance::invoke_with_local_meter`.
    ///
    /// The meter itself is the instance-wide authority, shared by both
    /// execution paths.
    meter: Arc<InstanceMeter>,
}

/// The invocation-local fuel cell of the current thread, when that thread is
/// inside an AOT invocation (see `AotInstance::invoke_with_local_meter`).
///
/// The host-call libcall reads this to flush the invocation's accumulated fuel
/// into the authoritative meter at a host boundary. It is a thread-local
/// rather than a vmctx field because imported functions — including the
/// compiled host-call stubs — receive the *shared* instance context, which is
/// deliberately identical for every invocation; only the invocation's own
/// direct callees see the per-invocation context copy.
#[derive(Clone, Copy)]
struct InvocationMeter {
    /// The current invocation's local fuel cell.
    local: *const MeterCells,
    /// The authoritative meter of the instance that owns `local`. Compared
    /// against the meter a host call dispatches through, so a native call into
    /// *another* instance never drains this invocation's fuel into that
    /// instance's meter.
    owner: *const InstanceMeter,
}

thread_local! {
    /// The innermost AOT invocation on this thread, restored on return so
    /// host-initiated re-entry into the same instance nests correctly.
    static INVOCATION_METER: Cell<Option<InvocationMeter>> = const { Cell::new(None) };
}

/// Drains this thread's invocation-local fuel cell into `meter` when `meter`
/// is the authoritative meter of the invocation the thread is running.
///
/// Called at the host-call boundary so a host function observes a count that
/// includes the guest work done since the last flush, and so a budget raised
/// (or reset) between invocations takes effect for the remainder of a running
/// invocation. A no-op when the thread is not inside an AOT invocation, or
/// when the host call belongs to a different instance.
fn drain_invocation_meter(meter: &InstanceMeter) {
    let Some(scope) = INVOCATION_METER.with(Cell::get) else {
        return;
    };
    if !std::ptr::eq(scope.owner, meter) || scope.local.is_null() {
        return;
    }
    // SAFETY: `scope.local` points at the local fuel cell of the invocation
    // this thread is currently running; the scope is cleared when that
    // invocation returns, so the cell outlives this call.
    meter.drain_invocation_cells(unsafe { &*scope.local });
}

impl AotInstance {
    /// Prepares a loaded artifact for execution with no imports.
    pub fn new(module: &AotModule) -> Result<Self> {
        let shared_store = AotStore::shared();
        Self::instantiate(&shared_store, module, &[])
    }

    /// Resolves imports and prepares a loaded artifact for native execution.
    pub fn instantiate(
        shared_store: &SharedAotStore,
        module: &AotModule,
        imports: &[(String, String, AotExtern)],
    ) -> Result<Self> {
        Self::instantiate_with_registry(
            shared_store,
            module,
            imports,
            Arc::new(ParkingMutex::new(SharedMemoryRegistry::default())),
        )
    }

    /// Like [`instantiate`](Self::instantiate), but the instance uses the
    /// provided shared-memory registry instead of a fresh one.
    ///
    /// Two instances created with the same registry Arc see the same shared
    /// regions: a region allocated by one can be attached by the other, and
    /// host waiters on the registry interoperate with both. This mirrors
    /// `Store::with_shared_registry` on the interpreter side; without it,
    /// each AOT instance's regions would be invisible to every other.
    pub fn instantiate_with_registry(
        shared_store: &SharedAotStore,
        module: &AotModule,
        imports: &[(String, String, AotExtern)],
        registry: Arc<ParkingMutex<SharedMemoryRegistry>>,
    ) -> Result<Self> {
        Self::instantiate_with_registry_and_options(
            shared_store,
            module,
            imports,
            registry,
            InstanceOptions::default(),
        )
    }

    /// Like [`instantiate_with_registry`](Self::instantiate_with_registry),
    /// with per-instance [`InstanceOptions`] (shadow-stack slot sizing).
    pub fn instantiate_with_registry_and_options(
        shared_store: &SharedAotStore,
        module: &AotModule,
        imports: &[(String, String, AotExtern)],
        registry: Arc<ParkingMutex<SharedMemoryRegistry>>,
        options: InstanceOptions,
    ) -> Result<Self> {
        traps::ensure_installed().map_err(|error| {
            WasmError::Runtime(format!("signal machinery unavailable: {error}"))
        })?;

        let mut store = shared_store.lock().map_err(|_| poisoned_case())?;

        let image = Arc::new(ExecutableCode::from_bytes(&module.code_image)?);

        // Register the code image for trap classification: wasm-function trap
        // sites plus trampoline/stub trap sites.
        let mut trap_offsets: Vec<(u32, TrapCode)> = module
            .functions
            .iter()
            .flat_map(|function| {
                function
                    .traps
                    .iter()
                    .map(move |&(offset, code)| (function.code_offset + offset, code))
            })
            .collect();
        trap_offsets.extend(module.extra_traps.iter().copied());
        trap_offsets.sort_unstable_by_key(|(offset, _)| *offset);
        let registered = traps::RegisteredCode::new(
            image.entry(),
            module.code_image.len(),
            Arc::new(trap_offsets),
        );

        let mut ctx = Box::new(VmCtx::empty());
        // The shared context's stack limit starts disabled (0 = no entry
        // check). The single-threaded `invoke` refreshes it to the invoking
        // thread's bound before every call, but `invoke_shared` must never
        // write it: indirect and imported callees read the limit from this
        // shared context (via their `FuncDesc`), and one thread's bound is
        // not a valid bound for another thread — it would false-trap
        // `StackOverflow` when thread stacks overlap. Concurrent invocations
        // instead carry the thread-relative bound in a per-invocation
        // context copy (see `invoke_shared`); indirect-callee stack overflow
        // on the concurrent path is recovered by the guard page + signal
        // alt-stack rather than the entry check, and the signal handler
        // classifies the exhausted-stack fault as `StackOverflow`.
        ctx.stack_limit = 0;
        let ctx_ptr = ctx.as_ref() as *const VmCtx as *const u8;

        let mut host_funcs: Vec<Arc<dyn HostFunc>> = Vec::new();
        let mut host_types: Vec<FunctionType> = Vec::new();
        let mut funcs: Vec<FuncDesc> = Vec::new();
        let mut refs: Vec<u32> = Vec::with_capacity(
            module
                .imports
                .iter()
                .filter(|i| matches!(i.kind, ImportKind::Func(_)))
                .count()
                + module.functions.len(),
        );

        // Module-local function descriptors: imports first, then defined.
        for import in module.imports.iter() {
            let ImportKind::Func(type_idx) = &import.kind else {
                continue;
            };
            let expected = module
                .types
                .get(*type_idx as usize)
                .ok_or_else(|| WasmError::Instantiate(format!("type {type_idx} not found")))?;
            let provided = imports
                .iter()
                .find(|(m, n, _)| m == &import.module && n == &import.name)
                .ok_or_else(|| {
                    WasmError::Instantiate(format!(
                        "import {}.{} is not satisfied",
                        import.module, import.name
                    ))
                })?;

            let desc = match &provided.2 {
                AotExtern::HostFunc(func) => {
                    if let Some(actual) = func.function_type()
                        && actual != expected
                    {
                        return Err(WasmError::Instantiate(format!(
                            "import {}.{} function type mismatch",
                            import.module, import.name
                        )));
                    }
                    let ordinal = host_funcs.len();
                    let stub_offset =
                        *module
                            .func_import_stub_offsets
                            .get(ordinal)
                            .ok_or_else(|| {
                                WasmError::Instantiate(format!(
                                    "missing host-call stub for import {}.{}",
                                    import.module, import.name
                                ))
                            })?;
                    // SAFETY: stub offsets are validated against the code image.
                    let entry = unsafe { image.entry().add(stub_offset as usize) };
                    let type_id = store.canonical_type_id(expected);
                    host_funcs.push(func.clone());
                    host_types.push(expected.clone());
                    FuncDesc {
                        entry,
                        vmctx: ctx_ptr,
                        type_id,
                        _pad: 0,
                    }
                }
                AotExtern::Func(handle) => {
                    let desc = store.func_desc(*handle).cloned().ok_or_else(|| {
                        WasmError::Instantiate(format!("unknown function handle {handle}"))
                    })?;
                    if desc.type_id != store.canonical_type_id(expected) {
                        return Err(WasmError::Instantiate(format!(
                            "import {}.{} function type mismatch",
                            import.module, import.name
                        )));
                    }
                    desc
                }
                _ => {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} must be a function",
                        import.module, import.name
                    )));
                }
            };
            funcs.push(desc);
        }

        for function in &module.functions {
            // SAFETY: code offsets are validated against the code image length.
            let entry = unsafe { image.entry().add(function.code_offset as usize) };
            let type_id = store.canonical_type_id(&module.types[function.type_idx as usize]);
            funcs.push(FuncDesc {
                entry,
                vmctx: ctx_ptr,
                type_id,
                _pad: 0,
            });
        }

        // Store-wide handles for every function (this is `vmctx.store_funcs`).
        for desc in &funcs {
            refs.push(store.push_func(*desc));
        }

        // Canonical signature ids, one per module type index.
        let type_ids: Vec<u32> = module
            .types
            .iter()
            .map(|ty| store.canonical_type_id(ty))
            .collect();

        // Globals: materialise 8-byte cells (imports first, then defined).
        // Compiled `global.get`/`global.set` read/write these cells through
        // `vmctx.globals`. Definition-time initialisers are evaluated in order
        // so later initialisers may reference earlier immutable globals. This
        // must run before element/data replay because active-segment offsets
        // may reference imported globals.
        let mut global_cells: Vec<u8> = Vec::new();
        let mut global_types: Vec<GlobalType> = Vec::new();
        let mut const_globals: Vec<Option<Arc<Mutex<Global>>>> = Vec::new();
        for import in module.imports.iter() {
            let ImportKind::Global(gty) = &import.kind else {
                continue;
            };
            let provided = imports
                .iter()
                .find(|(m, n, _)| m == &import.module && n == &import.name)
                .ok_or_else(|| {
                    WasmError::Instantiate(format!(
                        "import {}.{} is not satisfied",
                        import.module, import.name
                    ))
                })?;
            let AotExtern::Global(global) = &provided.2 else {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} must be a global",
                    import.module, import.name
                )));
            };
            if global.type_ != *gty {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} global type mismatch",
                    import.module, import.name
                )));
            }
            global_cells.extend_from_slice(&value_to_slot(&global.value).to_le_bytes());
            global_types.push(gty.clone());
            const_globals.push(match gty.mutable {
                false => Some(Arc::new(Mutex::new(global.clone()))),
                true => None,
            });
        }
        for (gty, init) in &module.globals {
            let value = evaluate_const_expr(init, &const_globals, &refs)?;
            global_cells.extend_from_slice(&value_to_slot(&value).to_le_bytes());
            global_types.push(gty.clone());
            const_globals.push(match gty.mutable {
                false => Some(Arc::new(Mutex::new(Global::new(gty.clone(), value)?))),
                true => None,
            });
        }

        // Tables: imported tables resolve to a shared cell buffer; defined
        // tables allocate fresh cells and replay active element segments.
        let mut tables: Vec<Arc<Mutex<AotTable>>> = Vec::new();
        for import in module.imports.iter() {
            let ImportKind::Table(expected) = &import.kind else {
                continue;
            };
            let provided = imports
                .iter()
                .find(|(m, n, _)| m == &import.module && n == &import.name)
                .ok_or_else(|| {
                    WasmError::Instantiate(format!(
                        "import {}.{} is not satisfied",
                        import.module, import.name
                    ))
                })?;
            let AotExtern::Table(table) = &provided.2 else {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} must be a table",
                    import.module, import.name
                )));
            };
            {
                let table = table.lock().map_err(|_| poisoned_table())?;
                if !table_matches_required(&table, expected) {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} table type mismatch",
                        import.module, import.name
                    )));
                }
            }
            tables.push(table.clone());
        }

        for table_type in &module.tables {
            let table = Arc::new(Mutex::new(AotTable::with_initial(
                table_type.clone(),
                table_type.limits.min(),
            )?));
            tables.push(table);
        }

        // Replay active element segments into the tables.
        for (segment, func_indices) in module.elems.iter().zip(module.elem_funcs.iter()) {
            let ElemKind::Active { table_idx, offset } = &segment.kind else {
                continue;
            };
            let offset = eval_offset(offset, &const_globals, &refs)?;
            let table = tables
                .get(*table_idx as usize)
                .ok_or_else(|| WasmError::Instantiate(format!("table {table_idx} not found")))?;
            let mut table = table.lock().map_err(|_| poisoned_lock())?;
            let end = (offset as usize)
                .checked_add(func_indices.len())
                .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
            if end > table.cells.len() {
                return Err(WasmError::Trap(TrapCode::TableOutOfBounds));
            }
            for (index, func_index) in func_indices.iter().enumerate() {
                table.cells[offset as usize + index] = if *func_index == u32::MAX {
                    0 // null
                } else {
                    refs[*func_index as usize]
                };
            }
        }

        // Per-instance table slots: `vmctx.tables[i]` points at the *shared*
        // cells holder of table `i`, so growth performed by any instance
        // that can reach the table (including this one, or another importing
        // the same shared table) is observed by compiled code here. The
        // holders live inside the `AotTable`s kept alive by `_tables`.
        let table_slots: Vec<*const TableCells> = tables
            .iter()
            .map(|table| {
                let table = table.lock().expect("table lock");
                debug_assert_eq!(
                    table.holder.base,
                    table.cells.as_ptr() as *mut u8,
                    "table cell storage moved after creation"
                );
                &table.holder as *const TableCells
            })
            .collect();

        // Defined memories: mmap-backed, described for compiled heap accesses.
        // Imported memories resolve first (their index space precedes defined).
        let mut memories: Vec<Arc<Mutex<Memory>>> = Vec::new();
        for import in module.imports.iter() {
            let ImportKind::Memory(expected) = &import.kind else {
                continue;
            };
            let provided = imports
                .iter()
                .find(|(m, n, _)| m == &import.module && n == &import.name)
                .ok_or_else(|| {
                    WasmError::Instantiate(format!(
                        "import {}.{} is not satisfied",
                        import.module, import.name
                    ))
                })?;
            let AotExtern::Memory(memory) = &provided.2 else {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} must be a memory",
                    import.module, import.name
                )));
            };
            {
                let memory = memory.lock().map_err(|_| poisoned_lock())?;
                if !memory_matches_required(&memory, expected) {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} memory type mismatch",
                        import.module, import.name
                    )));
                }
            }
            memories.push(memory.clone());
        }
        for memory_type in &module.memories {
            let memory = Memory::try_new(memory_type.clone())?;
            memories.push(Arc::new(Mutex::new(memory)));
        }
        let memory_descs: Vec<MemoryDesc> = memories
            .iter()
            .map(|memory| {
                let memory = memory.lock().expect("memory lock");
                MemoryDesc {
                    // The runtime treats the mmap region as mutably shared by
                    // the compiled code (mirrors `LoadedModule::memory_context`).
                    base: memory.as_ptr() as *mut u8,
                    len: memory.len_bytes(),
                    capacity: memory.capacity_bytes(),
                }
            })
            .collect();

        // Replay active data segments into the memories.
        for segment in &module.data {
            let DataKind::Active { memory_idx, offset } = &segment.kind else {
                continue;
            };
            let offset = eval_offset(offset, &const_globals, &refs)?;
            let memory = memories
                .get(*memory_idx as usize)
                .ok_or_else(|| WasmError::Instantiate(format!("memory {memory_idx} not found")))?;
            memory
                .lock()
                .map_err(|_| poisoned_lock())?
                .write(offset, &segment.init)?;
        }

        // Materialise passive segment state for the bulk-memory runtime ops.
        // Active segments are already replayed above and are marked dropped;
        // passive segments stay available until `data.drop`/`elem.drop`.
        let data_segments: Vec<Vec<u8>> = module
            .data
            .iter()
            .map(|segment| segment.init.clone())
            .collect();
        let data_available: Vec<AtomicBool> = module
            .data
            .iter()
            .map(|segment| AtomicBool::new(!matches!(segment.kind, DataKind::Active { .. })))
            .collect();
        let elem_segments: Vec<Vec<u32>> = module
            .elem_funcs
            .iter()
            .map(|func_indices| {
                func_indices
                    .iter()
                    .map(|func_index| {
                        if *func_index == u32::MAX {
                            0 // null entry
                        } else {
                            refs[*func_index as usize]
                        }
                    })
                    .collect()
            })
            .collect();
        let elem_available: Vec<AtomicBool> = module
            .elems
            .iter()
            .map(|segment| AtomicBool::new(matches!(segment.kind, ElemKind::Passive)))
            .collect();

        let rt_store = Arc::new(Mutex::new(Store::with_shared_registry(registry.clone())));
        let shared_memory = {
            let store = rt_store.lock().map_err(|_| poisoned_case())?;
            store.shared_memory_registry()
        };
        let libcalls = LibcallTable::new();
        let dispatch = Box::new(AotDispatchState {
            host_funcs,
            host_types,
            store: rt_store,
            memories: memories.clone(),
            tables: tables.clone(),
            data_segments,
            data_available,
            elem_segments,
            elem_available,
            meter: Arc::new(InstanceMeter::new()),
        });

        let dangling: *const u8 = std::ptr::NonNull::<u8>::dangling().as_ptr();
        ctx.funcs = if funcs.is_empty() {
            dangling.cast()
        } else {
            funcs.as_ptr()
        };
        ctx.store_funcs = store.funcs_ptr();
        ctx.type_ids = if type_ids.is_empty() {
            dangling.cast()
        } else {
            type_ids.as_ptr()
        };
        ctx.refs = if refs.is_empty() {
            dangling.cast()
        } else {
            refs.as_ptr()
        };
        ctx.globals = if global_cells.is_empty() {
            dangling.cast_mut()
        } else {
            global_cells.as_mut_ptr()
        };
        ctx.memories = if memory_descs.is_empty() {
            dangling.cast()
        } else {
            memory_descs.as_ptr()
        };
        ctx.tables = if table_slots.is_empty() {
            dangling.cast()
        } else {
            table_slots.as_ptr()
        };
        ctx.libcalls = &*libcalls as *const LibcallTable as *const u8;
        ctx.dispatch = (&*dispatch as *const AotDispatchState) as *mut core::ffi::c_void;
        // Point compiled code at the instance's lock-free meter cells. The
        // `Arc<InstanceMeter>` lives inside `dispatch` (moved into `_dispatch`
        // below) for the instance's lifetime, so the raw pointer stays valid.
        ctx.meter = dispatch.meter.cells();

        let mut instance = Self {
            image,
            functions: module.functions.clone(),
            types: module.types.clone(),
            exports: module.exports.clone(),
            ctx,
            _funcs: funcs,
            _type_ids: type_ids,
            _refs: refs,
            _globals: global_cells,
            _global_types: global_types,
            _tables: tables,
            _table_slots: table_slots,
            _memories: memories,
            _memory_descs: memory_descs,
            _libcalls: libcalls,
            _dispatch: dispatch,
            _store: shared_store.clone(),
            _shared_memory: shared_memory,
            _attached_regions: Mutex::new(Vec::new()),
            _registered: registered,
            stack_pointer_global: None,
            stack_size: 0,
            stack_slots: Mutex::new(Vec::new()),
        };

        // Resolve the module's shadow-stack global. The artifact records the
        // index the compiler routed through the vmctx `stack_pointer` field
        // (config override or the `__stack_pointer` export); export-based
        // detection is kept as a fallback for artifacts without the record.
        // Its initial value is the stack top; the exported `__heap_base`
        // (stack bottom) refines the reserved stack region when present. The
        // shared context's stack pointer keeps the module's initial value for
        // the single-threaded `invoke` path.
        let stack_pointer_global = module.stack_pointer_global.or_else(|| {
            instance
                .exports
                .iter()
                .find_map(|export| match &export.kind {
                    ExportKind::Global(index) if export.name == "__stack_pointer" => Some(*index),
                    _ => None,
                })
        });
        instance.stack_pointer_global = stack_pointer_global.map(|index| index as usize);
        if let Some(index) = stack_pointer_global {
            let index = index as usize;
            let cell = index * GLOBAL_CELL_SIZE;
            let init = instance
                ._globals
                .get(cell..cell + 4)
                .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .unwrap_or(0);
            instance.ctx.stack_pointer = init;
            // The linker places the stack between `__heap_base` (bottom) and
            // the initial `__stack_pointer` (top); the reserved region is the
            // difference. Without `__heap_base` (not all toolchains export
            // it), the top address alone is the fallback — for the rustc/LLD
            // layout the stack sits at the top of linear memory, so the top
            // over-estimates rather than under-estimates the region.
            let heap_base = instance
                .exports
                .iter()
                .find_map(|export| match &export.kind {
                    ExportKind::Global(index) if export.name == "__heap_base" => {
                        let cell = *index as usize * GLOBAL_CELL_SIZE;
                        instance._globals.get(cell..cell + 4).map(|bytes| {
                            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                        })
                    }
                    _ => None,
                });
            let reserved = heap_base
                .and_then(|heap| init.checked_sub(heap))
                .filter(|size| *size > 0)
                .unwrap_or(init);
            // Clamp the slot size to a sane band: a module with no reserved
            // stack (init 0) still gets a usable stack; a pathologically large
            // init cannot exhaust the reservation.
            instance.stack_size = options
                .stack_size
                .unwrap_or(reserved as usize)
                .clamp(MIN_STACK_SIZE, MAX_STACK_SIZE);
        }

        // Run the start function, if any, after the instance is fully wired.
        if let Some(start) = module.start {
            instance.invoke_start(start)?;
            // The start function runs through `invoke`, which sets the
            // shared context's stack limit for the instantiating thread;
            // re-disable it so concurrent invocations on other threads never
            // check against it (see the `stack_limit` comment above).
            instance.ctx.stack_limit = 0;
        }

        Ok(instance)
    }

    /// The store-native handle for a function, usable to import it into
    /// another module.
    pub fn func_handle(&self, func_index: u32) -> Option<u32> {
        self._refs.get(func_index as usize).copied()
    }

    /// A shared table reference, usable to import it into another module.
    pub fn table_handle(&self, table_index: u32) -> Option<Arc<Mutex<AotTable>>> {
        self._tables.get(table_index as usize).cloned()
    }

    /// A shared memory reference, usable to import it into another module.
    pub fn memory_handle(&self, memory_index: u32) -> Option<Arc<Mutex<Memory>>> {
        self._memories.get(memory_index as usize).cloned()
    }

    /// Returns the instance's shared-memory registry.
    ///
    /// The registry is shared with the instance's store, so every instance
    /// created from the same store sees the same regions; `notify_region`
    /// and `register_region_waiter` on the registry interoperate with guest
    /// `memory.atomic.wait/notify` on attached ranges.
    pub fn shared_memory_registry(&self) -> Arc<ParkingMutex<SharedMemoryRegistry>> {
        self._shared_memory.clone()
    }

    /// Allocates a new shared region and maps it into this instance's first
    /// memory. Returns `(region_id, page_offset)`.
    ///
    /// Thread-safe (`&self`): the memory and registry locks are taken in the
    /// same order as the interpreter's `Instance` (memory first, registry
    /// second), so mixed interpreter/AOT embedding cannot deadlock.
    pub fn allocate_shared_region(
        &self,
        size: u32,
        prot: RegionProt,
    ) -> Result<(SharedRegionId, u32)> {
        let memory = self._memories.first().cloned().ok_or_else(|| {
            WasmError::Runtime("no memory to attach shared region to".to_string())
        })?;
        let mut mem = memory.lock().map_err(|_| poisoned_lock())?;
        let result = self
            ._shared_memory
            .lock()
            .allocate_region(&mut mem, size, prot)?;
        self._attached_regions
            .lock()
            .map_err(|_| poisoned_lock())?
            .push(result.0);
        Ok(result)
    }

    /// Allocates a shared region without mapping it into any guest memory.
    pub fn allocate_shared_region_standalone(&self, size: u32) -> Result<SharedRegionId> {
        self._shared_memory.lock().allocate_region_standalone(size)
    }

    /// Destroys a shared region; it must have no attached mappings.
    pub fn destroy_shared_region(&self, region_id: SharedRegionId) -> Result<()> {
        self._shared_memory.lock().destroy_region(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn shared_region_len(&self, region_id: SharedRegionId) -> Result<u32> {
        self._shared_memory.lock().region_len(region_id)
    }

    /// Attaches an existing shared region to this instance's first memory.
    ///
    /// The region's physical pages are mapped into the guest's address space
    /// (`mmap(MAP_FIXED | MAP_SHARED)`), so writes are immediately visible to
    /// every other attached instance. Returns the page offset where the
    /// region was mapped.
    pub fn attach_shared_region(
        &self,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
    ) -> Result<u32> {
        let memory = self._memories.first().cloned().ok_or_else(|| {
            WasmError::Runtime("no memory to attach shared region to".to_string())
        })?;
        let mut mem = memory.lock().map_err(|_| poisoned_lock())?;
        let page_offset =
            self._shared_memory
                .lock()
                .attach_region(&mut mem, region_id, prot, reader_slot)?;
        self._attached_regions
            .lock()
            .map_err(|_| poisoned_lock())?
            .push(region_id);
        Ok(page_offset)
    }

    /// Detaches a shared region from this instance's first memory.
    pub fn detach_shared_region(&self, region_id: SharedRegionId) -> Result<()> {
        let memory = self._memories.first().cloned().ok_or_else(|| {
            WasmError::Runtime("no memory to detach shared region from".to_string())
        })?;
        let mut mem = memory.lock().map_err(|_| poisoned_lock())?;
        self._shared_memory
            .lock()
            .detach_region(&mut mem, region_id)?;
        self._attached_regions
            .lock()
            .map_err(|_| poisoned_lock())?
            .retain(|id| *id != region_id);
        Ok(())
    }

    /// Writes data to a shared region from the host side.
    pub fn write_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        self._shared_memory
            .lock()
            .write_to_region(region_id, offset, data)
    }

    /// Reads data from a shared region from the host side.
    pub fn read_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        self._shared_memory
            .lock()
            .read_from_region(region_id, offset, buf)
    }

    /// Reads the current value of global `index` (imports first, then defined).
    pub fn global_value(&self, index: u32) -> Option<WasmValue> {
        let ty = self._global_types.get(index as usize)?;
        let cell = self
            ._globals
            .get(index as usize * 8..index as usize * 8 + 8)?;
        let slot = u64::from_le_bytes(cell.try_into().ok()?);
        Some(slot_to_value(slot, &ty.content_type))
    }

    /// Runs the module's start function inside a trap-recovery boundary.
    fn invoke_start(&mut self, start: u32) -> Result<()> {
        let imported_func_count = (self._funcs.len() - self.functions.len()) as u32;
        if start < imported_func_count {
            // An imported start function: call its host-call stub directly. A
            // `() -> ()` signature means the stub's ABI is `(vmctx) -> ()`.
            let desc = self._funcs[start as usize];
            // SAFETY: `desc.entry` is a validated, signature-compatible
            // machine-code entry for a zero-arg host stub.
            let stub: unsafe extern "C" fn(*const VmCtx) =
                unsafe { std::mem::transmute(desc.entry) };
            traps::catch_traps(|| unsafe { stub(self.ctx.as_ref()) })
        } else {
            self.invoke(start, &[]).map(|_| ())
        }
    }

    /// The function index of the named function export, if present.
    pub fn export_func_index(&self, name: &str) -> Option<u32> {
        self.exports
            .iter()
            .find(|export| export.name == name)
            .and_then(|export| match export.kind {
                ExportKind::Func(index) => Some(index),
                _ => None,
            })
    }

    /// Invokes a function export by name.
    ///
    /// Single-threaded entry: refreshes the shared context's thread-relative
    /// stack limit before entering native code, so imported and indirect
    /// callees (which read the limit from the instance context) see this
    /// thread's bound. For concurrent invocations of one instance from many
    /// threads, use [`invoke_export_shared`](Self::invoke_export_shared).
    pub fn invoke_export(&mut self, name: &str, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let index = self
            .export_func_index(name)
            .ok_or_else(|| WasmError::Runtime(format!("function export '{name}' not found")))?;
        self.invoke(index, args)
    }

    /// Invokes a function export by name on a shared instance.
    ///
    /// Concurrent entry: takes `&self` so the instance can be wrapped in an
    /// `Arc` and invoked from many threads at once. Each invocation runs with
    /// its own execution context — a copy of the instance context carrying
    /// this thread's stack limit — so concurrent invocations never write the
    /// shared context and each keeps independent locals and control flow on
    /// its own native stack.
    pub fn invoke_export_shared(&self, name: &str, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let index = self
            .export_func_index(name)
            .ok_or_else(|| WasmError::Runtime(format!("function export '{name}' not found")))?;
        self.invoke_shared(index, args)
    }

    /// Invokes the function `func_idx` natively.
    ///
    /// Single-threaded entry; see [`invoke_export`](Self::invoke_export).
    pub fn invoke(&mut self, func_idx: u32, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        // The shared context's stack limit is refreshed for indirect and
        // imported callees, which read it from the shared context (their
        // `FuncDesc` carries it); the invocation itself runs on a copy that
        // also carries its own fuel cell.
        self.ctx.stack_limit = traps::thread_stack_limit(MAX_WASM_STACK);
        let context = *self.ctx;
        self.invoke_with_local_meter(func_idx, args, &context)
    }

    /// Invokes the function `func_idx` natively on a shared instance.
    ///
    /// Concurrent entry; see [`invoke_export_shared`](Self::invoke_export_shared).
    ///
    /// # Per-invocation context copy
    ///
    /// Each invocation runs on a *copy* of the instance context with this
    /// thread's stack bound and its own fuel cell — never the shared context —
    /// so concurrent invocations from many threads cannot race on it. The copy
    /// is shallow: only `stack_limit`, `meter` and, for a module with a shadow
    /// stack, the `stack_pointer` value differ; the memory, table, function,
    /// dispatch and globals pointers are shared (see the `VmCtx` docs for the
    /// full contract). Because the copy lives on the invoking thread's stack
    /// and the callee receives its address directly, no cross-thread
    /// publication of the copy is ever needed.
    ///
    /// # Shadow stacks
    ///
    /// A rustc-compiled module keeps every function frame on a single
    /// `__stack_pointer` shadow stack in linear memory, so two concurrent
    /// invocations of one instance would overlap their frames if they shared
    /// that pointer. Compiled `global.get`/`global.set` of `__stack_pointer`
    /// read/write the vmctx `stack_pointer` field (the compiler routes it),
    /// so each invocation points it at a private stack slot carved from the
    /// committed memory and no globals copying is involved: ordinary mutable
    /// globals stay genuinely shared through the globals array. Modules
    /// without a stack pointer keep the shared-globals behaviour unchanged.
    pub fn invoke_shared(&self, func_idx: u32, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        // Per-invocation execution context: a copy of the instance context
        // with this thread's stack bound. Direct calls propagate it, so the
        // whole invocation (entry and same-module callees) checks a correct,
        // thread-relative limit while leaving the shared context untouched.
        let mut context = *self.ctx.as_ref();
        context.stack_limit = traps::thread_stack_limit(MAX_WASM_STACK);

        if self.stack_pointer_global.is_none() {
            // No shadow stack: shared globals and stack pointer, as before.
            return self.invoke_with_local_meter(func_idx, args, &context);
        }

        // Reserve a private stack slot and point this invocation's stack
        // pointer at it. The slot is carved from the committed memory (see
        // `acquire_stack_slot`) with a PROT_NONE guard page at its bottom, so
        // a stack overflow traps `MemoryOutOfBounds` instead of silently
        // clobbering the guest heap below; the stack top is kept a small
        // guard below the slot's end so accesses at the entry stack pointer
        // stay inside the memory's addressable bound.
        let slot_base = self.acquire_stack_slot()?;
        let stack_top = slot_base
            .wrapping_add(self.slot_total_bytes())
            .wrapping_sub(STACK_TOP_GUARD);
        context.stack_pointer = stack_top;

        let result = self.invoke_with_local_meter(func_idx, args, &context);

        self.release_stack_slot(slot_base);
        result
    }

    /// Runs one native invocation against its own invocation-local fuel cell.
    ///
    /// `context` already carries the invocation's stack bound (and shadow
    /// stack pointer); this adds the fuel cell, records the invocation on the
    /// calling thread (so the host-call boundary can flush it), and drains the
    /// cell into the authoritative instance meter when the invocation ends —
    /// on the trapping path too, so charges made before a trap still land.
    ///
    /// The local cell is what keeps concurrent invocations of one instance
    /// from ping-ponging a single shared cache line: compiled code's per-loop
    /// atomic add stays on a line only this invocation touches, and the shared
    /// counter is written once per host call and once per invocation. Its
    /// budget field is the *remaining* allowance, so the inline check traps
    /// when this invocation alone exhausts the budget the instance had left
    /// (the interpreter's model: a cached `(count, budget)` snapshot,
    /// refreshed at flush points).
    fn invoke_with_local_meter(
        &self,
        func_idx: u32,
        args: &[WasmValue],
        context: &VmCtx,
    ) -> Result<Vec<WasmValue>> {
        let meter = &self._dispatch.meter;
        let local = meter.invocation_cells();

        let mut context = *context;
        context.meter = &local as *const MeterCells;

        // Publish the cell for the host-call boundary; restore the previous
        // scope on the way out so host-initiated re-entry into this instance
        // (a nested invocation on the same thread) cannot unset an outer
        // invocation's scope.
        let previous = INVOCATION_METER.with(|slot| {
            slot.replace(Some(InvocationMeter {
                local: &local as *const MeterCells,
                owner: Arc::as_ptr(meter),
            }))
        });

        let result = self.invoke_with_context(func_idx, args, &context);

        // Drain while the cell is still live (it lives in this frame), before
        // the scope is restored.
        let _ = meter.drain_invocation_cells(&local);
        INVOCATION_METER.with(|slot| slot.set(previous));

        result
    }

    /// Total byte span of one stack slot: the usable pages plus the bottom
    /// PROT_NONE guard page.
    fn slot_total_bytes(&self) -> u32 {
        let page = crate::memory::PAGE_SIZE_BYTES;
        let usable_pages = self.stack_size.div_ceil(page as usize) as u32;
        usable_pages.wrapping_add(1).wrapping_mul(page)
    }

    /// Acquires a private shadow-stack slot, recycling a freed slot or
    /// growing memory 0 by the slot size and taking the new pages. Returns the
    /// slot's base byte offset.
    ///
    /// The slot is carved from the committed (owned) memory rather than a
    /// top-down reservation because compiled accesses — and the module's own
    /// stack/pointer arithmetic — treat the owned length as the addressable
    /// bound, so the stack pointer must lie within it. Each grow appends, so a
    /// stack slot is always disjoint from the guest allocator's own
    /// `memory.grow` regions. The lowest page of every slot is `mprotect`ed
    /// PROT_NONE: a shadow stack that grows past its slot faults on that page
    /// and traps, instead of silently corrupting the guest heap below.
    ///
    /// Engine-internal growth is not charged against the instance's memory
    /// budget (the budget gates guest `memory.grow`, not engine stacks);
    /// `stats()` still reports the slot pages, since they are committed
    /// memory. Slots are bounded by the peak concurrent invocation count and
    /// are recycled, so the overhead does not accumulate.
    fn acquire_stack_slot(&self) -> Result<u32> {
        if let Some(base) = self.stack_slots.lock().map_err(|_| poisoned_lock())?.pop() {
            return Ok(base);
        }
        let memory = self
            ._memories
            .first()
            .ok_or_else(|| WasmError::Runtime("no memory for stack slot".to_string()))?
            .clone();
        let mut memory = memory.lock().map_err(|_| poisoned_lock())?;
        let page = crate::memory::PAGE_SIZE_BYTES;
        let usable_pages = self.stack_size.div_ceil(page as usize) as u32;
        // + 1 page: the bottom PROT_NONE guard.
        let old_pages = memory.grow(usable_pages.wrapping_add(1))?;
        let base = old_pages.wrapping_mul(page);
        memory.protect_owned(base, page as usize, libc::PROT_NONE)?;
        Ok(base)
    }

    /// Returns a shadow-stack slot to the free list for reuse.
    fn release_stack_slot(&self, base: u32) {
        if let Ok(mut slots) = self.stack_slots.lock() {
            slots.push(base);
        }
    }

    fn invoke_with_context(
        &self,
        func_idx: u32,
        args: &[WasmValue],
        context: &VmCtx,
    ) -> Result<Vec<WasmValue>> {
        let function = self
            .functions
            .iter()
            .find(|f| f.func_index == func_idx)
            .ok_or_else(|| {
                WasmError::Runtime(format!("function {func_idx} not found in artifact"))
            })?;
        let func_type = self
            .types
            .get(function.type_idx as usize)
            .ok_or_else(|| WasmError::Runtime(format!("type {} not found", function.type_idx)))?;

        validate_args(args, func_type)?;

        let arg_slots: Vec<u64> = args.iter().map(value_to_slot).collect();
        let mut result_slots = vec![0u64; func_type.results.len()];

        // SAFETY: offsets were validated against the code image length by the
        // loader; the trampoline and callee were emitted for this signature.
        let trampoline_ptr = unsafe { self.image.entry().add(function.trampoline_offset as usize) };
        let callee = unsafe { self.image.entry().add(function.code_offset as usize) };
        let trampoline: ArrayCall = unsafe { std::mem::transmute(trampoline_ptr) };

        // Native execution is wrapped in a signal-recovery boundary: a trap
        // returns here as `Err(Trap(code))` via the process-wide handler.
        traps::catch_traps(|| unsafe {
            trampoline(
                context,
                callee,
                arg_slots.as_ptr(),
                result_slots.as_mut_ptr(),
            );
        })?;

        decode_results(&result_slots, func_type)
    }

    /// Sets or resets the instance's memory budget (maximum committed page
    /// count); `None` means unbounded.
    ///
    /// Enforced by the native `memory.grow` libcall *inside* the grow
    /// critical section (the memory lock), so concurrent grows cannot both
    /// pass the check and push the instance past its budget.
    pub fn set_memory_budget(&self, budget: Option<u32>) -> Result<()> {
        self._dispatch.meter.set_memory_budget(budget)
    }

    /// Sets or resets the instance's execution budget (maximum metering
    /// units); `None` means unbounded.
    ///
    /// Enforced by the inline fuel charge emitted at function entry and each
    /// loop back-edge. Each invocation charges an invocation-local cell whose
    /// budget field is the allowance left in the instance meter, so a charge
    /// traps [`TrapCode::ExecutionBudgetExceeded`] when the invocation alone
    /// exhausts what the instance had left; the counter may overshoot the
    /// budget by at most one charge (plus, under concurrent invocations, the
    /// units not yet flushed by the other threads — the budget is advisory
    /// across threads, as on the interpreter path).
    ///
    /// The allowance is snapshotted when the invocation starts and refreshed
    /// at each flush point (a host-call boundary and invocation end), so a
    /// reset between invocations takes effect on the next invocation and a
    /// reset made while one runs takes effect at its next host call — the same
    /// granularity as the interpreter's cached `(count, budget)` snapshot.
    pub fn set_execution_budget(&self, budget: Option<u64>) -> Result<()> {
        self._dispatch.meter.set_execution_budget(budget)
    }

    /// Returns a snapshot of the instance's metering data: committed owned
    /// memory pages (shared-region pages excluded) and the executed metering
    /// units charged on the AOT path (size-weighted fuel).
    pub fn stats(&self) -> Result<InstanceStats> {
        let pages = self._memories.iter().try_fold(0u32, |acc, memory| {
            let pages = memory.lock().map_err(|_| poisoned_lock())?.size();
            acc.checked_add(pages)
                .ok_or_else(|| WasmError::Runtime("memory page count overflowed".to_string()))
        })?;
        Ok(self._dispatch.meter.snapshot(pages))
    }

    /// Returns the trap site of the most recent trap on the *calling thread*:
    /// `(function index, byte offset within that function's code, trap code)`.
    ///
    /// Diagnostic aid for root-causing guest traps (the signal handler only
    /// classifies the faulting PC to a [`TrapCode`]; this maps it back to the
    /// wasm function and instruction). `None` when the last invocation on
    /// this thread succeeded, or when the faulting PC is outside this
    /// instance's code image (e.g. a host-side fault).
    pub fn last_trap_site(&self) -> Option<(u32, u32, TrapCode)> {
        let pc = traps::last_trap_pc()?;
        let base = self.image.entry() as usize;
        if pc < base || pc >= base + self.image.len() {
            return None;
        }
        let offset = (pc - base) as u32;
        for function in &self.functions {
            let start = function.code_offset;
            let end = start.saturating_add(function.code_len);
            if offset >= start && offset < end {
                let code = function
                    .traps
                    .iter()
                    .find(|(trap_offset, _)| *trap_offset == offset - start)
                    .map(|(_, code)| *code)
                    .unwrap_or(TrapCode::MemoryOutOfBounds);
                return Some((function.func_index, offset - start, code));
            }
        }
        None
    }
}

// SAFETY: an `AotInstance` shares no mutable state across threads while it is
// being invoked. The compiled image, function/type/global/table/memory
// descriptor arrays and the shared store are immutable after instantiation;
// memory and table mutations are serialised behind their mutexes; the dispatch
// state's mutable segment-availability flags are atomics; and each concurrent
// invocation uses a per-invocation context copy with its own fuel cell, never
// writing the shared context. (`invoke`/`invoke_export` mutate the shared
// context's stack limit but require `&mut self`, so they cannot race with
// shared invocations.)
unsafe impl Send for AotInstance {}

unsafe impl Sync for AotInstance {}

impl Drop for AotInstance {
    fn drop(&mut self) {
        // Detach every shared region this instance attached, so the
        // registry's attachment counts stay accurate and `destroy_region`
        // succeeds after the instance goes away. The memory's own drop
        // unmaps the shared pages regardless; this only fixes the bookkeeping.
        // Lock order: memory first, registry second — the same order as
        // `allocate_shared_region`/`attach_shared_region`/`detach_shared_region`
        // (and the interpreter's `Instance`), so a concurrent attach on
        // another thread can never deadlock against a drop.
        let regions: Vec<SharedRegionId> = {
            let mut attached = match self._attached_regions.lock() {
                Ok(attached) => attached,
                Err(_) => return,
            };
            std::mem::take(&mut *attached)
        };
        if regions.is_empty() {
            return;
        }
        for region_id in regions {
            if let Some(memory) = self._memories.first()
                && let Ok(mut mem) = memory.lock()
            {
                let mut shared_memory = self._shared_memory.lock();
                let _ = shared_memory.detach_region(&mut mem, region_id);
            }
        }
    }
}

impl LibcallTable {
    fn new() -> Box<Self> {
        Box::new(Self {
            host_call,
            atomic_notify,
            atomic_wait32,
            atomic_wait64,
            memory_size,
            memory_grow,
            memory_copy,
            memory_fill,
            memory_init,
            data_drop,
            table_size,
            table_grow,
            table_copy,
            table_fill,
            table_init,
            elem_drop,
        })
    }
}

/// `memory.atomic.notify`: wakes waiters at `addr` in memory `mem_idx`.
unsafe extern "C" fn atomic_notify(ctx: *const u8, mem_idx: u64, addr: u64, count: u64) -> u64 {
    match memory_notify(ctx, mem_idx, addr, count) {
        Ok(woken) => u64::from(woken),
        Err(_) => LIBCALL_TRAP,
    }
}

/// `memory.atomic.wait32`: compares, then parks until notified or timed out.
unsafe extern "C" fn atomic_wait32(
    ctx: *const u8,
    mem_idx: u64,
    addr: u64,
    expected: u64,
    timeout: u64,
) -> u64 {
    match memory_wait(ctx, mem_idx, addr, expected, timeout, false) {
        Ok(status) => status as u32 as u64,
        Err(_) => LIBCALL_TRAP,
    }
}

/// `memory.atomic.wait64`: compares, then parks until notified or timed out.
unsafe extern "C" fn atomic_wait64(
    ctx: *const u8,
    mem_idx: u64,
    addr: u64,
    expected: u64,
    timeout: u64,
) -> u64 {
    match memory_wait(ctx, mem_idx, addr, expected, timeout, true) {
        Ok(status) => status as u32 as u64,
        Err(_) => LIBCALL_TRAP,
    }
}

/// Bounds-checks owned/shared access for a bulk-memory range.
fn check_memory_range(memory: &Memory, addr: u32, len: u32) -> Result<()> {
    let end = addr.checked_add(len).ok_or_else(oob_trap)?;
    if !memory.is_valid_access(addr, len as usize)? {
        return Err(oob_trap());
    }
    // `is_valid_access` only checks readable; bulk writes must also be
    // writable (read-only shared domains trap like the interpreter).
    memory.check_writable(addr, len as usize)?;
    let _ = end;
    Ok(())
}

/// `data.drop`: marks data segment `seg_idx` no longer available.
unsafe extern "C" fn data_drop(ctx: *const u8, seg_idx: u64) -> u64 {
    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch(ctx) }) else {
        return LIBCALL_TRAP;
    };
    match dispatch.data_available.get(seg_idx as usize) {
        Some(flag) => {
            flag.store(false, Ordering::SeqCst);
            0
        }
        None => LIBCALL_TRAP,
    }
}

fn decode_results(slots: &[u64], func_type: &FunctionType) -> Result<Vec<WasmValue>> {
    slots
        .iter()
        .zip(func_type.results.iter())
        .map(|(slot, result)| Ok(slot_to_value(*slot, result)))
        .collect()
}

/// Borrows the per-instance dispatch state out of a vmctx pointer.
///
/// # Safety
/// `ctx` must point at a live [`VmCtx`] whose `dispatch` references a live
/// [`AotDispatchState`] for the duration of the native call.
unsafe fn dispatch<'a>(ctx: *const u8) -> Option<&'a AotDispatchState> {
    let vmctx = unsafe { &*(ctx as *const VmCtx) };
    if vmctx.dispatch.is_null() {
        None
    } else {
        Some(unsafe { &*(vmctx.dispatch as *const AotDispatchState) })
    }
}

/// Resolves an instance memory by index from the dispatch state.
fn dispatch_memory(ctx: *const u8, mem_idx: u64) -> Result<Arc<Mutex<Memory>>> {
    // SAFETY: `ctx` is a live `VmCtx` for the native call's duration.
    let dispatch = unsafe { dispatch(ctx) }
        .ok_or_else(|| WasmError::Runtime("AOT dispatch state missing".to_string()))?;
    dispatch
        .memories
        .get(mem_idx as usize)
        .cloned()
        .ok_or_else(|| WasmError::Runtime(format!("memory {mem_idx} not found")))
}

/// Resolves an instance table by index from the dispatch state.
fn dispatch_table(ctx: *const u8, table_idx: u64) -> Result<Arc<Mutex<AotTable>>> {
    // SAFETY: `ctx` is a live `VmCtx` for the native call's duration.
    let dispatch = unsafe { dispatch(ctx) }
        .ok_or_else(|| WasmError::Runtime("AOT dispatch state missing".to_string()))?;
    dispatch
        .tables
        .get(table_idx as usize)
        .cloned()
        .ok_or_else(|| WasmError::Runtime(format!("table {table_idx} not found")))
}

/// `elem.drop`: marks element segment `seg_idx` no longer available.
unsafe extern "C" fn elem_drop(ctx: *const u8, seg_idx: u64) -> u64 {
    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch(ctx) }) else {
        return LIBCALL_TRAP;
    };
    match dispatch.elem_available.get(seg_idx as usize) {
        Some(flag) => {
            flag.store(false, Ordering::SeqCst);
            0
        }
        None => LIBCALL_TRAP,
    }
}

/// Evaluates an active-segment offset constant expression (`i32.const` or a
/// `global.get` of an imported immutable global) to a byte offset.
fn eval_offset(expr: &[u8], globals: &[Option<Arc<Mutex<Global>>>], refs: &[u32]) -> Result<u32> {
    match evaluate_const_expr(expr, globals, refs)? {
        WasmValue::I32(value) => Ok(value as u32),
        other => Err(WasmError::Instantiate(format!(
            "offset must be an i32 constant, got {:?}",
            other.val_type()
        ))),
    }
}

/// Dispatches an imported host-function call on behalf of a compiled stub.
unsafe extern "C" fn host_call(
    ctx: *const u8,
    ordinal: u64,
    args: *mut u64,
    results: *mut u64,
) -> u64 {
    // SAFETY: `ctx` is a live `VmCtx` whose `dispatch` points to a live
    // `AotDispatchState` for the duration of the call.
    unsafe {
        let vmctx = &*(ctx as *const VmCtx);
        if vmctx.dispatch.is_null() {
            return 1;
        }
        let dispatch = &*(vmctx.dispatch as *const AotDispatchState);

        let Some(func) = dispatch.host_funcs.get(ordinal as usize) else {
            return 1;
        };
        let func_type = &dispatch.host_types[ordinal as usize];

        // Host boundary: commit the fuel this invocation has accumulated in
        // its local cell into the authoritative meter and refresh its
        // allowance, so the guest continues against an up-to-date budget (a
        // raise or reset mid-invocation is seen here) and a host function that
        // queries the meter sees the guest work done so far.
        drain_invocation_meter(&dispatch.meter);

        let wasm_args: Vec<WasmValue> = func_type
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| slot_to_value(*args.add(index), param))
            .collect();

        let result = {
            let Ok(mut store) = dispatch.store.lock() else {
                return 1;
            };
            let mut caller = HostCaller::new(&mut store, &dispatch.memories);
            func.call(&mut caller, &wasm_args)
        };

        // Refresh again: the host function may have re-entered this instance,
        // and that nested invocation's fuel is already committed.
        drain_invocation_meter(&dispatch.meter);

        match result {
            Ok(values) => {
                if values.len() != func_type.results.len() {
                    return 1;
                }
                for (index, value) in values.iter().enumerate() {
                    *results.add(index) = value_to_slot(value);
                }
                0
            }
            Err(_) => 1,
        }
    }
}

/// `memory.copy`: memmove semantics across (possibly the same) memory.
unsafe extern "C" fn memory_copy(
    ctx: *const u8,
    dst_idx: u64,
    src_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> u64 {
    match memory_copy_impl(ctx, dst_idx, src_idx, dst, src, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn memory_copy_impl(
    ctx: *const u8,
    dst_idx: u64,
    src_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| oob_trap())?;
    let src = u32::try_from(src).map_err(|_| oob_trap())?;
    let len = u32::try_from(len).map_err(|_| oob_trap())?;
    let dst_mem = dispatch_memory(ctx, dst_idx)?;
    let src_mem = dispatch_memory(ctx, src_idx)?;

    const CHUNK: usize = 4096;
    let mut buf = vec![0u8; CHUNK.min(len as usize)];

    if Arc::ptr_eq(&dst_mem, &src_mem) {
        let mut memory = dst_mem.lock().map_err(|_| poisoned_lock())?;
        check_memory_range(&memory, dst, len)?;
        check_memory_range(&memory, src, len)?;
        if src < dst {
            let mut remaining = len;
            while remaining > 0 {
                let chunk = CHUNK.min(remaining as usize);
                let off = remaining - chunk as u32;
                memory.read(src + off, &mut buf[..chunk])?;
                memory.write(dst + off, &buf[..chunk])?;
                remaining -= chunk as u32;
            }
        } else {
            let mut offset = 0u32;
            while offset < len {
                let chunk = CHUNK.min((len - offset) as usize);
                memory.read(src + offset, &mut buf[..chunk])?;
                memory.write(dst + offset, &buf[..chunk])?;
                offset += chunk as u32;
            }
        }
    } else {
        let mut dst_lock = dst_mem.lock().map_err(|_| poisoned_lock())?;
        let src_lock = src_mem.lock().map_err(|_| poisoned_lock())?;
        if !src_lock.is_valid_access(src, len as usize)?
            || !dst_lock.is_valid_access(dst, len as usize)?
        {
            return Err(oob_trap());
        }
        dst_lock.check_writable(dst, len as usize)?;
        let mut offset = 0u32;
        while offset < len {
            let chunk = CHUNK.min((len - offset) as usize);
            src_lock.read(src + offset, &mut buf[..chunk])?;
            dst_lock.write(dst + offset, &buf[..chunk])?;
            offset += chunk as u32;
        }
    }
    Ok(())
}

/// `memory.fill`: fills `len` bytes at `dst` with `val & 0xFF`.
unsafe extern "C" fn memory_fill(
    ctx: *const u8,
    mem_idx: u64,
    dst: u64,
    val: u64,
    len: u64,
) -> u64 {
    match memory_fill_impl(ctx, mem_idx, dst, val, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn memory_fill_impl(ctx: *const u8, mem_idx: u64, dst: u64, val: u64, len: u64) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| oob_trap())?;
    let len = u32::try_from(len).map_err(|_| oob_trap())?;
    let memory = dispatch_memory(ctx, mem_idx)?;
    let mut memory = memory.lock().map_err(|_| poisoned_lock())?;
    check_memory_range(&memory, dst, len)?;

    const CHUNK: usize = 4096;
    let chunk_buf = vec![val as u8; CHUNK.min(len as usize)];
    let mut offset = 0u32;
    while offset < len {
        let chunk = CHUNK.min((len - offset) as usize);
        memory.write(dst + offset, &chunk_buf[..chunk])?;
        offset += chunk as u32;
    }
    Ok(())
}

/// `memory.grow`: grows memory `mem_idx` by `delta` pages; returns the old
/// size or `-1`.
///
/// The instance meter's memory-page budget is checked *inside* the grow
/// critical section, so concurrent grows cannot both pass the check and push
/// the instance past its configured budget. The critical section is the
/// instance's full memory-lock set (taken in ascending index order; only
/// this libcall ever holds more than one memory lock, so the order cannot
/// deadlock) because the budget counts committed pages across the whole
/// instance — matching the interpreter's grow path and `stats` — and
/// concurrent invocations growing *different* memories must not race the
/// check either. A budget overrun traps with `MemoryLimitExceeded`
/// (matching the interpreter's guest `memory.grow`); declared-maximum
/// failures keep returning `-1`, which the specification permits for any
/// grow failure.
unsafe extern "C" fn memory_grow(ctx: *const u8, mem_idx: u64, delta: u64) -> u64 {
    // Resolving `mem_idx` also validates it against the dispatch state's
    // memory directory.
    if dispatch_memory(ctx, mem_idx).is_err() {
        return u64::from(u32::MAX);
    }
    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch(ctx) }) else {
        return u64::from(u32::MAX);
    };

    let mut guards = Vec::with_capacity(dispatch.memories.len());
    for memory in &dispatch.memories {
        match memory.lock() {
            Ok(guard) => guards.push(guard),
            Err(_) => return u64::from(u32::MAX),
        }
    }

    let Some(total) = guards
        .iter()
        .try_fold(0u32, |acc, memory| acc.checked_add(memory.size()))
    else {
        return u64::from(u32::MAX);
    };
    let Some(new_total) = total.checked_add(delta as u32) else {
        return u64::from(u32::MAX);
    };
    if dispatch.meter.ensure_memory_pages(new_total).is_err() {
        return LIBCALL_TRAP;
    }
    // The index was validated against this same directory above.
    match guards[mem_idx as usize].grow(delta as u32) {
        Ok(old) => u64::from(old),
        Err(_) => u64::from(u32::MAX),
    }
}

/// `memory.init`: copies `len` bytes from data segment `seg_idx` into memory.
unsafe extern "C" fn memory_init(
    ctx: *const u8,
    mem_idx: u64,
    seg_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> u64 {
    match memory_init_impl(ctx, mem_idx, seg_idx, dst, src, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn memory_init_impl(
    ctx: *const u8,
    mem_idx: u64,
    seg_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| oob_trap())?;
    let src = u32::try_from(src).map_err(|_| oob_trap())?;
    let len = u32::try_from(len).map_err(|_| oob_trap())?;

    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch(ctx) }) else {
        return Err(WasmError::Runtime("AOT dispatch state missing".to_string()));
    };
    let segment = dispatch
        .data_segments
        .get(seg_idx as usize)
        .ok_or_else(|| WasmError::Runtime(format!("data segment {seg_idx} not found")))?;
    let available = dispatch
        .data_available
        .get(seg_idx as usize)
        .map(|flag| flag.load(Ordering::SeqCst))
        == Some(true);
    let segment_len = if available { segment.len() as u32 } else { 0 };
    let src_end = src.checked_add(len).ok_or_else(oob_trap)?;
    if src_end > segment_len {
        return Err(oob_trap());
    }
    let bytes = if available && len > 0 {
        segment[src as usize..src_end as usize].to_vec()
    } else {
        Vec::new()
    };
    let memory = dispatch_memory(ctx, mem_idx)?;
    memory
        .lock()
        .map_err(|_| poisoned_lock())?
        .write(dst, &bytes)
}

/// Limits subtyping for an imported memory, mirroring the interpreter's
/// `memory_matches_required`.
fn memory_matches_required(actual: &Memory, required: &crate::runtime::MemoryType) -> bool {
    actual.size() >= required.limits.min()
        && actual.type_().shared == required.shared
        && match (actual.type_().limits.max(), required.limits.max()) {
            (_, None) => true,
            (Some(actual_max), Some(required_max)) => actual_max <= required_max,
            (None, Some(_)) => false,
        }
}

fn memory_notify(ctx: *const u8, mem_idx: u64, addr: u64, count: u64) -> Result<u32> {
    let addr = u32::try_from(addr).map_err(|_| oob_trap())?;
    // Natural alignment for a 4-byte notify, matching interpreter semantics.
    if addr % 4 != 0 {
        return Err(oob_trap());
    }
    let memory = dispatch_memory(ctx, mem_idx)?;
    // `Memory::notify` performs the owned/shared bounds check itself.
    memory
        .lock()
        .map_err(|_| poisoned_lock())?
        .notify(addr, count as u32)
}

/// `memory.size`: returns the memory's owned size in pages.
unsafe extern "C" fn memory_size(ctx: *const u8, mem_idx: u64) -> u64 {
    let Ok(memory) = dispatch_memory(ctx, mem_idx) else {
        return LIBCALL_TRAP;
    };
    match memory.lock() {
        Ok(memory) => u64::from(memory.size()),
        Err(_) => LIBCALL_TRAP,
    }
}

fn memory_wait(
    ctx: *const u8,
    mem_idx: u64,
    addr: u64,
    expected: u64,
    timeout: u64,
    is_64: bool,
) -> Result<i32> {
    let addr = u32::try_from(addr).map_err(|_| oob_trap())?;
    let access_width = if is_64 { 8u32 } else { 4u32 };
    if addr % access_width != 0 {
        return Err(oob_trap());
    }
    let memory = dispatch_memory(ctx, mem_idx)?;

    // Bounds-checked read, compare, and waiter registration happen under the
    // memory lock; the registry keeps the waiter reachable after the lock
    // drops, so the park below never holds the memory lock — a parked waiter
    // must not block a notifier on another thread.
    let registry = {
        let memory = memory.lock().map_err(|_| poisoned_lock())?;
        // Mirrors the interpreter's `do_wait`: bounds-checked read, compare,
        // then register a waiter before dropping the lock to sleep.
        let actual = if is_64 {
            memory.read_i64(addr)? as i64
        } else {
            memory.read_i32(addr)? as i64
        };
        if actual != expected as i64 {
            return Ok(1);
        }
        memory.waiter_registry(addr)
    };

    // Nanosecond timeout: negative means wait forever.
    let timeout_ns = if (timeout as i64) < 0 {
        u64::MAX
    } else {
        timeout
    };

    let woken = registry.park(timeout_ns);
    Ok(if woken { 0 } else { 2 })
}

fn oob_trap() -> WasmError {
    WasmError::Trap(TrapCode::MemoryOutOfBounds)
}

fn poisoned_case() -> WasmError {
    WasmError::Runtime("AOT store lock poisoned".to_string())
}

fn poisoned_lock() -> WasmError {
    WasmError::Runtime("table lock poisoned".to_string())
}

fn poisoned_table() -> WasmError {
    WasmError::Runtime("AOT table lock poisoned".to_string())
}

fn slot_to_value(slot: u64, ty: &ValType) -> WasmValue {
    match ty {
        ValType::Num(NumType::I32) => WasmValue::I32(slot as u32 as i32),
        ValType::Num(NumType::I64) => WasmValue::I64(slot as i64),
        ValType::Num(NumType::F32) => WasmValue::F32(f32::from_bits(slot as u32)),
        ValType::Num(NumType::F64) => WasmValue::F64(f64::from_bits(slot)),
        ValType::Ref(RefType::FuncRef) if slot == 0 => WasmValue::NullRef(RefType::FuncRef),
        ValType::Ref(RefType::FuncRef) => WasmValue::FuncRef(slot as u32),
        ValType::Ref(RefType::ExternRef) if slot == 0 => WasmValue::NullRef(RefType::ExternRef),
        // Externref slots are host index + 1 (slot 0 is the null sentinel), so
        // a genuine host externref with index 0 round-trips as slot 1.
        ValType::Ref(RefType::ExternRef) => WasmValue::ExternRef((slot - 1) as u32),
    }
}

/// `table.copy`: copies `len` handles between tables (memmove within one).
unsafe extern "C" fn table_copy(
    ctx: *const u8,
    dst_idx: u64,
    src_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> u64 {
    match table_copy_impl(ctx, dst_idx, src_idx, dst, src, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn table_copy_impl(
    ctx: *const u8,
    dst_idx: u64,
    src_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| table_trap())?;
    let src = u32::try_from(src).map_err(|_| table_trap())?;
    let len = u32::try_from(len).map_err(|_| table_trap())?;
    let dst_end = dst.checked_add(len).ok_or_else(table_trap)?;
    let src_end = src.checked_add(len).ok_or_else(table_trap)?;

    let dst_tbl = dispatch_table(ctx, dst_idx)?;
    let src_tbl = dispatch_table(ctx, src_idx)?;

    if Arc::ptr_eq(&dst_tbl, &src_tbl) {
        let mut table = dst_tbl.lock().map_err(|_| poisoned_table())?;
        if src_end as usize > table.cells.len() || dst_end as usize > table.cells.len() {
            return Err(table_trap());
        }
        if len > 0 {
            table
                .cells
                .copy_within(src as usize..src_end as usize, dst as usize);
        }
    } else {
        let mut dst_cells = dst_tbl.lock().map_err(|_| poisoned_table())?;
        let src_cells = src_tbl.lock().map_err(|_| poisoned_table())?;
        if src_end as usize > src_cells.cells.len() || dst_end as usize > dst_cells.cells.len() {
            return Err(table_trap());
        }
        if len > 0 {
            dst_cells.cells[dst as usize..dst_end as usize]
                .copy_from_slice(&src_cells.cells[src as usize..src_end as usize]);
        }
    }
    Ok(())
}

/// `table.fill`: sets `len` entries at `dst` to `val`.
unsafe extern "C" fn table_fill(
    ctx: *const u8,
    table_idx: u64,
    dst: u64,
    val: u64,
    len: u64,
) -> u64 {
    match table_fill_impl(ctx, table_idx, dst, val, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn table_fill_impl(ctx: *const u8, table_idx: u64, dst: u64, val: u64, len: u64) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| table_trap())?;
    let len = u32::try_from(len).map_err(|_| table_trap())?;
    let end = dst.checked_add(len).ok_or_else(table_trap)?;
    let table = dispatch_table(ctx, table_idx)?;
    let mut table = table.lock().map_err(|_| poisoned_table())?;
    if end as usize > table.cells.len() {
        return Err(table_trap());
    }
    if len > 0 {
        table.cells[dst as usize..end as usize].fill(val as u32);
    }
    Ok(())
}

/// `table.grow`: appends `delta` copies of `init`; returns old size or `-1`.
///
/// The cell storage is capacity-reserved at table creation, so growth within
/// the reservation never reallocates and the `base` published to compiled
/// code stays valid. The new length is published to the shared holder *after*
/// the new cells are initialised; see [`TableCells`] for the concurrency
/// contract. Growth beyond the reservation fails with `-1`, which the
/// specification permits.
unsafe extern "C" fn table_grow(ctx: *const u8, table_idx: u64, delta: u64, init: u64) -> u64 {
    let Ok(table) = dispatch_table(ctx, table_idx) else {
        return u64::from(u32::MAX);
    };
    let mut table = match table.lock() {
        Ok(table) => table,
        Err(_) => return u64::from(u32::MAX),
    };
    let old = table.cells.len() as u32;
    let Some(new) = old.checked_add(delta as u32) else {
        return u64::from(u32::MAX);
    };
    if let Some(max) = table.type_.limits.max()
        && new > max
    {
        return u64::from(u32::MAX);
    }
    if new as usize > table.cells.capacity() {
        // Beyond the reserved capacity: fail rather than reallocate, which
        // would invalidate the base pointer already published to compiled
        // code in every instance that can reach this table.
        return u64::from(u32::MAX);
    }
    table.cells.resize(new as usize, init as u32);
    table.holder.len = new;
    u64::from(old)
}

/// `table.init`: copies `len` handles from element segment `seg_idx`.
unsafe extern "C" fn table_init(
    ctx: *const u8,
    seg_idx: u64,
    table_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> u64 {
    match table_init_impl(ctx, seg_idx, table_idx, dst, src, len) {
        Ok(()) => 0,
        Err(_) => LIBCALL_TRAP,
    }
}

fn table_init_impl(
    ctx: *const u8,
    seg_idx: u64,
    table_idx: u64,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let dst = u32::try_from(dst).map_err(|_| table_trap())?;
    let src = u32::try_from(src).map_err(|_| table_trap())?;
    let len = u32::try_from(len).map_err(|_| table_trap())?;
    let dst_end = dst.checked_add(len).ok_or_else(table_trap)?;
    let src_end = src.checked_add(len).ok_or_else(table_trap)?;

    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch(ctx) }) else {
        return Err(WasmError::Runtime("AOT dispatch state missing".to_string()));
    };
    let segment = dispatch
        .elem_segments
        .get(seg_idx as usize)
        .ok_or_else(|| WasmError::Runtime(format!("element segment {seg_idx} not found")))?;
    let available = dispatch
        .elem_available
        .get(seg_idx as usize)
        .map(|flag| flag.load(Ordering::SeqCst))
        == Some(true);
    let segment_len = if available { segment.len() as u32 } else { 0 };
    if src_end > segment_len {
        return Err(table_trap());
    }

    let table = dispatch_table(ctx, table_idx)?;
    let mut table = table.lock().map_err(|_| poisoned_table())?;
    if dst_end as usize > table.cells.len() {
        return Err(table_trap());
    }
    if len > 0 {
        table.cells[dst as usize..dst_end as usize]
            .copy_from_slice(&segment[src as usize..src_end as usize]);
    }
    Ok(())
}

/// Table type subtyping for an imported table, mirroring the interpreter.
fn table_matches_required(actual: &AotTable, required: &crate::runtime::TableType) -> bool {
    actual.type_.elem_type == required.elem_type
        && (actual.type_.nullable == required.nullable
            || (!actual.type_.nullable && required.nullable))
        && actual.cells.len() as u32 >= required.limits.min()
        && match (actual.type_.limits.max(), required.limits.max()) {
            (_, None) => true,
            (Some(actual_max), Some(required_max)) => actual_max <= required_max,
            (None, Some(_)) => false,
        }
}

/// `table.size`: returns the table's element count.
unsafe extern "C" fn table_size(ctx: *const u8, table_idx: u64) -> u64 {
    let Ok(table) = dispatch_table(ctx, table_idx) else {
        return LIBCALL_TRAP;
    };
    match table.lock() {
        Ok(table) => u64::from(table.cells.len() as u32),
        Err(_) => LIBCALL_TRAP,
    }
}

fn table_trap() -> WasmError {
    WasmError::Trap(TrapCode::TableOutOfBounds)
}

fn validate_args(args: &[WasmValue], func_type: &FunctionType) -> Result<()> {
    if args.len() != func_type.params.len() {
        return Err(WasmError::Runtime(format!(
            "function expects {} arguments, got {}",
            func_type.params.len(),
            args.len()
        )));
    }
    for (value, param) in args.iter().zip(func_type.params.iter()) {
        if value.val_type() != *param {
            return Err(WasmError::Runtime(format!(
                "argument type mismatch: expected {param:?}, got {:?}",
                value.val_type()
            )));
        }
    }
    Ok(())
}

fn value_to_slot(value: &WasmValue) -> u64 {
    match value {
        WasmValue::I32(v) => (*v as u32) as u64,
        WasmValue::I64(v) => *v as u64,
        WasmValue::F32(v) => v.to_bits() as u64,
        WasmValue::F64(v) => v.to_bits(),
        WasmValue::FuncRef(h) => (*h) as u64,
        // See `slot_to_value`: externref slots reserve 0 for null.
        WasmValue::ExternRef(h) => (*h) as u64 + 1,
        WasmValue::NullRef(_) => 0,
    }
}
