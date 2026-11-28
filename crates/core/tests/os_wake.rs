//! Cross-thread park/wake conformance on an `mmap`'d shared page.
//!
//! Stage 2 host waits park directly on the shared region's host mapping, not
//! on the engine's waiter registry. This test proves the paired primitive on
//! that exact kind of page: one thread parks in [`os_wake::wait`], another
//! stores the new word value and calls [`os_wake::wake`].
//!
//! Skipped where [`os_wake::SUPPORTED`] is false (macOS, by design: Darwin
//! rejects wait-word primitives on `MAP_SHARED` memory).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use wasmtiny::runtime::SharedMemoryRegistry;
use wasmtiny::runtime::os_wake::{self, WaitOutcome};

#[test]
fn paired_park_wake_on_shared_page() {
    if !os_wake::SUPPORTED {
        return;
    }

    let mut registry = SharedMemoryRegistry::default();
    let region_id = registry
        .allocate_region_standalone(64)
        .expect("allocate shared region");
    let region = registry.get_region(region_id).expect("region present");
    let ptr = region.ptr();

    // SAFETY: `ptr` is the region's live mapping, large enough for the word,
    // and stays alive for the whole test via `registry`.
    let word = unsafe { &*ptr.cast::<AtomicU32>() };
    word.store(0, Ordering::SeqCst);

    let address = ptr as usize;
    let waiter = std::thread::spawn(move || {
        let ptr = address as *mut u8;
        // SAFETY: the mapping is alive until the registry drops after join.
        unsafe { os_wake::wait(ptr, 0, Duration::from_secs(2)) }
    });

    // Give the waiter time to park, then change the word and wake it.
    std::thread::sleep(Duration::from_millis(50));
    word.store(1, Ordering::SeqCst);
    // SAFETY: same live mapping as above.
    let _delivered = unsafe { os_wake::wake(ptr, 1) };

    let outcome = waiter.join().expect("waiter thread");
    assert!(
        matches!(outcome, WaitOutcome::Woken | WaitOutcome::Recheck),
        "waiter must report woken or recheck, got {outcome:?}"
    );
}
