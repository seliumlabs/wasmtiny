//! Tests for the host-region-wait-notify change.
//!
//! Stage 1: public host wait/notify API on shared regions (guest notify
//! wakes host waiters, host notify wakes guest waiters, lost-wake-safe
//! register → re-check → wait idiom).
//! Stage 2: optional platform wake emission as a side effect of
//! `memory.atomic.notify` on shared ranges (return value unchanged,
//! per-OS conformance).

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use parking_lot::Mutex as ParkingMutex;
use wasmtiny::{
    RegionProt, SharedRegionId, WasmApplication, WasmValue,
    runtime::{HostWaitSupport, SharedMemoryRegistry, Store, WakeOutcome},
};

type Registry = Arc<ParkingMutex<SharedMemoryRegistry>>;

/// Guest module with a shared memory exposing notify/wait32 helpers that
/// take the guest address as a parameter.
const GUEST_WAT: &str = r#"
    (module
        (memory 1 1 shared)
        (export "memory" (memory 0))
        (func (export "notify") (param i32) (result i32)
            local.get 0
            i32.const 1       ;; count
            memory.atomic.notify)
        (func (export "wait32") (param i32) (result i32)
            local.get 0
            i32.const 0       ;; expected (zero-filled region)
            i64.const 2000000000  ;; 2s timeout backstop
            memory.atomic.wait32))
"#;
/// Byte offset of the test word within the region.
const OFFSET: usize = 64;
const PAGE_BYTES: u32 = 65536;

struct Fixture {
    app: Arc<Mutex<WasmApplication>>,
    store: Arc<Mutex<Store>>,
    region_id: SharedRegionId,
    /// Page offset where the region was mapped into the guest's memory.
    page_offset: u32,
}

impl Fixture {
    fn registry(&self) -> Registry {
        self.store.lock().unwrap().shared_memory_registry()
    }

    /// Guest address mapping `OFFSET` within the region.
    fn guest_addr(&self) -> i32 {
        (self.page_offset * PAGE_BYTES + OFFSET as u32) as i32
    }
}

/// Waiter cleanup: dropping the handle deregisters it, so a subsequent
/// notify finds no waiters and truthfully reports zero.
#[test]
fn dropped_waiter_is_deregistered() {
    let fx = setup();
    let registry = fx.registry();

    let waiter = registry
        .lock()
        .register_region_waiter(fx.region_id, OFFSET)
        .expect("register host waiter");
    drop(waiter);

    let woken = registry
        .lock()
        .notify_region(fx.region_id, OFFSET, 1)
        .expect("notify_region");
    assert_eq!(woken, 0, "registry must not retain dropped waiters");
}

#[cfg(all(
    feature = "platform-wake-emission",
    any(target_os = "linux", target_os = "freebsd")
))]
fn errno() -> i32 {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(target_os = "freebsd")]
    unsafe {
        *libc::__error()
    }
    #[cfg(target_os = "windows")]
    {
        // Windows reports errors differently; parking above does not use errno.
        0
    }
}

