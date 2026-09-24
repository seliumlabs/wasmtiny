//! Shared-region attach/detach on the AOT instance (spike finding 4).
//!
//! `AotInstance` previously had no `SharedMemoryRegistry`: the interpreter's
//! `Instance` could allocate/attach/detach shared regions and host waiters
//! interoperated with guest wait/notify, but an AOT instance could not.
//! These tests pin the AOT surface (`allocate_shared_region`,
//! `attach_shared_region`, `detach_shared_region`, `shared_memory_registry`)
//! and the cross-instance visibility of attached regions.

use std::sync::Arc;

use parking_lot::Mutex as ParkingMutex;
use wasmtiny::{
    RegionProt, SharedRegionId,
    aot::{AotInstance, AotLoader, AotModule},
    runtime::{SharedMemoryRegistry, TrapCode, WakeOutcome, WasmError, WasmValue},
};
use wasmtiny_aotc::{CompilerConfig, compile_artifact};

/// Guest module with a shared memory exposing wait32/notify and load/store
/// helpers that take the guest address as a parameter. `wait32` bumps an
/// **atomic** guest-visible parked counter (owned address 4) on entry and
/// clears it on return, so tests synchronise deterministically on
/// registration without lost updates (a worker that bumped the counter but
/// had not yet registered its waiter when a notify fired simply parks and a
/// later drain notify releases it).
const GUEST: &str = r#"(module
    (memory 1 1 shared)
    (export "memory" (memory 0))
    (func (export "notify") (param i32) (result i32)
        (local.get 0) (i32.const 1) (memory.atomic.notify))
    (func (export "wait32") (param i32) (result i32)
        (local $r i32)
        (i32.atomic.rmw.add (i32.const 4) (i32.const 1))
        (drop)
        (local.get 0) (i32.const 0) (i64.const 3000000000) (memory.atomic.wait32)
        (local.set $r)
        (i32.atomic.rmw.add (i32.const 4) (i32.const -1))
        (drop)
        (local.get $r))
    (func (export "parked_count") (result i32)
        (i32.atomic.load (i32.const 4)))
    (func (export "store") (param i32 i32)
        (i32.store (local.get 0) (local.get 1)))
    (func (export "load") (param i32) (result i32)
        (i32.load (local.get 0))))"#;
/// Byte offset of the test word within the region.
const OFFSET: u32 = 64;
const PAGE_BYTES: u32 = 65536;

struct Fixture {
    instance: Arc<AotInstance>,
    registry: Arc<ParkingMutex<SharedMemoryRegistry>>,
    region_id: SharedRegionId,
    page_offset: u32,
}

impl Fixture {
    /// The guest address mapping `OFFSET` within the region.
    fn guest_addr(&self) -> i32 {
        (self.page_offset * PAGE_BYTES + OFFSET) as i32
    }

    fn invoke(&self, name: &str, args: &[WasmValue]) -> Vec<WasmValue> {
        let index = self.instance.export_func_index(name).expect("export");
        self.instance
            .invoke_shared(index, args)
            .expect("invocation succeeds")
    }
}

/// Finding 4: detach unmaps the region (guest access traps `MemoryOutOfBounds`)
/// and the registry then allows `destroy_region`.
#[test]
fn aot_shared_region_detach_unmaps_and_destroy_works() {
    let fx = setup();
    let addr = fx.guest_addr();

    // Sanity: the region is accessible before detach.
    fx.invoke("store", &[WasmValue::I32(addr), WasmValue::I32(7)]);
    assert_eq!(
        fx.invoke("load", &[WasmValue::I32(addr)]),
        vec![WasmValue::I32(7)]
    );

    // Detach: the guest's mapping is restored to PROT_NONE, so guest access
    // to the region's address traps.
    fx.instance
        .detach_shared_region(fx.region_id)
        .expect("detach succeeds");
    let result = fx.instance.invoke_shared(
        fx.instance.export_func_index("load").expect("load"),
        &[WasmValue::I32(addr)],
    );
    assert!(
        matches!(result, Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))),
        "guest access to a detached region must trap, got {result:?}"
    );

    // With zero attachments the registry can destroy the region.
    fx.registry
        .lock()
        .destroy_region(fx.region_id)
        .expect("destroy after detach succeeds");
}

/// Finding 4: the AOT instance can allocate and attach a shared region, the
/// registry is shared with the store, and guest wait/notify on the attached
/// range interoperates with host waiters and `notify_region`.
#[test]
fn aot_shared_region_guest_wait_notify_and_host_interop() {
    let fx = setup();

    // Guest address of the region's test word; the region was allocated at
    // the top of the guest VA range (page 0 for this 1-page module, see the
    // interpreter fixture), so the word is readable/writable.
    let addr = fx.guest_addr();

    // A host waiter registered on (region, OFFSET) is woken by guest notify.
    let host_waiter = fx
        .registry
        .lock()
        .register_region_waiter(fx.region_id, OFFSET as usize)
        .expect("register host waiter");
    let notified = fx.invoke("notify", &[WasmValue::I32(addr)]);
    assert_eq!(
        notified,
        vec![WasmValue::I32(1)],
        "guest notify counts the host waiter"
    );
    assert_eq!(
        host_waiter
            .wait(std::time::Duration::from_secs(5))
            .expect("wait"),
        WakeOutcome::Woken,
        "guest notify must wake the host waiter on the attached region"
    );

    // A guest waiter parked on the region is woken by host notify_region.
    let guest = {
        let instance = fx.instance.clone();
        std::thread::spawn(move || {
            instance.invoke_shared(
                instance.export_func_index("wait32").expect("wait32"),
                &[WasmValue::I32(addr)],
            )
        })
    };
    // Counter-based readiness, then drain notify_region until the waiter has
    // returned. A notify that raced the waiter's registration is covered by
    // a later iteration, so the test is deterministic.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fx.invoke("parked_count", &[])[0].i32().unwrap() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "guest waiter never entered wait32"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut woken = 0u32;
    while fx.invoke("parked_count", &[])[0].i32().unwrap() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "host notify_region never released the guest waiter"
        );
        woken += fx
            .registry
            .lock()
            .notify_region(fx.region_id, OFFSET as usize, 1)
            .expect("host notify_region");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        woken >= 1,
        "host notify_region must wake the parked guest waiter"
    );
    let result = guest.join().expect("guest waiter thread survives");
    assert_eq!(
        result,
        Ok(vec![WasmValue::I32(0)]),
        "guest wait32 on the region must return woken (0)"
    );
}

