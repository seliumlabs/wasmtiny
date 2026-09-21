//! Native execution: invoking a finish-linked function through its entry
//! trampoline, dispatching imported host functions, and wiring the shared
//! store-wide function/table state used by `call_indirect`.

use std::sync::{Arc, Mutex};

use crate::runtime::{
    DataKind, ElemKind, ExportKind, ExportType, FunctionType, Global, GlobalType, HostCaller,
    HostFunc, ImportKind, Memory, NumType, RefType, Result, Store, TrapCode, ValType, WasmError,
    WasmValue, evaluate_const_expr,
};

use super::{
    code::ExecutableCode,
    context::{FuncDesc, MemoryDesc, TableCells, VmCtx},
    loader::{AotFunction, AotModule},
    store::{AotExtern, AotStore, AotTable, SharedAotStore},
    traps,
};

/// Budget of host stack (in bytes) granted to wasm recursion before the
/// entry-time stack check traps.
const MAX_WASM_STACK: usize = 256 * 1024;

/// The fixed shape of an array-call entry trampoline:
/// `(vmctx, callee, args: *const u64, results: *mut u64) -> ()`.
type ArrayCall = unsafe extern "C" fn(*const VmCtx, *const u8, *const u64, *mut u64);

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
    /// Whether each data segment is still available to `memory.init`.
    data_available: Vec<bool>,
    /// Element-segment store handles (resolved from `refs`), indexed by
    /// unified element-segment index.
    elem_segments: Vec<Vec<u32>>,
    /// Whether each element segment is still available to `table.init`.
    elem_available: Vec<bool>,
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