/// Task 4.5: guest notify wakes a host thread parked in the OS wait-word
/// primitive on the same pages (Stage 2 platforms, feature enabled).
///
/// Platform scope: Darwin kernels reject ALL wait-word primitives
/// (`__ulock_wait`, `os_sync_wait_on_address`, the restricted futex
/// syscall) with EFAULT/EPERM when the target word lives in MAP_SHARED
/// memory, so platform wake emission cannot function on macOS regardless of
/// engine configuration. CI therefore gates enablement on this test for
/// Linux/Windows/FreeBSD and must keep emission disabled on macOS.
#[cfg(all(
    feature = "platform-wake-emission",
    any(target_os = "linux", target_os = "windows", target_os = "freebsd")
))]
#[test]
fn guest_notify_wakes_host_os_waiter_conformance() {
    let fx = setup();
    let registry = fx.registry();

    // The capability query must reflect the compiled-in emission.
    assert_eq!(
        registry.lock().host_wait_support(),
        HostWaitSupport::RegistryAndOsWake
    );

    // Park the host thread on the exact address emission targets: the
    // region's host mapping base + offset.
    let base = registry.lock().get_region(fx.region_id).unwrap().ptr();
    // SAFETY: OFFSET is within the region's mapped length; the mapping
    // outlives the test (the registry is alive for its duration).
    let target = unsafe { base.add(OFFSET) } as usize;

    let parked = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300)); // let the guest start after us
        let target = target as *mut u8;
        // SAFETY: target points into the live shared-region mapping.
        unsafe { park_on_word(target) }
    });

    thread::sleep(Duration::from_millis(600)); // ensure the host is parked
    let addr = fx.guest_addr();
    let notified = spawn_guest_notify(fx.app.clone(), addr, Duration::ZERO)
        .join()
        .expect("notify thread");
    // No WASM waiter is parked here: the host thread rides the platform
    // primitive directly, so the instruction counts zero WASM waiters.
    // The OS wake is verified below via the parked thread's result.
    assert_eq!(notified, 0);

    let woken = parked.join().expect("parked host thread");
    assert_eq!(
        woken, true,
        "guest notify must wake a host thread parked via the platform primitive"
    );
}

/// Task 4.3: a guest parked in `memory.atomic.wait32` on a shared-range
/// address is woken by host-initiated `notify_region`.
#[test]
fn guest_wait_woken_by_host_notify_region() {
    let fx = setup();
    let registry = fx.registry();

    let addr = fx.guest_addr();
    let guest = spawn_guest_wait(fx.app.clone(), addr, Duration::from_millis(300));

    thread::sleep(Duration::from_millis(600));
    let woken = registry
        .lock()
        .notify_region(fx.region_id, OFFSET, 1)
        .expect("host notify_region");
    assert_eq!(woken, 1, "the parked guest waiter should be counted");

    let result = guest.join().expect("guest thread");
    assert_eq!(
        result, 0,
        "guest wait32 must complete as woken (0), not timed out (2)"
    );
}

/// Capability advertisement: the support level is a build-time constant
/// with no runtime toggle. It matches `os_wake::COMPILED_IN` exactly.
#[test]
fn host_wait_support_reflects_compiled_state() {
    let store = Store::new();
    let registry = store.shared_memory_registry();

    let expected = if wasmtiny::runtime::os_wake::COMPILED_IN {
        HostWaitSupport::RegistryAndOsWake
    } else {
        HostWaitSupport::RegistryOnly
    };
    assert_eq!(registry.lock().host_wait_support(), expected);

    // The level is identical across registries and stores — it is
    // process-wide and immutable.
    let other = Store::new();
    assert_eq!(
        other.shared_memory_registry().lock().host_wait_support(),
        expected
    );
}

/// Task 4.1: a host thread registered on `(region, offset)` wakes when a
/// guest executes `memory.atomic.notify` on the address mapping that offset.
#[test]
fn host_waiter_woken_by_guest_notify() {
    let fx = setup();
    let registry = fx.registry();

    let waiter = registry
        .lock()
        .register_region_waiter(fx.region_id, OFFSET)
        .expect("register host waiter");

    let addr = fx.guest_addr();
    let notifier = spawn_guest_notify(fx.app.clone(), addr, Duration::from_millis(300));

    let outcome = waiter
        .wait(Duration::from_secs(10))
        .expect("host wait should not error");
    assert_eq!(outcome, WakeOutcome::Woken, "host waiter must be woken");

    let notified = notifier.join().expect("notifier thread");
    assert_eq!(notified, 1);
}

