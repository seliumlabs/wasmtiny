use std::{
    collections::{HashMap, VecDeque},
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

/// Shared waiter map for a region: byte offset within the region -> queue of
/// parked waiters.
///
/// Each address owns a *queue* of one node per parked thread (see
/// [`WaiterNode`]); `memory.atomic.notify(n)` pops and wakes up to `n`
/// distinct nodes, matching the threads proposal, where the old single
/// flag-per-address entry could never wake more than one waiter.
pub(crate) type WaiterMap = Arc<RwLock<HashMap<u32, Arc<WaiterQueue>>>>;

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
/// //    latched in the waiter's node, so this returns Woken immediately
/// //    instead of sleeping.
/// match waiter.wait(Duration::from_secs(1))? {
///     WakeOutcome::Woken => { /* re-check and make progress */ }
///     WakeOutcome::TimedOut => { /* backstop; retry the loop */ }
/// }
/// ```
///
/// Each registration pushes a fresh node onto the address's waiter queue —
/// the same queue guest `memory.atomic.wait32`/`memory.atomic.notify` use on
/// shared ranges — so guest notifies wake host waiters and vice versa, and
/// several host threads may register on the same address independently.
///
/// Handles are cheap to create ("register cheap, wait often"); dropping
/// the last handle for an offset deregisters its node from the queue, so no
/// stale entries are retained. Keep a bounded timeout as a backstop.
pub struct RegionWaiter {
    map: WaiterMap,
    offset: u32,
    node: Arc<WaiterNode>,
}

/// A single parked thread's wake state.
///
/// `notified` is set by a notify that popped this node out of its queue
/// (under the node's mutex), so a notify landing after the node was
/// registered but before the owner parked is latched: the park observes the
/// flag and returns immediately instead of sleeping through the timeout.
#[derive(Debug)]
pub(crate) struct WaiterNode {
    pub(crate) notified: ParkingMutex<bool>,
    pub(crate) condvar: Condvar,
}

/// A per-address queue of parked waiters.
///
/// One [`WaiterNode`] per parked thread: a notify pops up to `n` nodes and
/// wakes each, so several threads can park on the same word and a
/// `notify(n)` releases up to `n` distinct waiters (threads proposal). The
/// queue itself never holds a parked thread — waiting happens on the node's
/// own mutex/condvar, so a parked waiter never blocks a notifier.
#[derive(Debug)]
pub(crate) struct WaiterQueue {
    inner: ParkingMutex<VecDeque<Arc<WaiterNode>>>,
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
    /// observed here (the flag is checked under the node's mutex before
    /// sleeping), which is what makes the register → re-check → wait idiom
    /// race-free. Spurious condvar wakeups are re-checked against the
    /// notified flag and re-slept with the remaining timeout, so `Woken`
    /// really means "a notify arrived".
    ///
    /// The handle is re-registered on every call: a notify that woke an
    /// earlier `wait` pops the node out of the queue, so a subsequent
    /// `wait` on the same handle re-joins the queue first (register cheap,
    /// wait often). While the node is already queued (the register →
    /// re-check → wait idiom), re-registration is a no-op and exactly one
    /// registration remains. A wake consumed before the re-registration is
    /// still reported (`Woken`) and the re-registered node is removed again,
    /// so a handle never leaves a stale registration behind after `wait`
    /// returns.
    pub fn wait(&self, timeout: Duration) -> Result<WakeOutcome> {
        // Re-join the queue unless this node is still registered (a previous
        // wake popped it, a previous timeout deregistered it).
        ensure_waiter_registered(&self.map, self.offset, &self.node);
        let deadline = Instant::now()
            .checked_add(timeout)
            .expect("wait timeout overflows Instant");
        let mut notified = self.node.notified.lock();
        loop {
            if *notified {
                *notified = false;
                // The wake that set this flag popped the node; if the
                // re-registration above pushed it back (a stale latch), drop
                // it again so the handle leaves no queued registration behind.
                unregister_waiter(&self.map, self.offset, &self.node);
                return Ok(WakeOutcome::Woken);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                drop(notified);
                unregister_waiter(&self.map, self.offset, &self.node);
                return Ok(WakeOutcome::TimedOut);
            };
            let result = self.node.condvar.wait_for(&mut notified, remaining);
            if result.timed_out() && !*notified {
                drop(notified);
                unregister_waiter(&self.map, self.offset, &self.node);
                return Ok(WakeOutcome::TimedOut);
            }
            // Either the flag was set (loop head returns Woken) or the
            // wake was spurious — re-check and keep waiting.
        }
    }
}

impl Drop for RegionWaiter {
    fn drop(&mut self) {
        // Deregister this waiter's node from the region's queue. A node
        // already popped by a notify is no longer queued and is left alone.
        unregister_waiter(&self.map, self.offset, &self.node);
    }
}

impl WaiterNode {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            notified: ParkingMutex::new(false),
            condvar: Condvar::new(),
        })
    }
}