fn instantiate_guest() -> Arc<AotInstance> {
    Arc::new(AotInstance::new(&load(GUEST)).expect("instantiation succeeds"))
}

fn load(source: &str) -> AotModule {
    let wasm = wat::parse_str(source).expect("wat parses");
    let bytes = compile_artifact(&wasm, &CompilerConfig::host()).expect("compilation succeeds");
    AotLoader::new().load(&bytes).expect("artifact loads")
}

/// A fresh fixture: instance + one attached read/write region.
fn setup() -> Fixture {
    let instance = instantiate_guest();
    let registry = instance.shared_memory_registry();
    let (region_id, page_offset) = instance
        .allocate_shared_region(PAGE_BYTES, RegionProt::ReadWrite)
        .expect("allocate and attach shared region");
    Fixture {
        instance,
        registry,
        region_id,
        page_offset,
    }
}

/// Finding 4 + spike finding 1: N *concurrently invoked* guest threads park
/// on ONE shared-region word; `notify_region(N)` releases all N — the
/// 2-worker hang repro on a shared range. The AOT path is used here because
/// `invoke_shared` gives real concurrent invocations (the interpreter
/// fixture serialises on its application lock).
///
/// Synchronisation is counter-based (the guest parked counter, drained by
/// repeated `notify_region`) rather than a fixed sleep, so a worker whose
/// registration raced a notify is released by a later iteration and the test
/// is deterministic; the joins assert every waiter was woken, none timed out.
#[test]
fn shared_region_multi_waiter_notify_releases_all() {
    let fx = setup();
    let registry = fx.registry.clone();
    let addr = fx.guest_addr();

    for workers in [2usize, 4] {
        let waits: Vec<_> = (0..workers)
            .map(|_| {
                let instance = fx.instance.clone();
                std::thread::spawn(move || {
                    let index = instance.export_func_index("wait32").expect("wait32");
                    instance.invoke_shared(index, &[WasmValue::I32(addr)])
                })
            })
            .collect();

        // Wait until every waiter has entered wait32.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while fx.invoke("parked_count", &[])[0].i32().unwrap() < workers as i32 {
            assert!(
                std::time::Instant::now() < deadline,
                "{workers} workers: never all parked"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Drain notify_region until no waiter remains parked.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut woken = 0u32;
        while fx.invoke("parked_count", &[])[0].i32().unwrap() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "{workers} workers: never all woken"
            );
            woken += registry
                .lock()
                .notify_region(fx.region_id, OFFSET as usize, workers as u32)
                .expect("notify_region(N)");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            woken >= workers as u32,
            "{workers} workers: notify_region must release {workers} distinct waiters (woken {woken})"
        );

        for wait in waits {
            let result = wait.join().expect("guest waiter thread survives");
            assert_eq!(
                result,
                Ok(vec![WasmValue::I32(0)]),
                "{workers} workers: every guest waiter must be woken (0), none may time out (2)"
            );
        }
    }
}

/// Finding 4: two AOT instances sharing one registry observe each other's
/// writes through their own guest mappings of the same attached region.
#[test]
fn two_aot_instances_share_attached_region_bytes() {
    let shared_store = wasmtiny::aot::AotStore::shared();
    let registry = Arc::new(ParkingMutex::new(SharedMemoryRegistry::default()));
    let a = Arc::new(
        AotInstance::instantiate_with_registry(&shared_store, &load(GUEST), &[], registry.clone())
            .expect("A instantiates"),
    );
    let b = Arc::new(
        AotInstance::instantiate_with_registry(&shared_store, &load(GUEST), &[], registry.clone())
            .expect("B instantiates"),
    );

    let (region_id, _page_a) = a
        .allocate_shared_region(PAGE_BYTES, RegionProt::ReadWrite)
        .expect("A allocates and attaches");
    let page_b = b
        .attach_shared_region(region_id, RegionProt::ReadWrite, None)
        .expect("B attaches the same region");
    let page_a = a
        .memory_handle(0)
        .expect("A memory")
        .lock()
        .expect("lock")
        .shared_ranges()
        .iter()
        .find(|r| r.region_id == region_id)
        .map(|r| r.page_offset)
        .expect("A has the region mapped");

    // A writes through its guest mapping; B reads through its own.
    let word = |page: u32| (page * PAGE_BYTES + OFFSET) as i32;
    let invoke = |instance: &AotInstance, name: &str, args: &[WasmValue]| {
        let index = instance.export_func_index(name).expect("export");
        instance
            .invoke_shared(index, args)
            .expect("invocation succeeds")
    };
    invoke(
        &a,
        "store",
        &[
            WasmValue::I32(word(page_a)),
            WasmValue::I32(0xABCD1234u32 as i32),
        ],
    );
    let read = invoke(&b, "load", &[WasmValue::I32(word(page_b))]);
    assert_eq!(
        read,
        vec![WasmValue::I32(0xABCD1234u32 as i32)],
        "writes through instance A must be visible through instance B's mapping"
    );
}
