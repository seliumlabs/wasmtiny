use std::{
    collections::HashMap,
    ffi::CString,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex as ParkingMutex, RwLock};

use crate::{
    memory::{PAGE_SIZE_BYTES, RegionProt},
    runtime::{Memory, Result, WasmError, os_wake},
};

/// Shared waiter map for a region: byte offset within the region -> waiter.
pub(crate) type WaiterMap = Arc<RwLock<HashMap<u32, Arc<SharedWaiter>>>>;

/// Maximum shared region size (1 GiB).
const MAX_REGION_SIZE: u32 = 1 << 30;
/// Monotonic counter for generating unique shm names.
static NEXT_SHM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Shared region id.
pub struct SharedRegionId(u64);

/// An mmap-backed shared memory region.
///
/// Shared regions are backed by `shm_open` to obtain a file descriptor, then
/// mapped with `mmap(MAP_SHARED)`. The same fd is used with `mmap(MAP_FIXED |
/// MAP_SHARED)` to map the identical physical pages into multiple guest address
/// spaces, achieving true cross-instance visibility without software copies.
pub struct SharedRegion {
    /// Base pointer of the creator's mmap of the region.
    ptr: *mut u8,
    /// Length in bytes (page-aligned).
    len: usize,
    /// File descriptor from shm_open; kept alive so guests can MAP_FIXED the
    /// same pages. Closed on Drop.
    fd: i32,
    /// Number of guest instances that currently have this region mapped.
    attachment_count: AtomicUsize,
    /// Shared waiters for atomic wait/notify on addresses within this region.
    /// Keyed by byte offset within the region (not guest address).
    waiters: WaiterMap,
}

/// Outcome of a host-side [`RegionWaiter::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// A notify arrived for this waiter before or during the wait.
    Woken,
    /// The timeout elapsed without a notify.
    TimedOut,
}

/// A registered host waiter on a `(region_id, offset)` pair.
///
/// Registration is explicit and separable from waiting so embedders can
/// implement the **register → re-check → wait** idiom without losing
/// wakeups:
///
/// ```ignore
/// // 1. Register BEFORE reading the shared word.
/// let waiter = registry.register_region_waiter(region_id, offset)?;
/// // 2. Re-check the shared word through your own mapping.
/// if ring_is_empty() { return Ok(()); } // no need to sleep
/// // 3. Only then block. A notify that landed between steps 1 and 3 is
/// //    latched in the waiter's notified flag, so this returns Woken
/// //    immediately instead of sleeping.
/// match waiter.wait(Duration::from_secs(1))? {
///     WakeOutcome::Woken => { /* re-check and make progress */ }
///     WakeOutcome::TimedOut => { /* backstop; retry the loop */ }
/// }
/// ```
///
/// The waiter occupies an entry in the region's waiter registry — the same
/// registry guest `memory.atomic.wait32`/`memory.atomic.notify` use on
/// shared ranges, so guest notifies wake host waiters and vice versa.
///
/// Handles are cheap to create ("register cheap, wait often"); dropping
/// the last handle for an offset deregisters it from the registry, so no
/// stale entries are retained. Keep a bounded timeout as a backstop.
pub struct RegionWaiter {
    map: WaiterMap,
    offset: u32,
    inner: Arc<SharedWaiter>,
}

/// A waiter for atomic wait/notify on shared memory addresses.
#[derive(Debug)]
pub(crate) struct SharedWaiter {
    pub(crate) notified: ParkingMutex<bool>,
    pub(crate) condvar: Condvar,
}

/// Shared memory registry.
///
/// Manages the lifecycle of shared memory regions: creation, destruction,
/// attachment to guest instances, and detachment. Regions are mapped directly
/// into guest linear memory via `mmap(MAP_FIXED | MAP_SHARED)` using a shared
/// file descriptor, so writes in one guest are visible to all others without
/// any software copy path.
///
/// The registry provides **no** public byte-level read or write methods; all
/// data access goes through the guest's native load/store instructions on the
/// mapped pages. Host-side convenience accessors are `pub(crate)` only.
pub struct SharedMemoryRegistry {
    next_region_id: u64,
    regions: HashMap<SharedRegionId, Arc<SharedRegion>>,
}

