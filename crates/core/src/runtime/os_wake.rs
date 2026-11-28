//! Platform wake-primitive emission and host wait/wake (Stage 2 of
//! host-region wait/notify).
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
//! `SharedMemoryRegistry::host_wait_support()`.
//!
//! [`wait`] and [`wake`] expose the same primitives to embedders so host
//! waiters can park on the region's host mapping and be woken by the
//! engine's emission. [`SUPPORTED`] reports whether a backend exists for
//! the target at all; the conformance-gated capability is
//! `SharedMemoryRegistry::host_wait_support()`.
//!
//! Platform table:
//! - Linux: `futex(FUTEX_WAIT)` / `futex(FUTEX_WAKE)`
//! - Windows: `WaitOnAddress` / `WakeByAddressAll`
//! - FreeBSD: `_umtx_op(UMTX_OP_WAIT_UINT_PRIVATE)` /
//!   `_umtx_op(UMTX_OP_WAKE_PRIVATE)`
//! - All other targets: no backend ([`SUPPORTED`] is false).
//!
//! macOS is deliberately **not** supported: Darwin kernels reject all
//! wait-word primitives (`__ulock_wait`, `os_sync_wait_on_address`, the
//! restricted futex syscall) with EFAULT/EPERM when the target word lives
//! in `MAP_SHARED` memory, so parking and emission cannot function there
//! regardless of configuration. The `__ulock_*` backend below is retained
//! for the day that changes.
//!
//! Cross-mapping caveat: on Linux, `FUTEX_WAKE` on a `MAP_SHARED` mapping
//! is keyed by the underlying inode, so a wake reaches waiters parked on
//! *other mappings* of the same shm object. On Windows and FreeBSD the
//! wake primitives are keyed by virtual address, so emission only reaches
//! waiters parked on the engine's own mapping of the region — which is
//! the in-process embedder case (e.g. Selium's host proxies), but not
//! cross-mapping wakes.

use core::time::Duration;

/// True when a wait/wake backend is compiled in for this target.
///
/// This is purely a platform fact: macOS is excluded because Darwin rejects
/// wait-word primitives on `MAP_SHARED` memory (see module docs). Whether the
/// engine actually emits wakes is the separate, conformance-gated
/// [`COMPILED_IN`] / `SharedMemoryRegistry::host_wait_support()` signal.
pub const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd"
));

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

/// The result of [`wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The platform primitive delivered a wake.
    Woken,
    /// The timeout elapsed without a wake.
    TimedOut,
    /// The word already differed from `expected`, or the wait was
    /// interrupted by a signal. The caller MUST re-check the word.
    Recheck,
}

/// Returns true when emission is compiled in.
///
/// A constant today (build-time decision); kept as a function so the
/// call sites in `Memory::notify` read as "is emission active here".
pub fn active() -> bool {
    COMPILED_IN
}

/// Parks the current thread until the little-endian `u32` word at `ptr`
/// differs from `expected` or `timeout` elapses.
///
/// `Duration::MAX` blocks indefinitely. A word that already differs returns
/// [`WaitOutcome::Recheck`] without parking; callers always re-check the word
/// on every outcome except `TimedOut`.
///
/// # Safety
///
/// `ptr` must point to a mapped, writable, 4-byte-aligned word (the shared
/// region's host mapping) that remains valid for the duration of the call.
pub unsafe fn wait(ptr: *mut u8, expected: u32, timeout: Duration) -> WaitOutcome {
    // SAFETY: delegated to the caller; each backend follows the same contract.
    unsafe { wait_impl(ptr, expected, timeout) }
}

/// Wakes up to `count` threads parked on the little-endian `u32` word at
/// `ptr`, returning the number of wake attempts delivered.
///
/// Mirrors the engine's platform-wake emission so the two sides agree on the
/// wake primitive. Platforms without a backend return 0.
///
/// # Safety
///
/// `ptr` must point to a mapped, readable, 4-byte-aligned word that remains
/// valid for the duration of the call.
pub unsafe fn wake(ptr: *mut u8, count: u32) -> u32 {
    // SAFETY: delegated to the caller; each backend follows the same contract.
    unsafe { wake_impl(ptr, count) }
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
    // SAFETY: contract is documented above.
    unsafe {
        let _ = wake(ptr, u32::MAX);
    }
}