/// Borrows the per-instance dispatch state mutably.
///
/// # Safety
/// `ctx` must point at a live [`VmCtx`] whose `dispatch` references a live
/// [`AotDispatchState`] for the duration of the native call, and no other
/// reference to that state may be alive during the borrow.
unsafe fn dispatch_mut<'a>(ctx: *const u8) -> Option<&'a mut AotDispatchState> {
    let vmctx = unsafe { &*(ctx as *const VmCtx) };
    if vmctx.dispatch.is_null() {
        None
    } else {
        Some(unsafe { &mut *(vmctx.dispatch as *mut AotDispatchState) })
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

// ---------------------------------------------------------------------------
// Memory-atomic libcalls.
//
// Each returns a packed `u64`: the high word is a non-zero trap flag (always
// `MemoryOutOfBounds` in practice) and the low word is the result. Compiled
// code traps on a non-zero high word and otherwise sign-extends the low word.
// ---------------------------------------------------------------------------

/// Packed trap sentinel shifted into the high word of a libcall result.
const LIBCALL_TRAP: u64 = 1 << 32;

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

    {
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
        memory.get_waiter(addr);
    }

    // Nanosecond timeout: negative means wait forever.
    let timeout_ns = if (timeout as i64) < 0 {
        u64::MAX
    } else {
        timeout
    };

    let woken = memory
        .lock()
        .map_err(|_| poisoned_lock())?
        .wait_on(addr, timeout_ns);
    Ok(if woken { 0 } else { 2 })
}

fn oob_trap() -> WasmError {
    WasmError::Trap(TrapCode::MemoryOutOfBounds)
}

fn table_trap() -> WasmError {
    WasmError::Trap(TrapCode::TableOutOfBounds)
}

// ---------------------------------------------------------------------------
// Memory / table runtime-op libcalls (memory.size, memory.grow, bulk memory,
// table size/grow/copy/fill/init). Packed return: high word non-zero = trap,
// low word = signed i32 result.
// ---------------------------------------------------------------------------

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

/// `memory.grow`: grows by `delta` pages; returns the old size or `-1`.
unsafe extern "C" fn memory_grow(ctx: *const u8, mem_idx: u64, delta: u64) -> u64 {
    let Ok(memory) = dispatch_memory(ctx, mem_idx) else {
        return u64::from(u32::MAX);
    };
    match memory.lock() {
        Ok(mut memory) => match memory.grow(delta as u32) {
            Ok(old) => u64::from(old),
            Err(_) => u64::from(u32::MAX),
        },
        Err(_) => u64::from(u32::MAX),
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

/// `data.drop`: marks data segment `seg_idx` no longer available.
unsafe extern "C" fn data_drop(ctx: *const u8, seg_idx: u64) -> u64 {
    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch_mut(ctx) }) else {
        return LIBCALL_TRAP;
    };
    match dispatch.data_available.get_mut(seg_idx as usize) {
        Some(flag) => {
            *flag = false;
            0
        }
        None => LIBCALL_TRAP,
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

/// `elem.drop`: marks element segment `seg_idx` no longer available.
unsafe extern "C" fn elem_drop(ctx: *const u8, seg_idx: u64) -> u64 {
    // SAFETY: the dispatch state outlives the native call.
    let Some(dispatch) = (unsafe { dispatch_mut(ctx) }) else {
        return LIBCALL_TRAP;
    };
    match dispatch.elem_available.get_mut(seg_idx as usize) {
        Some(flag) => {
            *flag = false;
            0
        }
        None => LIBCALL_TRAP,
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
    let available = dispatch.data_available.get(seg_idx as usize) == Some(&true);
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
    let available = dispatch.elem_available.get(seg_idx as usize) == Some(&true);
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

fn poisoned_table() -> WasmError {
    WasmError::Runtime("AOT table lock poisoned".to_string())
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
    _libcalls: Box<LibcallTable>,
    _dispatch: Box<AotDispatchState>,
    _store: SharedAotStore,
    _registered: traps::RegisteredCode,
}

type SharedTableTag = Arc<Mutex<AotTable>>;

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
        // Set a thread-relative stack limit at instantiation so even
        // cross-module callees (invoked through their own vmctx) have a
        // correct bound before their first direct `invoke`.
        ctx.stack_limit = traps::thread_stack_limit(MAX_WASM_STACK);
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
        let data_available: Vec<bool> = module
            .data
            .iter()
            .map(|segment| !matches!(segment.kind, DataKind::Active { .. }))
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
        let elem_available: Vec<bool> = module
            .elems
            .iter()
            .map(|segment| matches!(segment.kind, ElemKind::Passive))
            .collect();

        let rt_store = Arc::new(Mutex::new(Store::new()));
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
            _registered: registered,
        };

        // Run the start function, if any, after the instance is fully wired.
        if let Some(start) = module.start {
            instance.invoke_start(start)?;
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
    pub fn invoke_export(&mut self, name: &str, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let index = self
            .export_func_index(name)
            .ok_or_else(|| WasmError::Runtime(format!("function export '{name}' not found")))?;
        self.invoke(index, args)
    }

    /// Invokes the function `func_idx` natively.
    pub fn invoke(&mut self, func_idx: u32, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
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

        // The per-invocation stack limit bounds wasm recursion; it is
        // thread-relative so cross-module callees (which carry their own
        // vmctx) see a consistent bound.
        self.ctx.stack_limit = traps::thread_stack_limit(MAX_WASM_STACK);

        // SAFETY: offsets were validated against the code image length by the
        // loader; the trampoline and callee were emitted for this signature.
        let trampoline_ptr = unsafe { self.image.entry().add(function.trampoline_offset as usize) };
        let callee = unsafe { self.image.entry().add(function.code_offset as usize) };
        let trampoline: ArrayCall = unsafe { std::mem::transmute(trampoline_ptr) };

        // Native execution is wrapped in a signal-recovery boundary: a trap
        // returns here as `Err(Trap(code))` via the process-wide handler.
        traps::catch_traps(|| unsafe {
            trampoline(
                self.ctx.as_ref(),
                callee,
                arg_slots.as_ptr(),
                result_slots.as_mut_ptr(),
            );
        })?;

        decode_results(&result_slots, func_type)
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

fn poisoned_lock() -> WasmError {
    WasmError::Runtime("table lock poisoned".to_string())
}

fn poisoned_case() -> WasmError {
    WasmError::Runtime("AOT store lock poisoned".to_string())
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

fn decode_results(slots: &[u64], func_type: &FunctionType) -> Result<Vec<WasmValue>> {
    slots
        .iter()
        .zip(func_type.results.iter())
        .map(|(slot, result)| Ok(slot_to_value(*slot, result)))
        .collect()
}
