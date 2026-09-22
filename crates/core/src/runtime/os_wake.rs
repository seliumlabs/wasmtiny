//! Platform wake-primitive emission (Stage 2 of host-region wait/notify).
//!
//! When compiled in, a guest `memory.atomic.notify` executed on a
//! shared-range address additionally emits the host platform's wake
//! primitive for the region's **host mapping** address
//! (`SharedRegion::ptr() + offset`), so embedder threads sleeping in the
//! matching OS wait-word primitive wake without touching the in-process
//! waiter registry.
//!
//! Emission is strictly a side effect: the notify instruction's return
//! value continues to count only WASM waiters woken, per the threads
//! proposal. Enablement is purely a **build-time** decision: the
//! `platform-wake-emission` cargo feature compiles the backends in, and
//! `COMPILED_IN`/[`active`] are immutable constants. There is no runtime
//! toggle, so the capability level is process-wide and identical for
//! every store and registry — embedders detect it once via
//! `SharedMemoryRegistry::host_wait_support()`. CI gates the feature per
//! platform on the cross-thread conformance test passing.
//!
//! Platform table (with the feature enabled):
//! - Linux: `futex(FUTEX_WAKE)`
//! - Windows: `WakeByAddress`
//! - FreeBSD: `_umtx_op(UMTX_OP_WAKE_PRIVATE)`
//! - All other targets compile to a no-op.
//!
//! macOS is deliberately **not** compiled in: Darwin kernels reject all
//! wait-word primitives (`__ulock_wait`, `os_sync_wait_on_address`, the
//! restricted futex syscall) with EFAULT/EPERM when the target word lives
//! in `MAP_SHARED` memory, so emission cannot function there regardless
//! of configuration. The `__ulock_wake` backend below is retained for
//! the day that changes.
//!
//! Cross-mapping caveat: on Linux, `FUTEX_WAKE` on a `MAP_SHARED` mapping
//! is keyed by the underlying inode, so a wake reaches waiters parked on
//! *other mappings* of the same shm object. On Windows and FreeBSD the
//! wake primitives are keyed by virtual address, so emission only reaches
//! waiters parked on the engine's own mapping of the region — which is
//! the in-process embedder case (e.g. Selium's host proxies), but not
//! cross-mapping wakes.

/// True when the emission code is compiled in for this target.
///
/// Requires the `platform-wake-emission` cargo feature AND a supported
/// OS. macOS is excluded: Darwin rejects wait-word primitives on
/// `MAP_SHARED` memory, so emission cannot function there (see module
/// docs). The `__ulock_wake` backend is retained for future enablement.
pub const COMPILED_IN: bool = cfg!(all(
    feature = "platform-wake-emission",
    any(
        target_os = "linux",
        target_os = "windows",
        target_os = "freebsd"
    )
));

/// Returns true when emission is compiled in.
///
/// A constant today (build-time decision); kept as a function so the
/// call sites in `Memory::notify` read as "is emission active here".
pub fn active() -> bool {
    COMPILED_IN
}

/// Emits the platform wake primitive for the 4-byte word at `ptr`.
///
/// # Safety
///
/// `ptr` must point to mapped, readable memory (the shared region's host
/// mapping) that remains valid for the duration of the call.
pub unsafe fn emit_wake(ptr: *mut u8) {
    if !active() {
        return;
    }
    // SAFETY: contract is documented above; each platform backend requires
    // ptr to be a valid, aligned address in the current address space.
    #[cfg(target_os = "linux")]
    unsafe {
        // FUTEX_WAKE with an effectively unbounded count; futexes are keyed
        // by physical page + offset for shared mappings, so waiters on other
        // mappings of the same shm pages are woken too.
        let _ = libc::syscall(
            libc::SYS_futex,
            ptr.cast::<libc::c_void>(),
            libc::FUTEX_WAKE,
            i32::MAX,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<libc::c_void>(),
            0,
        );
    }

    #[cfg(target_os = "macos")]
    unsafe {
        // Never reachable today: macOS is excluded from `COMPILED_IN`, so
        // `active()` is always false there. Retained against the day Darwin
        // allows wait-word primitives on MAP_SHARED memory.
        const UL_COMPARE_AND_WAIT: u32 = 0x0000_0001;
        // Undocumented libsystem symbol; Stage 2 only, cfg-isolated, and
        // gated by the conformance test in CI.
        unsafe extern "C" {
            fn __ulock_wake(operation: u32, addr: *mut libc::c_void, wake_value: u64) -> i32;
        }
        let _ = __ulock_wake(UL_COMPARE_AND_WAIT, ptr.cast::<libc::c_void>(), 0);
    }

    #[cfg(target_os = "windows")]
    unsafe {
        unsafe extern "system" {
            #[link_name = "WakeByAddress"]
            fn wake_by_address(addr: *mut libc::c_void);
        }
        wake_by_address(ptr.cast::<libc::c_void>());
    }

    #[cfg(target_os = "freebsd")]
    unsafe {
        // sys/umtx.h: UMTX_OP_WAKE_PRIVATE wakes private-address waiters.
        const UMTX_OP_WAKE_PRIVATE: libc::c_int = 15;
        unsafe extern "C" {
            fn _umtx_op(
                obj: *mut libc::c_void,
                op: libc::c_int,
                val: libc::c_ulong,
                uaddr: *mut libc::c_void,
                uaddr2: *mut libc::c_void,
            ) -> libc::c_int;
        }
        let _ = _umtx_op(
            ptr.cast::<libc::c_void>(),
            UMTX_OP_WAKE_PRIVATE,
            libc::c_ulong::MAX,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }

    // Other platforms: no emission (compiled to nothing).
    #[allow(unused_variables)]
    let _ = ptr;
}