#[cfg(target_os = "linux")]
unsafe fn wait_impl(ptr: *mut u8, expected: u32, timeout: Duration) -> WaitOutcome {
    let ts: Option<libc::timespec> = if timeout == Duration::MAX {
        None
    } else {
        Some(libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        })
    };
    let ts_ptr = match ts.as_ref() {
        Some(ts) => ts as *const libc::timespec,
        None => std::ptr::null(),
    };

    // SAFETY: contract of [`wait`]; ptr is a live mapped 4-byte word.
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr.cast::<libc::c_void>(),
            libc::FUTEX_WAIT,
            expected as libc::c_int,
            ts_ptr,
            std::ptr::null::<libc::c_void>(),
            0,
        )
    };
    // `libc::syscall` is the libc wrapper, not the raw instruction: it
    // reports failure as -1 with the error in `errno` (both glibc and
    // musl), never as the raw `-errno` value the kernel uses internally.
    // A successful FUTEX_WAIT returns exactly 0, so -1 is unambiguous.
    if r == 0 {
        return WaitOutcome::Woken;
    }
    if r == -1 {
        return match std::io::Error::last_os_error().raw_os_error() {
            // EAGAIN: value already differed (race). EINTR: interrupted.
            // Both mean "re-check the word" rather than "timed out".
            Some(libc::EAGAIN) | Some(libc::EINTR) => WaitOutcome::Recheck,
            _ => WaitOutcome::TimedOut,
        };
    }
    WaitOutcome::TimedOut
}

#[cfg(target_os = "windows")]
unsafe fn wait_impl(ptr: *mut u8, expected: u32, timeout: Duration) -> WaitOutcome {
    unsafe extern "system" {
        #[link_name = "WaitOnAddress"]
        fn wait_on_address_(
            address: *mut libc::c_void,
            compare: *mut libc::c_void,
            size: libc::c_ulong,
            timeout_ms: u32,
        ) -> i32;
    }

    const INFINITE: u32 = u32::MAX;
    const ERROR_TIMEOUT: i32 = 0x5B4; // 1460

    let expected_word = expected.to_le_bytes();
    let effective_timeout = if timeout == Duration::MAX {
        INFINITE
    } else {
        timeout.as_millis().min(INFINITE as u128) as u32
    };

    // SAFETY: `WaitOnAddress` atomically compares the 4-byte word at `ptr`
    // against `expected_word` and parks. `expected_word` lives across the call.
    let ok = unsafe {
        wait_on_address_(
            ptr.cast::<libc::c_void>(),
            expected_word.as_ptr() as *mut libc::c_void,
            4,
            effective_timeout,
        )
    };
    // TRUE: woken by WakeByAddress*. FALSE with ERROR_TIMEOUT: timed out.
    // FALSE with any other error: spurious — re-check the word.
    if ok != 0 {
        return WaitOutcome::Woken;
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_TIMEOUT) {
        WaitOutcome::TimedOut
    } else {
        WaitOutcome::Recheck
    }
}

#[cfg(target_os = "freebsd")]
unsafe fn wait_impl(ptr: *mut u8, expected: u32, timeout: Duration) -> WaitOutcome {
    unsafe extern "C" {
        fn _umtx_op(
            obj: *mut libc::c_void,
            op: libc::c_int,
            val: libc::c_ulong,
            uaddr: *mut libc::c_void,
            uaddr2: *mut libc::c_void,
        ) -> libc::c_int;
    }

    // UMTX_OP_WAIT_UINT_PRIVATE atomically compares the 32-bit word at `obj`
    // against `val` and parks, with an absolute timeout in `uaddr2`.
    const UMTX_OP_WAIT_UINT_PRIVATE: libc::c_int = 15;

    let ts: Option<libc::timespec> = if timeout == Duration::MAX {
        None
    } else {
        // `_umtx_op` takes an *absolute* CLOCK_MONOTONIC deadline derived from
        // the caller's relative request.
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid out pointer; CLOCK_MONOTONIC is a valid clock id.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return WaitOutcome::TimedOut; // clock failure: treat as timed out
        }
        let deadline_ns = (now.tv_sec as i128)
            .saturating_mul(1_000_000_000)
            .saturating_add(now.tv_nsec as i128)
            .saturating_add((timeout.as_nanos() as i128).min(i128::MAX));
        Some(libc::timespec {
            tv_sec: (deadline_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (deadline_ns % 1_000_000_000) as libc::c_long,
        })
    };
    let ts_ptr = match ts.as_ref() {
        Some(ts) => ts as *const libc::timespec,
        None => std::ptr::null(),
    };

    // SAFETY: contract of [`wait`].
    let r = unsafe {
        _umtx_op(
            ptr.cast::<libc::c_void>(),
            UMTX_OP_WAIT_UINT_PRIVATE,
            expected as libc::c_ulong,
            std::ptr::null_mut(),
            ts_ptr as *mut libc::c_void,
        )
    };
    match r {
        0 => WaitOutcome::Woken,
        e if e == libc::ETIMEDOUT => WaitOutcome::TimedOut,
        e if e == libc::EINTR || e == libc::EAGAIN => WaitOutcome::Recheck,
        _ => WaitOutcome::TimedOut,
    }
}