/// The engine's host-wait support level for shared regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWaitSupport {
    /// Only the in-process registry is available: host waiters registered
    /// via [`SharedMemoryRegistry::register_region_waiter`] are woken by
    /// guest/host notifies going through the registry.
    RegistryOnly,
    /// Registry support plus platform wake emission: a guest
    /// `memory.atomic.notify` on a shared range additionally emits the host
    /// platform's wake primitive on the region's host mapping address.
    /// Reported only when emission is compiled in (build-time; there is no
    /// runtime toggle).
    RegistryAndOsWake,
}

impl SharedRegionId {
    /// Constant `fn`.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Constant `fn`.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl SharedRegion {
    /// Creates a new shared region backed by `shm_open` + `mmap(MAP_SHARED)`.
    ///
    /// The shared memory object is unlinked immediately after creation so it
    /// does not persist in the filesystem namespace; the fd keeps it alive
    /// until all mappings are released and the fd is closed.
    fn new(size: u32) -> Result<Self> {
        if size == 0 {
            return Err(WasmError::Runtime(
                "shared region size must be greater than zero".to_string(),
            ));
        }
        if size > MAX_REGION_SIZE {
            return Err(WasmError::Runtime(format!(
                "shared region size {} exceeds maximum {}",
                size, MAX_REGION_SIZE
            )));
        }

        let len = size as usize;

        // Generate a unique name incorporating PID and entropy for
        // cross-process uniqueness. POSIX shm names are limited in length
        // (e.g. 31 chars on macOS), so use a short prefix + hash.
        let id = NEXT_SHM_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let entropy = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!("/wt_{}_{}_{}", pid, id, entropy);
        let c_name = CString::new(name.as_bytes())
            .map_err(|_| WasmError::Runtime("failed to create shm name".to_string()))?;

        // Create the shared memory object.
        // SAFETY: c_name is a valid C string. O_CREAT | O_RDWR with mode 0600.
        let fd = unsafe {
            libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err(WasmError::Runtime(format!(
                "shm_open failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Unlink immediately — the fd keeps the object alive.
        // SAFETY: c_name is still valid.
        unsafe {
            libc::shm_unlink(c_name.as_ptr());
        }

        // Set the size of the shared memory object.
        // SAFETY: fd is a valid open file descriptor.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(WasmError::Runtime(format!(
                "ftruncate failed for shared region: {}",
                err
            )));
        }

        // Map the shared memory object into the creator's address space.
        // SAFETY: fd is valid and the object is at least `len` bytes.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(WasmError::Runtime(format!(
                "mmap failed for shared region: {}",
                err
            )));
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            len,
            fd,
            attachment_count: AtomicUsize::new(0),
            waiters: WaiterMap::new(RwLock::new(HashMap::new())),
        })
    }

    /// Returns the base pointer of the creator's mapping.
    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Returns the file descriptor backing this shared region.
    ///
    /// Used by `Memory::map_shared_region` to `mmap(MAP_FIXED | MAP_SHARED)`
    /// the same physical pages into a guest's address space.
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Returns a reference to the shared waiters Arc.
    ///
    /// Used by `Memory::map_shared_region` to share waiters across instances.
    pub(crate) fn waiters_arc(&self) -> WaiterMap {
        self.waiters.clone()
    }

    /// Returns the length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the region is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the current attachment count.
    pub fn attachment_count(&self) -> usize {
        self.attachment_count.load(Ordering::SeqCst)
    }

    /// Increments the attachment count.
    fn attach(&self) {
        self.attachment_count.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrements the attachment count.
    fn detach(&self) {
        self.attachment_count.fetch_sub(1, Ordering::SeqCst);
    }
}

// SAFETY: The fd and ptr are process-wide resources. The fd is a plain integer
// and the ptr points to a shared mapping that the kernel serialises. All
// mutation of attachment_count is atomic.
unsafe impl Send for SharedRegion {}

unsafe impl Sync for SharedRegion {}

