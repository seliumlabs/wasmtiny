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
///
/// # Per-invocation copies (concurrent execution contract)
///
/// [`AotInstance::invoke_shared`](crate::aot::AotInstance::invoke_shared)
/// runs with a *copy* of this struct, not the shared one. The contract of
/// that copy is:
///
/// - **`stack_limit` is per-invocation.** The copy refreshes it to the
///   invoking thread's native-stack bound, so every direct callee on that
///   invocation checks a correct, thread-relative limit while the shared
///   context stays disabled (0) and unusable by other threads.
/// - **`stack_pointer` is per-invocation when the module has a shadow
///   stack.** Compiled `global.get`/`global.set` of the module's
///   `__stack_pointer` global read/write this field directly (never the
///   globals array — the compiler routes it). [`AotInstance::invoke_shared`]
///   (crate::aot::AotInstance::invoke_shared) points the copy at a private
///   stack slot by writing `stack_pointer`; the shared context keeps the
///   module's initial value for the single-threaded [`invoke`]
///   (crate::aot::AotInstance::invoke) path.
/// - **`globals` is always shared by pointer.** Ordinary mutable globals are
///   read and written in place through the one array — identical
///   unsynchronised-shared semantics to modules without a shadow stack, so
///   concurrent invocations observe each other's writes (non-atomic guest
///   read-modify-write cycles are racy by design, as on any shared memory).
/// - **Everything else is shared by pointer.** The copy carries the same
///   `memories`, `tables`, `funcs`, `store_funcs`, `type_ids`, `libcalls`,
///   `dispatch` and `refs` pointers as the shared context.
///
/// A rustc-compiled module uses the `__stack_pointer` shadow-stack global for
/// every frame; the per-invocation stack slot above is what lets several host
/// threads enter one instance without overlapping frames. Modules that also
/// use `__tls_base` (real per-thread TLS) need an engine-side per-thread TLS
/// block with `__wasm_init_tls`, which is not implemented; without it a
/// `__tls_base`-relative thread-local is shared across invocations. Tests
/// pinning this contract live in `crates/aotc/tests/concurrency.rs`
/// (`concurrent_mutable_global_access_is_correct`,
/// `concurrent_invocations_share_mutable_globals`, and
/// `shadow_stack_is_per_invocation`).
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
    /// The shadow-stack pointer. Compiled `__stack_pointer` `global.get`/`set`
    /// read/write this field; per-invocation on the concurrent path (see the
    /// struct docs), the module's initial value on the shared context.
    pub stack_pointer: u32,
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
            stack_pointer: 0,
        }
    }
}
