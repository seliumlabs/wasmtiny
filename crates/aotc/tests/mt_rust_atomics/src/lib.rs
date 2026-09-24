//! Real Rust module exercising wasm atomics: futex-backed std locks,
//! atomic wait/notify on a shared wake word, and a worker-pool park/wake
//! pattern. Built with `-Zbuild-std` + `+atomics` (see README in the
//! checked-in location for exact flags).

#![feature(stdarch_wasm_atomic_wait)]

use std::{
    sync::{Condvar, Mutex},
    sync::atomic::{AtomicU32, Ordering},
};

/// Condvar used for the wait/notify storm.
static CV: Condvar = Condvar::new();
static CV_COUNT: Mutex<u32> = Mutex::new(0);
/// Futex-backed std mutex shared by all workers.
static LOCK: Mutex<u64> = Mutex::new(0);
/// Number of workers currently parked on the wake word.
static PARKED: AtomicU32 = AtomicU32::new(0);
/// RMW counter (memory.atomic.rmw.add via AtomicU32::fetch_add).
static RMW: AtomicU32 = AtomicU32::new(0);
/// Shared wake word the guest worker pool parks on.
static WAKE: AtomicU32 = AtomicU32::new(0);

/// Bumps CV_COUNT and notifies one waiter.
#[no_mangle]
pub extern "C" fn cv_bump() -> u32 {
    let mut count = CV_COUNT.lock().unwrap();
    *count += 1;
    let c = *count;
    CV.notify_one();
    c
}

/// Wait/notify storm via std Condvar (futex-backed on wasm+atomics):
/// waits until CV_COUNT reaches `target`, re-checking at most `timeout_ns`
/// per wait. Returns the observed count so a regression (lost wake) fails
/// fast instead of hanging the suite.
#[no_mangle]
pub extern "C" fn cv_wait(target: u32, timeout_ns: u64) -> u32 {
    let bound = std::time::Duration::from_nanos(timeout_ns);
    let mut count = CV_COUNT.lock().unwrap();
    while *count < target {
        let (guard, _timeout) = CV.wait_timeout(count, bound).unwrap();
        count = guard;
    }
    *count
}

/// Concurrent memory growth helper. Returns the old size in pages or
/// `usize::MAX` (-1) on failure, per `memory.grow` semantics.
#[no_mangle]
pub extern "C" fn grow(delta: u32) -> u32 {
    core::arch::wasm32::memory_grow(0, delta as usize) as u32
}

/// Lock contention: each caller takes the futex-backed std mutex `iters`
/// times, accumulating into a shared u64.
#[no_mangle]
pub extern "C" fn lock_storm(iters: u32) -> u64 {
    let mut total = 0u64;
    for _ in 0..iters {
        let mut guard = LOCK.lock().unwrap();
        *guard += 1;
        total += *guard;
    }
    total
}

/// Read the std-lock accumulator.
#[no_mangle]
pub extern "C" fn lock_value() -> u64 {
    *LOCK.lock().unwrap()
}

/// How many workers are currently parked.
#[no_mangle]
pub extern "C" fn parked_count() -> u32 {
    PARKED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn rmw_bump() -> u32 {
    RMW.fetch_add(1, Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn rmw_read() -> u32 {
    RMW.load(Ordering::SeqCst)
}

/// Park on the wake word: return immediately (0) if the word is already set,
/// otherwise block until woken (0) or the timeout in nanoseconds elapses
/// (2). A negative timeout blocks forever.
///
/// Mirrors the worker-pool park: N workers park on ONE shared wake word and
/// a dispatcher's `worker_wake(N)` must release up to N distinct waiters.
#[no_mangle]
pub extern "C" fn worker_park(timeout_ns: i64) -> u32 {
    PARKED.fetch_add(1, Ordering::SeqCst);
    // Futex park: re-check the word, then wait on it with expected == 0. A
    // notify arriving between the re-check and the wait makes the wait
    // return immediately with status 1 (not-equal) and we report woken.
    loop {
        if WAKE.load(Ordering::SeqCst) != 0 {
            PARKED.fetch_sub(1, Ordering::SeqCst);
            return 0;
        }
        let status = unsafe {
            core::arch::wasm32::memory_atomic_wait32(
                &WAKE as *const AtomicU32 as *mut i32,
                0,
                timeout_ns,
            )
        };
        match status {
            0 => {
                // Woken by a notify.
                PARKED.fetch_sub(1, Ordering::SeqCst);
                return 0;
            }
            1 => {
                // The word changed between re-check and wait: someone woke us.
                PARKED.fetch_sub(1, Ordering::SeqCst);
                return 0;
            }
            _ => {
                // Timed out: a worker never gives up, it re-parks (the real
                // pool loops here). For the engine stress tests we report the
                // timeout so a regression fails fast instead of hanging.
                PARKED.fetch_sub(1, Ordering::SeqCst);
                return 2;
            }
        }
    }
}

/// Reset the wake word (after all workers have been woken).
#[no_mangle]
pub extern "C" fn worker_reset() {
    WAKE.store(0, Ordering::SeqCst);
    PARKED.store(0, Ordering::SeqCst);
}

/// Wake up to `n` parked workers on the wake word.
#[no_mangle]
pub extern "C" fn worker_wake(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    // The classic futex wake: bump the word, then notify. All sleeping
    // waiters re-check the word; the engine's registry must release up to
    // `n` distinct waiters per the threads proposal.
    WAKE.store(1, Ordering::SeqCst);
    unsafe { core::arch::wasm32::memory_atomic_notify(&WAKE as *const AtomicU32 as *mut i32, n) }
}