impl std::fmt::Debug for SharedRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRegion")
            .field("len", &self.len)
            .field("fd", &self.fd)
            .field(
                "attachment_count",
                &self.attachment_count.load(Ordering::SeqCst),
            )
            .finish()
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        // Unmap the creator's mapping first.
        if !self.ptr.is_null() && self.len > 0 {
            // SAFETY: ptr was allocated via mmap with self.len bytes.
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
        // Close the fd. The kernel frees the shared memory object once all
        // mappings (including those in guest address spaces) are released.
        if self.fd >= 0 {
            // SAFETY: fd was obtained from shm_open and has not been closed.
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

impl RegionWaiter {
    /// Blocks until a notify arrives for this waiter or the timeout elapses.
    ///
    /// A notify that arrived after registration but before this call is
    /// observed here (the flag is checked under the waiter's mutex before
    /// sleeping), which is what makes the register → re-check → wait idiom
    /// race-free. Spurious condvar wakeups are re-checked against the
    /// notified flag and re-slept with the remaining timeout, so `Woken`
    /// really means "a notify arrived".
    pub fn wait(&self, timeout: Duration) -> Result<WakeOutcome> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .expect("wait timeout overflows Instant");
        let mut notified = self.inner.notified.lock();
        loop {
            if *notified {
                *notified = false;
                return Ok(WakeOutcome::Woken);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(WakeOutcome::TimedOut);
            };
            let result = self.inner.condvar.wait_for(&mut notified, remaining);
            if result.timed_out() && !*notified {
                return Ok(WakeOutcome::TimedOut);
            }
            // Either the flag was set (loop head returns Woken) or the
            // wake was spurious — re-check and keep waiting.
        }
    }
}

impl Drop for RegionWaiter {
    fn drop(&mut self) {
        // Deregister from the region's waiter map, but only if the map
        // entry is still this waiter (it may have been removed and
        // recreated since registration).
        let mut map = self.map.write();
        if let Some(entry) = map.get(&self.offset)
            && Arc::ptr_eq(entry, &self.inner)
        {
            map.remove(&self.offset);
        }
    }
}

impl SharedWaiter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            notified: ParkingMutex::new(false),
            condvar: Condvar::new(),
        })
    }
}

impl SharedMemoryRegistry {
    /// Allocates a new shared region and maps it into the calling guest's memory.
    ///
    /// Returns `(region_id, page_offset)` where `page_offset` is the guest page
    /// where the region was mapped.
    pub fn allocate_region(
        &mut self,
        memory: &mut Memory,
        size: u32,
        prot: RegionProt,
    ) -> Result<(SharedRegionId, u32)> {
        if size == 0 {
            return Err(WasmError::Runtime(
                "shared region size must be greater than zero".to_string(),
            ));
        }

        // Overflow-safe page-aligned size
        let page_size = PAGE_SIZE_BYTES;
        let aligned_size = size
            .div_ceil(page_size)
            .checked_mul(page_size)
            .ok_or_else(|| {
                WasmError::Runtime("shared region size overflow during alignment".to_string())
            })?;

        let region = SharedRegion::new(aligned_size)?;
        let region_id = SharedRegionId(self.next_region_id);
        self.next_region_id += 1;

        // Map into guest memory using the shared fd (true shared pages).
        let page_offset = memory.map_shared_region(
            region.fd,
            region.len,
            region_id,
            prot,
            None,
            region.ptr,
            region.waiters_arc(),
        )?;

        region.attach();
        self.regions.insert(region_id, Arc::new(region));

        Ok((region_id, page_offset))
    }

