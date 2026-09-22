//! Runtime `VmCtx` — the per-instance context passed as the hidden first
//! argument to every compiled wasm function.
//!
//! The layout is the ABI contract with `wasmtiny-aotc` (see
//! `environment::VmCtxOffsets` there); it is duplicated here by design because
//! the runtime never links the compiler.

/// A linear-memory descriptor, as seen by compiled code.
#[repr(C)]
pub struct MemoryDesc {
    /// Base pointer of the linear memory.
    pub base: *mut u8,
    /// Current accessible length in bytes (owned pages).
    pub len: usize,
    /// Reserved capacity in bytes (heap bound).
    pub capacity: usize,
}

/// Shared, per-table cell state published to compiled code.
///
/// Every instance that can reach a table (defined or imported) receives a
/// pointer to the *same* holder, so a `table.grow` performed by any instance
/// is observed by every compiled caller — the old per-instance descriptor
/// snapshot went stale (and, worse, dangling after a cell reallocation) the
/// moment a table grew.
///
/// Concurrency contract:
///
/// - `base` is **immutable** after creation: the cell storage is
///   capacity-reserved when the table is built and never reallocated, only
///   `len` advances within the reservation. The whole reservation is
///   initialised to the null handle, so a racy `len` read can never expose
///   an uninitialised cell — at worst a stale bound traps the access or a
///   null entry traps `IndirectCallToNull`.
/// - `len` is a plain `u32` written under the table's mutex by `table.grow`
///   *after* the new cells are initialised, and read without the mutex by
///   compiled bounds checks. This is trusted memory in the same sense as
///   the memory descriptors: an aligned 32-bit read observes either the old
///   or the new length, both of which are safe bounds.
#[repr(C)]
pub struct TableCells {
    /// Pointer to the array of 4-byte funcref handles (immutable).
    pub base: *mut u8,
    /// Current element count.
    pub len: u32,
    /// Padding to keep the holder 16-byte aligned.
    pub _pad: u32,
}

/// A "fat" call target: the callee's entry plus its own context and canonical
/// signature id, so imported and cross-module calls dispatch correctly.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct FuncDesc {
    /// Native entry pointer (or trampoline).
    pub entry: *const u8,
    /// Pointer to the callee's own `VmCtx`.
    pub vmctx: *const u8,
    /// Canonical signature id (u32).
    pub type_id: u32,
    /// Padding.
    pub _pad: u32,
}

/// The hidden per-instance context. Pointer-sized fields for 64-bit targets.
///
/// `Copy` because a concurrent invocation clones the context (adjusting only
/// the per-invocation stack limit) instead of writing the shared one.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct VmCtx {
    /// Array of memory descriptors.
    pub memories: *const MemoryDesc,
    /// Array of table slots: one pointer per table index to the shared
    /// [`TableCells`] holder (see the struct docs for the concurrency
    /// contract).
    pub tables: *const *const TableCells,
    /// Array of 8-byte global cells.
    pub globals: *mut u8,
    /// Array of [`FuncDesc`] (module-local: imports first, then defined).
    pub funcs: *const FuncDesc,
    /// Store-wide array of [`FuncDesc`], indexed by store-native handle.
    pub store_funcs: *const FuncDesc,
    /// Array of canonical signature ids, indexed by module type index.
    pub type_ids: *const u32,
    /// Runtime libcall table (opaque here; concrete in the execution layer).
    pub libcalls: *const u8,
    /// Lowest allowed stack pointer before the stack-overflow trap.
    pub stack_limit: usize,
    /// Opaque per-instance dispatch state owned by the runtime.
    pub dispatch: *mut core::ffi::c_void,
    /// Array of store-native funcref handles (u32 per function index).
    pub refs: *const u32,
}

// SAFETY: `FuncDesc` stores raw pointers into instance-owned code/images that
// live for the store's lifetime and are never mutated after the store exists;
// they are only dereferenced by compiled code during a call on the invoking
// thread.
unsafe impl Send for FuncDesc {}

unsafe impl Sync for FuncDesc {}

impl VmCtx {
    /// A context whose every region is empty/null — safe as long as no field
    /// is dereferenced by the callee (e.g. functions with no memory/global/
    /// table/call access).
    pub fn empty() -> Self {
        let dangling = std::ptr::NonNull::<u8>::dangling().as_ptr();
        Self {
            memories: dangling.cast(),
            tables: dangling.cast(),
            globals: dangling,
            funcs: dangling.cast(),
            store_funcs: dangling.cast(),
            type_ids: dangling.cast(),
            libcalls: dangling,
            stack_limit: usize::MAX,
            dispatch: std::ptr::null_mut(),
            refs: dangling.cast(),
        }
    }
}