impl WaiterQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: ParkingMutex::new(VecDeque::new()),
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
    /// The waiter joins the region's per-offset waiter queue — the same
    /// mechanism guest `memory.atomic.wait32`/`memory.atomic.notify` use on
    /// shared ranges — so a guest notify on the address mapping `offset`
    /// wakes the returned waiter, and [`Self::notify_region`] wakes guest
    /// waiters parked on that offset. Several host threads may register on
    /// the same offset; each gets its own queue node.
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
        let node = register_waiter(&region.waiters, offset);

        Ok(Arc::new(RegionWaiter {
            map: region.waiters_arc(),
            offset,
            node,
        }))
    }

    /// Notifies up to `count` waiters registered on `(region_id, offset)`.
    ///
    /// Wakes both host waiters created via [`Self::register_region_waiter`]
    /// and guest threads parked in `memory.atomic.wait32`/`wait64` on the
    /// address mapping `offset`. Up to `count` *distinct* waiters are
    /// released, per the threads proposal. With no registered waiter this
    /// returns zero without erroring — a notify with nobody to wake is not
    /// a fault.
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
        Ok(notify_queue(&region.waiters, offset as u32, count))
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

/// Re-joins `node` to the queue for `key` unless it is already queued.
///
/// Used by [`RegionWaiter::wait`] so a handle whose node was popped by a
/// notify (or removed by a timeout) can wait again. No-op while the node is
/// still queued, keeping the register → re-check → wait idiom at exactly one
/// registration. Serialised with [`register_waiter`]/[`unregister_waiter`]
/// on the map write lock.
pub(crate) fn ensure_waiter_registered(waiters: &WaiterMap, key: u32, node: &Arc<WaiterNode>) {
    let mut map = waiters.write();
    let queue = map.entry(key).or_insert_with(WaiterQueue::new).clone();
    let mut inner = queue.inner.lock();
    if !inner.iter().any(|candidate| Arc::ptr_eq(candidate, node)) {
        inner.push_back(node.clone());
    }
}

/// Wakes up to `n` distinct threads parked in the queue for `key`; returns
/// the number of wake attempts delivered (zero when no waiter is queued).
///
/// Nodes are popped under the queue's own lock and woken *after* it is
/// dropped, so a notifier never holds the queue lock while another thread
/// registers or deregisters. `n == 0` notifies nobody (threads proposal).
pub(crate) fn notify_queue(waiters: &WaiterMap, key: u32, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let queue = {
        let map = waiters.read();
        map.get(&key).cloned()
    };
    let Some(queue) = queue else {
        return 0;
    };

    let mut woken = Vec::new();
    {
        let mut inner = queue.inner.lock();
        while woken.len() < n as usize {
            match inner.pop_front() {
                Some(node) => woken.push(node),
                None => break,
            }
        }
    }

    let count = woken.len() as u32;
    for node in woken {
        let mut notified = node.notified.lock();
        *notified = true;
        drop(notified);
        node.condvar.notify_one();
    }
    // `n` bounds the loop; woken.len() is at most n, so the count fits u32.
    count
}

/// Parks the calling thread on an already-registered `node` until notified
/// or timed out.
///
/// Returns true if woken, false if the timeout elapsed. A zero timeout does
/// not block: the node is deregistered and `false` is reported even if a
/// notify is already latched (interpreter semantics preserved). Does not
/// take any guest memory lock while parked — only the node's own
/// mutex/condvar are held. Spurious condvar wakeups are re-checked against
/// the notified flag and re-slept with the remaining timeout.
pub(crate) fn park_node(
    waiters: &WaiterMap,
    key: u32,
    node: &Arc<WaiterNode>,
    timeout_ns: u64,
) -> bool {
    if timeout_ns == 0 {
        unregister_waiter(waiters, key, node);
        return false;
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_nanos(timeout_ns))
        .expect("wait timeout overflows Instant");
    let mut notified = node.notified.lock();
    loop {
        if *notified {
            *notified = false;
            return true;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            drop(notified);
            unregister_waiter(waiters, key, node);
            return false;
        };
        let result = node.condvar.wait_for(&mut notified, remaining);
        if result.timed_out() && !*notified {
            drop(notified);
            unregister_waiter(waiters, key, node);
            return false;
        }
        // Either the flag was set (loop head returns true) or the wake was
        // spurious — re-check and keep waiting.
    }
}

/// Registers a fresh waiter node in the queue for `key` and returns it.
///
/// The node sits in the queue from registration until either a notify pops
/// it (the owner is then woken) or the owner removes it via
/// [`unregister_waiter`] (timeout or drop). Registration is done under the
/// map write lock so a concurrent unregister can never remove the queue out
/// from under a just-pushed node — the register → re-check → wait idiom
/// stays lost-wake-free.
pub(crate) fn register_waiter(waiters: &WaiterMap, key: u32) -> Arc<WaiterNode> {
    let node = WaiterNode::new();
    let mut map = waiters.write();
    let queue = map.entry(key).or_insert_with(WaiterQueue::new).clone();
    queue.inner.lock().push_back(node.clone());
    node
}

/// Removes `node` from the queue for `key` if it is still queued.
///
/// A node already popped by a notify is left alone. When the last node of a
/// queue is removed the queue entry itself is dropped, so addresses that are
/// no longer waited on leave no stale state behind. Serialised with
/// [`register_waiter`] on the map write lock, so a node can never be pushed
/// into a queue that a concurrent unregister has already discarded.
pub(crate) fn unregister_waiter(waiters: &WaiterMap, key: u32, node: &Arc<WaiterNode>) {
    let mut map = waiters.write();
    let Some(queue) = map.get(&key).cloned() else {
        return;
    };
    let mut inner = queue.inner.lock();
    inner.retain(|candidate| !Arc::ptr_eq(candidate, node));
    if inner.is_empty() {
        // The queue is empty and we hold the map write lock: no concurrent
        // register can be pushing into it, so removing the entry is safe.
        drop(inner);
        map.remove(&key);
    }
}