    /// Allocates a region without mapping it into any guest memory.
    /// Used for creating regions that will be attached later.
    pub fn allocate_region_standalone(&mut self, size: u32) -> Result<SharedRegionId> {
        if size == 0 {
            return Err(WasmError::Runtime(
                "shared region size must be greater than zero".to_string(),
            ));
        }

        let page_size = PAGE_SIZE_BYTES;
        let aligned_size = size
            .div_ceil(page_size)
            .checked_mul(page_size)
            .ok_or_else(|| {
                WasmError::Runtime("shared region size overflow during alignment".to_string())
            })?;

        let region = SharedRegion::new(aligned_size)?;
        let region_id = SharedRegionId(self.next_region_id);
        self.next_region_id += 1;

        self.regions.insert(region_id, Arc::new(region));
        Ok(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn region_len(&self, region_id: SharedRegionId) -> Result<u32> {
        let region = self.region(region_id)?;
        Ok(region.len() as u32)
    }

    /// Returns a reference to the shared region (crate-internal).
    pub fn get_region(&self, region_id: SharedRegionId) -> Result<Arc<SharedRegion>> {
        self.region(region_id)
    }

    /// Registers a host waiter on `(region_id, offset)` and returns a
    /// handle for waiting.
    ///
    /// The waiter joins the region's per-offset waiter registry — the same
    /// mechanism guest `memory.atomic.wait32`/`memory.atomic.notify` use on
    /// shared ranges — so a guest notify on the address mapping `offset`
    /// wakes the returned waiter, and [`Self::notify_region`] wakes guest
    /// waiters parked on that offset.
    ///
    /// See [`RegionWaiter`] for the register → re-check → wait idiom and
    /// deregistration-on-drop semantics. Registration bounds-checks the
    /// offset against the region length.
    pub fn register_region_waiter(
        &self,
        region_id: SharedRegionId,
        offset: usize,
    ) -> Result<Arc<RegionWaiter>> {
        let region = self.region(region_id)?;
        if offset >= region.len() {
            return Err(WasmError::Runtime(format!(
                "shared region waiter offset {} out of bounds for region of {} bytes",
                offset,
                region.len()
            )));
        }
        let offset = offset as u32;

        let inner = {
            let mut map = region.waiters.write();
            map.entry(offset).or_insert_with(SharedWaiter::new).clone()
        };

        Ok(Arc::new(RegionWaiter {
            map: region.waiters_arc(),
            offset,
            inner,
        }))
    }

    /// Notifies up to `count` waiters registered on `(region_id, offset)`.
    ///
    /// Wakes both host waiters created via [`Self::register_region_waiter`]
    /// and guest threads parked in `memory.atomic.wait32`/`wait64` on the
    /// address mapping `offset`. With no registered waiter this returns
    /// zero without erroring — a notify with nobody to wake is not a fault.
    pub fn notify_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        count: u32,
    ) -> Result<u32> {
        let region = self.region(region_id)?;
        if offset >= region.len() {
            return Err(WasmError::Runtime(format!(
                "shared region notify offset {} out of bounds for region of {} bytes",
                offset,
                region.len()
            )));
        }
        Ok(shared_notify(&region.waiters, offset as u32, count))
    }

    /// Reports the engine's host-wait support level.
    ///
    /// [`HostWaitSupport::RegistryAndOsWake`] is reported only when the
    /// platform wake emission code is compiled in — a build-time decision
    /// (the `platform-wake-emission` cargo feature on a supported OS).
    /// There is no runtime toggle: the level is process-wide, identical
    /// for every registry and store, and embedders detect it rather than
    /// configure it.
    pub fn host_wait_support(&self) -> HostWaitSupport {
        if os_wake::active() {
            HostWaitSupport::RegistryAndOsWake
        } else {
            HostWaitSupport::RegistryOnly
        }
    }

    /// Destroys a shared region.
    ///
    /// The region must have no attachments.
    pub fn destroy_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        let region = self.region(region_id)?;
        if region.attachment_count() != 0 {
            return Err(WasmError::Runtime(format!(
                "shared region {} still has {} attached mappings",
                region_id.raw(),
                region.attachment_count()
            )));
        }

        self.regions.remove(&region_id);
        Ok(())
    }

    /// Attaches an existing shared region into a guest's memory.
    ///
    /// The region's physical pages are mapped into the guest's address space
    /// using `mmap(MAP_FIXED | MAP_SHARED)` with the region's fd, so writes
    /// are immediately visible to all other attached instances.
    ///
    /// Returns the page offset where the region was mapped.
    pub fn attach_region(
        &mut self,
        memory: &mut Memory,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
    ) -> Result<u32> {
        let region = self.region(region_id)?;

        let page_offset = memory.map_shared_region(
            region.fd,
            region.len,
            region_id,
            prot,
            reader_slot,
            region.ptr,
            region.waiters_arc(),
        )?;

        region.attach();
        Ok(page_offset)
    }

    /// Detaches a shared region from a guest's memory.
    ///
    /// Unmaps the region's pages from the guest's address space and restores
    /// the virtual address reservation to `PROT_NONE`.
    pub fn detach_region(&mut self, memory: &mut Memory, region_id: SharedRegionId) -> Result<()> {
        let region = self.region(region_id)?;

        memory.unmap_shared_region(region_id)?;
        region.detach();

        Ok(())
    }