/// Out-of-bounds offsets are rejected on registration and notification;
/// unknown regions error rather than panicking.
#[test]
fn invalid_registry_args_error() {
    let fx = setup();
    let registry = fx.registry();

    assert!(
        registry
            .lock()
            .register_region_waiter(fx.region_id, PAGE_BYTES as usize)
            .is_err(),
        "offset at end of region must be rejected"
    );
    assert!(
        registry
            .lock()
            .notify_region(fx.region_id, PAGE_BYTES as usize, 1)
            .is_err(),
        "notify offset at end of region must be rejected"
    );

    let bogus = wasmtiny::SharedRegionId::from_raw(u64::MAX);
    assert!(
        registry.lock().notify_region(bogus, 0, 1).is_err(),
        "unknown region must error"
    );
}

/// Task 4.4: with platform wake emission compiled in, the notify
/// instruction's return value is unchanged — it counts only WASM waiters,
/// never host waiters or OS-side wakes.
#[cfg(feature = "platform-wake-emission")]
#[test]
fn notify_return_value_unaffected_by_emission() {
    fn run() -> i32 {
        let fx = setup();
        let registry = fx.registry();

        // A registered host waiter shares the registry entry with the guest
        // waiter below; neither may affect the instruction's return value.
        let _host_waiter = registry
            .lock()
            .register_region_waiter(fx.region_id, OFFSET)
            .expect("register host waiter");

        let addr = fx.guest_addr();
        // Park a genuine guest waiter before the notify fires.
        let guest = spawn_guest_wait(fx.app.clone(), addr, Duration::from_millis(200));
        thread::sleep(Duration::from_millis(500));
        let notified = spawn_guest_notify(fx.app.clone(), addr, Duration::ZERO)
            .join()
            .expect("notify thread");

        // The guest waiter is woken by the same notify; either way it exits.
        let _wait_result = guest.join().expect("guest wait thread");
        notified
    }

    let result = run();
    assert_eq!(result, 1, "one WASM waiter => return 1");
}

#[cfg(all(feature = "platform-wake-emission", target_os = "linux"))]
unsafe fn park_on_word(ptr: *mut u8) -> bool {
    unsafe {
        let ts = libc::timespec {
            tv_sec: 15,
            tv_nsec: 0,
        };
        loop {
            let r = libc::syscall(
                libc::SYS_futex,
                ptr.cast::<libc::c_void>(),
                libc::FUTEX_WAIT,
                0u32, // expected word value (zero-filled region)
                &ts,
                std::ptr::null::<libc::c_void>(),
                0usize,
            );
            if r == 0 {
                return true; // woken
            }
            match errno() {
                libc::EINTR => continue,
                _ => return false, // EAGAIN / ETIMEDOUT / unexpected
            }
        }
    }
}

#[cfg(all(feature = "platform-wake-emission", target_os = "windows"))]
unsafe fn park_on_word(ptr: *mut u8) -> bool {
    unsafe extern "system" {
        fn WaitOnAddress(
            addr: *mut libc::c_void,
            compare: *mut libc::c_void,
            size: usize,
            ms: u32,
        ) -> i32;
    }
    unsafe {
        let mut expected = 0i32;
        WaitOnAddress(
            ptr.cast::<libc::c_void>(),
            (&mut expected) as *mut i32 as *mut libc::c_void,
            4,
            15_000,
        ) != 0
    }
}

#[cfg(all(feature = "platform-wake-emission", target_os = "freebsd"))]
unsafe fn park_on_word(ptr: *mut u8) -> bool {
    unsafe extern "C" {
        fn _umtx_op(
            obj: *mut libc::c_void,
            op: libc::c_int,
            val: libc::c_ulong,
            uaddr: *mut libc::c_void,
            uaddr2: *mut libc::c_void,
        ) -> libc::c_int;
    }
    // sys/umtx.h: UMTX_OP_WAIT_UINT_PRIVATE waits for *(u32*)obj != val.
    const UMTX_OP_WAIT_UINT_PRIVATE: libc::c_int = 16;
    const CLOCK_MONOTONIC: u32 = 4;
    #[repr(C)]
    struct umtx_time {
        timeout: libc::timespec,
        clockid: u32,
        flags: u32,
    }
    unsafe {
        let ut = umtx_time {
            timeout: libc::timespec {
                tv_sec: 15,
                tv_nsec: 0,
            },
            clockid: CLOCK_MONOTONIC,
            flags: 0, // relative timeout
        };
        loop {
            let r = _umtx_op(
                ptr.cast::<libc::c_void>(),
                UMTX_OP_WAIT_UINT_PRIVATE,
                0, // expected value
                std::ptr::null_mut(),
                (&ut as *const umtx_time) as *mut libc::c_void,
            );
            if r == 0 {
                return true;
            }
            match errno() {
                libc::EINTR | libc::EAGAIN => continue,
                _ => return false,
            }
        }
    }
}