#[cfg(target_os = "macos")]
unsafe fn wait_impl(ptr: *mut u8, expected: u32, timeout: Duration) -> WaitOutcome {
    // Retained against the day Darwin allows wait-word primitives.
    // Never reached: [`SUPPORTED`] is false here and the engine does not
    // emit `__ulock_wake` either, so the two sides can never be paired.
    //
    // Constants audited against XNU's bsd/sys/ulock.h: the operation word
    // is `opcode | flags` — `UL_COMPARE_AND_WAIT == 0x2` in the low byte,
    // `ULF_NO_ERRNO == 0x0100_0000`. With `ULF_NO_ERRNO` failures return
    // `-errno`: 0 = woken, `-EAGAIN` = value mismatch (race — caller
    // re-checks), `-EINTR` = spurious, `-ETIMEDOUT` = timeout.
    unsafe extern "C" {
        fn __ulock_wait(
            operation: u32,
            addr: *mut libc::c_void,
            value: u64,
            timeout_us: u32,
        ) -> i32;
    }
    const UL_COMPARE_AND_WAIT: u32 = 0x0000_0002;
    const ULF_NO_ERRNO: u32 = 0x0100_0000;
    let timeout_us = if timeout == Duration::MAX {
        u32::MAX
    } else {
        timeout.as_micros().min(u32::MAX as u128) as u32
    };
    // SAFETY: contract of [`wait`].
    let r = unsafe {
        __ulock_wait(
            ULF_NO_ERRNO | UL_COMPARE_AND_WAIT,
            ptr.cast::<libc::c_void>(),
            expected as u64,
            timeout_us,
        )
    };
    match r {
        0 => WaitOutcome::Woken,
        e if e == -libc::EAGAIN || e == -libc::EINTR => WaitOutcome::Recheck,
        _ => WaitOutcome::TimedOut,
    }
}

// Platforms with no wired primitive: `SUPPORTED` is false there, so these
// bodies are unreachable by construction (retained for completeness so the
// crate still type-checks on exotic targets).
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "macos"
)))]
unsafe fn wait_impl(_ptr: *mut u8, _expected: u32, _timeout: Duration) -> WaitOutcome {
    WaitOutcome::TimedOut
}

#[cfg(target_os = "linux")]
unsafe fn wake_impl(ptr: *mut u8, count: u32) -> u32 {
    // SAFETY: contract of [`wake`]. Keyed by inode+offset for shared
    // mappings, so it reaches waiters on other mappings of the same shm pages
    // (matching the engine's emission).
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr.cast::<libc::c_void>(),
            libc::FUTEX_WAKE,
            count as libc::c_int,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<libc::c_void>(),
            0,
        )
    };
    // Errors report as -1 (libc wrapper convention) and fall through to 0
    // wake attempts delivered.
    u32::try_from(r).unwrap_or(0)
}

#[cfg(target_os = "windows")]
unsafe fn wake_impl(ptr: *mut u8, _count: u32) -> u32 {
    unsafe extern "system" {
        #[link_name = "WakeByAddressAll"]
        fn wake_by_address_all_(address: *mut libc::c_void);
    }
    // SAFETY: contract of [`wake`]. `WakeByAddressAll` has no count argument;
    // every waiter re-checks the word, so waking all is a safe superset.
    unsafe {
        wake_by_address_all_(ptr.cast::<libc::c_void>());
    }
    // `WakeByAddressAll` returns nothing; report one wake attempt delivered.
    1
}