    /// Writes data to a shared region directly (host-side convenience).
    ///
    /// This is `pub(crate)` — the registry's public API has no read/write
    /// methods per the shared-region-mapping spec. Host callers should go
    /// through `Instance` or `Store` wrappers instead.
    pub(crate) fn write_to_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        let region = self.region(region_id)?;
        let end = offset.checked_add(data.len()).ok_or_else(|| {
            WasmError::Runtime("shared region write offset+length overflow".to_string())
        })?;
        if end > region.len() {
            return Err(WasmError::Runtime(
                "shared region write out of bounds".to_string(),
            ));
        }
        // SAFETY: offset and length are bounds-checked above; ptr is valid.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), region.ptr.add(offset), data.len());
        }
        Ok(())
    }

    /// Reads data from a shared region directly (host-side convenience).
    ///
    /// This is `pub(crate)` — see [`write_to_region`] for rationale.
    pub(crate) fn read_from_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        let region = self.region(region_id)?;
        let end = offset.checked_add(buf.len()).ok_or_else(|| {
            WasmError::Runtime("shared region read offset+length overflow".to_string())
        })?;
        if end > region.len() {
            return Err(WasmError::Runtime(
                "shared region read out of bounds".to_string(),
            ));
        }
        // SAFETY: offset and length are bounds-checked above; ptr is valid.
        unsafe {
            std::ptr::copy_nonoverlapping(region.ptr.add(offset), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    fn region(&self, region_id: SharedRegionId) -> Result<Arc<SharedRegion>> {
        self.regions.get(&region_id).cloned().ok_or_else(|| {
            WasmError::Runtime(format!("shared region {} not found", region_id.raw()))
        })
    }
}

impl std::fmt::Debug for SharedMemoryRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedMemoryRegistry")
            .field("next_region_id", &self.next_region_id)
            .field("regions", &self.regions.len())
            .finish()
    }
}

impl Default for SharedMemoryRegistry {
    fn default() -> Self {
        Self {
            next_region_id: 1,
            regions: HashMap::new(),
        }
    }
}

/// Gets or creates the shared waiter entry for `offset` (crate-internal;
/// used by both the interpreter paths and the public API).
pub(crate) fn ensure_shared_waiter(waiters: &WaiterMap, offset: u32) {
    let mut map = waiters.write();
    map.entry(offset).or_insert_with(SharedWaiter::new);
}

/// Wakes up to `count` threads parked on the shared waiter for `offset`.
/// Returns the number of wake attempts delivered (zero when no waiter is
/// registered).
pub(crate) fn shared_notify(waiters: &WaiterMap, offset: u32, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let map = waiters.read();
    let Some(waiter) = map.get(&offset) else {
        return 0;
    };

    // The map holds ONE waiter entry per offset, whose `notified` flag
    // satisfies exactly one parked thread; notify counts above 1 can never
    // wake more than one distinct waiter and must not loop `n` times (a
    // guest notify with a huge count would spin the loop under this read
    // lock, stalling every registrant's deregistration).
    let mut notified = waiter.notified.lock();
    *notified = true;
    drop(notified);
    waiter.condvar.notify_one();
    1
}

/// Parks the calling thread on the shared waiter for `offset`.
///
/// Returns true if woken, false if the timeout elapsed. Interpreter
/// semantics: a zero timeout does not block and reports not-woken even if a
/// notify is already latched. Does not take any guest memory lock while
/// parked — only the waiter's own mutex/condvar are held. Spurious condvar
/// wakeups are re-checked against the notified flag and re-slept with the
/// remaining timeout.
pub(crate) fn shared_wait(waiters: &WaiterMap, offset: u32, timeout_ns: u64) -> bool {
    let waiter = {
        let mut map = waiters.write();
        map.entry(offset).or_insert_with(SharedWaiter::new).clone()
    };

    if timeout_ns == 0 {
        return false;
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_nanos(timeout_ns))
        .expect("wait timeout overflows Instant");
    let mut notified = waiter.notified.lock();
    loop {
        if *notified {
            *notified = false;
            return true;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        let result = waiter.condvar.wait_for(&mut notified, remaining);
        if result.timed_out() && !*notified {
            return false;
        }
        // Either the flag was set (loop head returns true) or the wake was
        // spurious — re-check and keep waiting.
    }
}