/// Task 4.2: the register → re-check → wait idiom — a notify landing after
/// registration but before `wait()` is latched and must wake the waiter
/// immediately instead of sleeping until the timeout.
#[test]
fn register_recheck_wait_idiom_does_not_lose_wakes() {
    let fx = setup();
    let registry = fx.registry();

    for _ in 0..500 {
        // 1. Register BEFORE re-checking the shared word.
        let waiter = registry
            .lock()
            .register_region_waiter(fx.region_id, OFFSET)
            .expect("register host waiter");

        // (The embedder would re-check the shared word here.)

        // 2. Notify arrives between registration/re-check and wait().
        let woken_count = registry
            .lock()
            .notify_region(fx.region_id, OFFSET, 1)
            .expect("notify_region");
        assert_eq!(woken_count, 1);

        // 3. Wait observes the latched notify immediately instead of
        //    sleeping out the timeout.
        let outcome = waiter
            .wait(Duration::from_millis(50))
            .expect("wait should not error");
        assert_eq!(
            outcome,
            WakeOutcome::Woken,
            "latched notify must not be lost"
        );
    }
}

fn setup() -> Fixture {
    let store = Arc::new(Mutex::new(Store::new()));
    let mut app = WasmApplication::with_store(store.clone());
    let module_bytes = wat::parse_str(GUEST_WAT).expect("compile wat");
    let module_idx = app
        .load_module_from_memory(&module_bytes)
        .expect("load module");
    app.instantiate(module_idx).expect("instantiate");
    // Attach a one-page read/write region to the module's memory.
    let (region_id, page_offset) = app
        .allocate_shared_region(module_idx, PAGE_BYTES, RegionProt::ReadWrite)
        .expect("allocate shared region");
    Fixture {
        app: Arc::new(Mutex::new(app)),
        store,
        region_id,
        page_offset,
    }
}

/// Runs the guest `notify(count=1)` export at `addr` on its own thread after
/// `delay`, returning the instruction's i32 result.
fn spawn_guest_notify(
    app: Arc<Mutex<WasmApplication>>,
    addr: i32,
    delay: Duration,
) -> thread::JoinHandle<i32> {
    thread::spawn(move || {
        thread::sleep(delay);
        let results = app
            .lock()
            .unwrap()
            .call_function(0, "notify", &[WasmValue::I32(addr)])
            .expect("guest notify should succeed");
        match results.first() {
            Some(WasmValue::I32(v)) => *v,
            other => panic!("unexpected notify result: {other:?}"),
        }
    })
}

/// Runs the guest `wait32(expected=0)` export at `addr` on its own thread
/// after `delay`; returns the instruction result (0 woken / 1 not-equal /
/// 2 timed out).
fn spawn_guest_wait(
    app: Arc<Mutex<WasmApplication>>,
    addr: i32,
    delay: Duration,
) -> thread::JoinHandle<i32> {
    thread::spawn(move || {
        thread::sleep(delay);
        let results = app
            .lock()
            .unwrap()
            .call_function(0, "wait32", &[WasmValue::I32(addr)])
            .expect("guest wait should succeed");
        match results.first() {
            Some(WasmValue::I32(v)) => *v,
            other => panic!("unexpected wait result: {other:?}"),
        }
    })
}