#[cfg(target_os = "freebsd")]
unsafe fn wake_impl(ptr: *mut u8, count: u32) -> u32 {
    unsafe extern "C" {
        fn _umtx_op(
            obj: *mut libc::c_void,
            op: libc::c_int,
            val: libc::c_ulong,
            uaddr: *mut libc::c_void,
            uaddr2: *mut libc::c_void,
        ) -> libc::c_int;
    }

    // sys/umtx.h: UMTX_OP_WAKE_PRIVATE wakes private-address waiters.
    const UMTX_OP_WAKE_PRIVATE: libc::c_int = 16;

    // SAFETY: contract of [`wake`].
    let r = unsafe {
        _umtx_op(
            ptr.cast::<libc::c_void>(),
            UMTX_OP_WAKE_PRIVATE,
            count as libc::c_ulong,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    u32::try_from(r).unwrap_or(0)
}

#[cfg(target_os = "macos")]
unsafe fn wake_impl(ptr: *mut u8, _count: u32) -> u32 {
    // Retained for the day Darwin allows wait-words on shared memory;
    // never reached because [`SUPPORTED`] is false there. Wakes use
    // `UL_COMPARE_AND_WAIT | ULF_WAKE_ALL`: the wake opcode must match the
    // waiters' opcode (XNU returns EDOM otherwise), and WAKE_ALL matches
    // the futex FUTEX_WAKE semantics the host backend expects.
    unsafe extern "C" {
        fn __ulock_wake(operation: u32, addr: *mut libc::c_void, wake_value: u64) -> i32;
    }
    const UL_COMPARE_AND_WAIT: u32 = 0x0000_0002;
    const ULF_WAKE_ALL: u32 = 0x0000_0100;
    const ULF_NO_ERRNO: u32 = 0x0100_0000;
    // SAFETY: contract of [`wake`].
    unsafe {
        let _ = __ulock_wake(
            ULF_NO_ERRNO | ULF_WAKE_ALL | UL_COMPARE_AND_WAIT,
            ptr.cast::<libc::c_void>(),
            0,
        );
    }
    1
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "macos"
)))]
unsafe fn wake_impl(_ptr: *mut u8, _count: u32) -> u32 {
    0
}

#[cfg(all(
    test,
    // Only platforms where the primitive demonstrably functions. macOS is
    // excluded: Darwin rejects `__ulock_wait` with junk errnos even on
    // private memory (see module docs).
    any(target_os = "linux", target_os = "windows", target_os = "freebsd")
))]
mod tests {
    use super::*;

    /// Mechanics check for each wired primitive on private memory: a parked
    /// waiter is released by `wake`.
    #[test]
    fn wait_word_roundtrip_on_private_memory() {
        let word = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let waiter = {
            let waiter_word = std::sync::Arc::clone(&word);
            std::thread::spawn(move || {
                // SAFETY: `waiter_word` keeps the u32 alive and 4-byte
                // aligned for the duration of the call.
                unsafe {
                    wait(
                        waiter_word.as_ref() as *const _ as *mut u8,
                        0,
                        Duration::from_millis(2_000),
                    )
                }
            })
        };

        std::thread::sleep(Duration::from_millis(50));
        word.store(1, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: `word` keeps the u32 alive and aligned for the duration of
        // the call.
        let _delivered = unsafe { wake(word.as_ref() as *const _ as *mut u8, 1) };

        let outcome = waiter.join().expect("waiter thread");
        assert!(
            matches!(outcome, WaitOutcome::Woken | WaitOutcome::Recheck),
            "waiter must report woken or recheck, got {outcome:?}"
        );
    }

    /// An unwoken wait reports a timeout.
    #[test]
    fn wait_word_times_out_without_wake() {
        let word: u32 = 0;
        // SAFETY: `word` is a live, 4-byte-aligned u32 for the duration of the
        // call.
        let outcome = unsafe { wait(&word as *const _ as *mut u8, 0, Duration::from_millis(25)) };
        assert_eq!(outcome, WaitOutcome::TimedOut);
    }

    /// A word that already differs returns immediately for a re-check rather
    /// than parking until the timeout.
    #[test]
    fn wait_word_rechecks_on_value_mismatch() {
        let word: u32 = 7;
        let start = std::time::Instant::now();
        // SAFETY: `word` is a live, 4-byte-aligned u32 for the duration of the
        // call.
        let outcome = unsafe {
            wait(
                &word as *const _ as *mut u8,
                0,
                Duration::from_millis(2_000),
            )
        };
        assert_eq!(outcome, WaitOutcome::Recheck);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "value mismatch must not park until the timeout"
        );
    }
}
