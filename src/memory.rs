//! Memory management for WebAssembly instances.
//!
//! This module provides the [`Memory`] type which represents a WebAssembly linear
//! memory. It handles allocation, growth, reading, and writing of memory pages.
//!
//! WebAssembly memories are defined by their page count, where each page is 64 KiB.
//! The runtime supports both minimum and maximum page limits.
//!
//! # Memory Backing
//!
//! Guest linear memory is backed by `mmap` with a pre-reserved virtual address range
//! spanning the maximum allowed pages. Growth is achieved via `mprotect` rather than
//! reallocation, avoiding pointer invalidation.
//!
//! # Memory Operations
//!
//! - Create: `Memory::new(memory_type)`
//! - Read: `memory.read(offset, buffer)`
//! - Write: `memory.write(offset, data)`
//! - Grow: `memory.grow(pages)`

use std::sync::Arc;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::{
    runtime::{MemoryType, Result, SharedRegionId, TrapCode, WasmError},
    runtime::{ensure_shared_waiter, os_wake, shared_notify, shared_wait},
};

/// Maximum number of pages (65536 pages = 4 GiB).
pub const MAX_PAGES: u32 = 65536;
/// Constant `PAGE_SIZE_BYTES`.
pub const PAGE_SIZE_BYTES: u32 = 65536;

/// PROT_NONE tail reserved past the accessible capacity of every memory.
///
/// The AOT compiler bounds guest heap accesses against the memory's
/// *capacity* and converts the small (`addr + access_size`) overhang past the
/// bound into a page fault (mapped to `MemoryOutOfBounds` by the trap
/// handler). That only works if an unmapped region follows the reservation:
/// when `min == max` the reservation is exactly the accessible length, so an
/// access just past the end would otherwise land in an adjacent mapping and
/// read foreign bytes (or zeros) instead of trapping. One extra wasm page is
/// far beyond the maximum 8-byte overhang of any scalar access.
const GUARD_RESERVE_BYTES: usize = PAGE_SIZE_BYTES as usize;

/// Protection level for a shared memory region mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionProt {
    /// Read and write access.
    ReadWrite,
    /// Read-only access.
    ReadOnly,
}

/// A shared page range mapped into guest linear memory.
#[derive(Debug, Clone)]
pub struct SharedRange {
    /// Offset in guest pages where this shared range starts.
    pub page_offset: u32,
    /// The shared region this range maps from.
    pub region_id: SharedRegionId,
    /// Length in bytes.
    pub len: u32,
    /// Protection level.
    pub prot: RegionProt,
    /// Which page within the range is writable by this consumer (if any).
    pub reader_slot: Option<u32>,
    /// Base pointer of the region's host mapping. Used for Stage 2 platform
    /// wake emission (`region.ptr() + offset`); not valid for data access
    /// by the guest (the guest accesses via its own mapping).
    pub(crate) region_ptr: *mut u8,
    /// Shared waiters for atomic wait/notify on addresses within this range.
    /// This is a reference to the SharedRegion's waiters map.
    pub(crate) waiters: crate::runtime::WaiterMap,
}

/// WebAssembly linear memory.
///
/// A linear memory is a contiguous array of bytes that can be read from and
/// written to by WebAssembly code. Memory grows in units of 64 KiB pages.
///
/// Memory is backed by `mmap` with a pre-reserved virtual address range.
/// Growth uses `mprotect` to extend the accessible range without reallocation.
///
/// # Example
///
/// ```
/// use wasmtiny::runtime::{MemoryType, Limits, Memory};
///
/// let mem_type = MemoryType::new(Limits::Min(1));
/// let mut mem = Memory::new(mem_type).unwrap();
/// assert_eq!(mem.size(), 1);
///
/// mem.grow(1).unwrap();
/// assert_eq!(mem.size(), 2);
/// ```
///
/// # Safety
///
/// The `Memory` struct manages `mmap`'d memory. It is `Send` but not `Sync`
/// (it is always accessed through `Arc<Mutex<Memory>>` in the runtime).
pub struct Memory {
    mem_type: MemoryType,
    /// Base pointer of the mmap'd region.
    ptr: *mut u8,
    /// Current valid length in bytes (owned pages only).
    len: usize,
    /// Total reserved virtual address range in bytes.
    capacity: usize,
    /// Shared page ranges mapped into this memory.
    shared_ranges: Vec<SharedRange>,
    /// Cursor for top-down shared mapping placement.
    next_shared_offset: usize,
    waiters: Arc<RwLock<std::collections::HashMap<u32, Arc<Waiter>>>>,
}

#[derive(Debug)]
struct Waiter {
    notified: Mutex<bool>,
    condvar: Condvar,
}

impl Memory {
    /// Creates a new `Memory`.
    pub fn new(mem_type: MemoryType) -> Result<Self> {
        Self::try_new(mem_type)
    }

    /// Tries to create a new `Memory`.
    pub fn try_new(mem_type: MemoryType) -> Result<Self> {
        let min_pages = mem_type.limits.min();
        if min_pages > MAX_PAGES {
            return Err(WasmError::Instantiate(format!(
                "memory minimum {} pages exceeds supported limit {}",
                min_pages, MAX_PAGES
            )));
        }

        let max_pages = mem_type
            .limits
            .max()
            .unwrap_or(MAX_PAGES)
            .min(MAX_PAGES)
            .max(min_pages); // Ensure capacity >= min_pages

        let capacity = if max_pages == 0 {
            // Zero-page memory: still need a valid pointer, use 1 page as minimum capacity
            PAGE_SIZE_BYTES as usize
        } else {
            (max_pages as usize)
                .checked_mul(PAGE_SIZE_BYTES as usize)
                .ok_or_else(|| {
                    WasmError::Instantiate("memory capacity overflow during allocation".to_string())
                })?
        };

        let initial_bytes = (min_pages as usize)
            .checked_mul(PAGE_SIZE_BYTES as usize)
            .ok_or_else(|| {
                WasmError::Instantiate("memory size overflow during allocation".to_string())
            })?;

        // Reserve the accessible capacity plus a PROT_NONE guard tail (see
        // `GUARD_RESERVE_BYTES`): heap accesses are fault-backed past `len`.
        let reserve_bytes = capacity.checked_add(GUARD_RESERVE_BYTES).ok_or_else(|| {
            WasmError::Instantiate("memory capacity overflow during allocation".to_string())
        })?;
        let ptr = mmap_reserve(reserve_bytes)?;

        // Make initial pages accessible
        if initial_bytes > 0
            && let Err(e) = mprotect_range(ptr, initial_bytes, libc::PROT_READ | libc::PROT_WRITE)
        {
            // Clean up on failure
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, reserve_bytes);
            }
            return Err(e);
        }

        Ok(Self {
            mem_type,
            ptr,
            len: initial_bytes,
            capacity,
            shared_ranges: Vec::new(),
            next_shared_offset: capacity,
            waiters: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Blocks the current thread waiting for the address to be notified.
    /// Returns true if woken, false if timeout.
    pub(crate) fn wait_on(&self, address: u32, timeout_ns: u64) -> bool {
        // Shared ranges use the per-region waiter registry (the same
        // mechanism backing the public host wait/notify API). The guest
        // memory lock is not held while parked — shared_wait only takes
        // the waiter's own mutex/condvar.
        if let Some((range, region_offset)) = self.find_shared_range(address) {
            return shared_wait(&range.waiters, region_offset, timeout_ns);
        }

        // Use local waiters for owned memory
        let waiter = {
            let mut waiters = self.waiters.write();
            waiters
                .entry(address)
                .or_insert_with(|| {
                    Arc::new(Waiter {
                        notified: Mutex::new(false),
                        condvar: Condvar::new(),
                    })
                })
                .clone()
        };

        if timeout_ns == 0 {
            return false;
        }

        let mut notified = waiter.notified.lock();
        if *notified {
            *notified = false;
            return true;
        }

        let timeout = std::time::Duration::from_nanos(timeout_ns);
        let result = waiter.condvar.wait_for(&mut notified, timeout);

        if result.timed_out() {
            false
        } else {
            *notified = false;
            true
        }
    }

    /// Notifies waiters at the given address.
    /// Returns the number of waiters notified.
    pub fn notify(&self, address: u32, n: u32) -> Result<u32> {
        // Bounds check: address must be in owned range or a shared range
        if !self.is_valid_access(address, 4)? {
            return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
        }

        // Check if address falls in a shared range
        if let Some((range, region_offset)) = self.find_shared_range(address) {
            // Delegate to the shared waiters registry (same mechanism as
            // SharedMemoryRegistry::notify_region).
            let notified = shared_notify(&range.waiters, region_offset, n);

            // Stage 2 side effect: when compiled in and enabled, also emit
            // the host platform's wake primitive for the region's host
            // mapping address. This never affects the instruction's return
            // value, which counts only WASM waiters woken.
            if os_wake::active() {
                // SAFETY: region_ptr is the region's live mmap base and
                // region_offset is within its length.
                unsafe {
                    os_wake::emit_wake(range.region_ptr.add(region_offset as usize));
                }
            }

            return Ok(notified);
        }

        // Use local waiters for owned memory
        let waiters = self.waiters.read();
        let Some(waiter) = waiters.get(&address) else {
            return Ok(0);
        };

        let mut notified = 0;
        for _ in 0..n {
            let mut flag = waiter.notified.lock();
            *flag = true;
            waiter.condvar.notify_one();
            notified += 1;
        }

        Ok(notified)
    }

    /// Returns a waiter reference for the given address (for atomic wait).
    pub(crate) fn get_waiter(&self, address: u32) {
        // Check if address falls in a shared range
        if let Some((range, region_offset)) = self.find_shared_range(address) {
            ensure_shared_waiter(&range.waiters, region_offset);
            return;
        }

        // Use local waiters for owned memory
        let mut waiters = self.waiters.write();
        waiters.entry(address).or_insert_with(|| {
            Arc::new(Waiter {
                notified: Mutex::new(false),
                condvar: Condvar::new(),
            })
        });
    }

    /// Returns the size in pages (owned pages only).
    pub fn size(&self) -> u32 {
        (self.len / PAGE_SIZE_BYTES as usize) as u32
    }

    /// Returns the declared type information.
    pub fn type_(&self) -> &MemoryType {
        &self.mem_type
    }

    /// Grows the underlying resource by the requested number of pages.
    ///
    /// Uses `mprotect` to extend the accessible range — no reallocation needed.
    pub fn grow(&mut self, delta: u32) -> Result<u32> {
        let old_size = self.size();
        let new_size = old_size.saturating_add(delta);

        if let Some(max) = self.mem_type.limits.max()
            && new_size > max
        {
            return Err(WasmError::Runtime(
                "memory size exceeds maximum".to_string(),
            ));
        }

        if new_size > MAX_PAGES {
            return Err(WasmError::Runtime(
                "memory size exceeds maximum allowed".to_string(),
            ));
        }

        let old_byte_len = self.len;
        let new_byte_len = (new_size as usize) * PAGE_SIZE_BYTES as usize;

        // mprotect the new range to make it accessible
        if new_byte_len > old_byte_len {
            let extra = new_byte_len - old_byte_len;
            // SAFETY: ptr + old_byte_len is within the reserved VA range
            let new_range_ptr = unsafe { self.ptr.add(old_byte_len) };
            mprotect_range(new_range_ptr, extra, libc::PROT_READ | libc::PROT_WRITE)?;
        }

        self.len = new_byte_len;
        Ok(old_size)
    }

    /// Returns the owned length in bytes (shared ranges are placed top-down
    /// and are not contiguous with owned pages).
    pub fn len_bytes(&self) -> usize {
        self.len
    }

    /// Returns the owned-page length in bytes.
    pub fn owned_len_bytes(&self) -> usize {
        self.len
    }

    /// Returns a const pointer to the base of the memory.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Returns a mutable pointer to the base of the memory.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    /// Returns the total reserved capacity in bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.capacity
    }

    /// Returns a pointer at the given byte offset, bounds-checked against
    /// the owned range OR any live shared range.
    fn ptr_at(&self, offset: u32, access_len: usize) -> Result<*const u8> {
        if self.is_valid_access(offset, access_len)? {
            // SAFETY: offset is within bounds (owned or shared)
            Ok(unsafe { self.ptr.add(offset as usize) })
        } else {
            Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
        }
    }

    /// Returns a mutable pointer at the given byte offset, bounds-checked.
    fn ptr_at_mut(&mut self, offset: u32, access_len: usize) -> Result<*mut u8> {
        if self.is_valid_access(offset, access_len)? {
            Ok(unsafe { self.ptr.add(offset as usize) })
        } else {
            Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
        }
    }

    /// Returns true if [offset, offset+access_len) is entirely within the
    /// owned range or entirely within a live shared range.
    pub(crate) fn is_valid_access(&self, offset: u32, access_len: usize) -> Result<bool> {
        if access_len == 0 {
            return Ok(offset as usize <= self.len || self.in_shared_range(offset, 0));
        }
        let start = offset as usize;
        let end = start
            .checked_add(access_len)
            .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;
        if start < self.len && end <= self.len {
            return Ok(true);
        }
        Ok(self.in_shared_range(offset, access_len))
    }

    /// Returns true if [offset, offset+len) falls entirely within a live
    /// shared range.
    fn in_shared_range(&self, offset: u32, len: usize) -> bool {
        let start = offset as u64;
        let end = if len == 0 {
            start + 1
        } else {
            start + len as u64
        };
        for range in &self.shared_ranges {
            let range_start = (range.page_offset as u64) * PAGE_SIZE_BYTES as u64;
            let range_end = range_start + range.len as u64;
            if start >= range_start && end <= range_end {
                return true;
            }
        }
        false
    }

    /// Checks that no byte in [offset, offset+len) falls in a read-only shared range.
    pub fn check_writable(&self, offset: u32, len: usize) -> Result<()> {
        if self.shared_ranges.is_empty() {
            return Ok(());
        }
        let access_start = offset as u64;
        let access_end = access_start + len as u64;

        for range in &self.shared_ranges {
            if range.prot != RegionProt::ReadOnly {
                continue;
            }
            let range_start = (range.page_offset as u64) * PAGE_SIZE_BYTES as u64;
            let range_end = range_start + range.len as u64;

            // Check for overlap
            if access_start < range_end && range_start < access_end {
                // There is an overlap with a read-only range.
                // But if the overlap is entirely within the reader_slot page, allow it.
                if let Some(slot) = range.reader_slot {
                    let slot_start = range_start + (slot as u64) * PAGE_SIZE_BYTES as u64;
                    let slot_end = slot_start + PAGE_SIZE_BYTES as u64;
                    if access_start >= slot_start && access_end <= slot_end {
                        continue;
                    }
                }
                return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
            }
        }
        Ok(())
    }

    /// Reads bytes from the underlying resource.
    pub fn read(&self, offset: u32, buf: &mut [u8]) -> Result<()> {
        let ptr = self.ptr_at(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        // SAFETY: ptr_at has bounds-checked the access.
        // The source is valid for buf.len() bytes and the pointers don't overlap
        // (buf is a separate allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    /// Writes bytes to the underlying resource.
    pub fn write(&mut self, offset: u32, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            // Still bounds-check the offset
            let _ = self.ptr_at(offset, 0)?;
            return Ok(());
        }
        self.check_writable(offset, buf.len())?;
        let ptr = self.ptr_at_mut(offset, buf.len())?;
        // SAFETY: ptr_at_mut has bounds-checked the access.
        // check_writable verified no read-only overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, buf.len());
        }
        Ok(())
    }

    /// Reads u8.
    pub fn read_u8(&self, offset: u32) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.read(offset, &mut buf)?;
        Ok(buf[0])
    }

    /// Writes u8.
    pub fn write_u8(&mut self, offset: u32, val: u8) -> Result<()> {
        self.write(offset, &[val])
    }

    /// Reads u32.
    pub fn read_u32(&self, offset: u32) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read(offset, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Writes u32.
    pub fn write_u32(&mut self, offset: u32, val: u32) -> Result<()> {
        self.write(offset, &val.to_le_bytes())
    }

    /// Reads i32.
    pub fn read_i32(&self, offset: u32) -> Result<i32> {
        Ok(self.read_u32(offset)? as i32)
    }

    /// Writes i32.
    pub fn write_i32(&mut self, offset: u32, val: i32) -> Result<()> {
        self.write_u32(offset, val as u32)
    }

    /// Reads u64.
    pub fn read_u64(&self, offset: u32) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read(offset, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Writes u64.
    pub fn write_u64(&mut self, offset: u32, val: u64) -> Result<()> {
        self.write(offset, &val.to_le_bytes())
    }

    /// Reads i64.
    pub fn read_i64(&self, offset: u32) -> Result<i64> {
        Ok(self.read_u64(offset)? as i64)
    }

    /// Writes i64.
    pub fn write_i64(&mut self, offset: u32, val: i64) -> Result<()> {
        self.write_u64(offset, val as u64)
    }

    /// Reads f32.
    pub fn read_f32(&self, offset: u32) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32(offset)?))
    }

    /// Writes f32.
    pub fn write_f32(&mut self, offset: u32, val: f32) -> Result<()> {
        self.write_u32(offset, val.to_bits())
    }

    /// Reads f64.
    pub fn read_f64(&self, offset: u32) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64(offset)?))
    }

    /// Writes f64.
    pub fn write_f64(&mut self, offset: u32, val: f64) -> Result<()> {
        self.write_u64(offset, val.to_bits())
    }

    /// Returns the underlying data as a byte slice (owned pages only).
    ///
    /// # Safety
    ///
    /// The returned slice covers only owned pages. Shared page data is accessible
    /// at higher offsets but is not included in this slice.
    pub fn data(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: ptr is valid for self.len bytes (owned pages are PROT_READ | PROT_WRITE).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Returns the underlying data as a mutable byte slice (owned pages only).
    pub fn data_mut(&mut self) -> &mut [u8] {
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: ptr is valid for self.len bytes and we have exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Returns a reference to the shared ranges.
    pub fn shared_ranges(&self) -> &[SharedRange] {
        &self.shared_ranges
    }

    /// Finds the shared range containing the given byte offset, if any.
    /// Returns the shared range and the byte offset within the region.
    pub(crate) fn find_shared_range(&self, offset: u32) -> Option<(&SharedRange, u32)> {
        for range in &self.shared_ranges {
            let range_start = (range.page_offset as u64) * PAGE_SIZE_BYTES as u64;
            let range_end = range_start + range.len as u64;
            if (offset as u64) >= range_start && (offset as u64) < range_end {
                let region_offset = offset as u64 - range_start;
                return Some((range, region_offset as u32));
            }
        }
        None
    }

    /// Maps shared pages into the guest address space at the next available offset.
    ///
    /// Uses `mmap(MAP_FIXED | MAP_SHARED, region_fd, 0)` to map the same
    /// physical pages that back the [`SharedRegion`] into this guest's
    /// reserved virtual address range. Writes through any attached guest are
    /// immediately visible to all other guests attached to the same region.
    ///
    /// Returns the page offset where the shared region was mapped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_shared_region(
        &mut self,
        region_fd: i32,
        region_len: usize,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
        region_ptr: *mut u8,
        waiters: crate::runtime::WaiterMap,
    ) -> Result<u32> {
        let page_size = PAGE_SIZE_BYTES as usize;

        // Validate reader_slot before doing any mapping work.
        if let Some(slot) = reader_slot {
            let slot_byte_end = (slot as usize + 1) * page_size;
            if slot_byte_end > region_len {
                return Err(WasmError::Runtime(format!(
                    "reader_slot {} (byte range [{}, {})) is out of range \
                     for shared region of {} bytes",
                    slot,
                    slot as usize * page_size,
                    slot_byte_end,
                    region_len,
                )));
            }
        }

        // Reject duplicate attach
        if self.shared_ranges.iter().any(|r| r.region_id == region_id) {
            return Err(WasmError::Runtime(format!(
                "shared region {} is already attached to this memory",
                region_id.raw()
            )));
        }

        // Place shared regions top-down from the end of the reserved VA range.
        let region_len_aligned = region_len.div_ceil(page_size) * page_size;
        if region_len_aligned > self.next_shared_offset {
            return Err(WasmError::Runtime(
                "insufficient virtual address space for shared region mapping".to_string(),
            ));
        }
        self.next_shared_offset -= region_len_aligned;
        let target_byte_offset = self.next_shared_offset;
        let page_offset = (target_byte_offset / page_size) as u32;
        let target_ptr = unsafe { self.ptr.add(target_byte_offset) };

        if target_byte_offset + region_len > self.capacity {
            self.next_shared_offset += region_len_aligned;
            return Err(WasmError::Runtime(
                "insufficient virtual address space for shared region mapping".to_string(),
            ));
        }

        // SAFETY: target_ptr points into our pre-reserved VA range and
        // region_len bytes are available. region_fd is a valid shm_open fd.
        let mapped = unsafe {
            libc::mmap(
                target_ptr as *mut libc::c_void,
                region_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FIXED | libc::MAP_SHARED,
                region_fd,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            self.next_shared_offset += region_len_aligned;
            return Err(WasmError::Runtime(format!(
                "mmap(MAP_FIXED | MAP_SHARED) failed for shared region: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Apply per-page protection with rollback on failure.
        if let Some(slot) = reader_slot {
            if prot == RegionProt::ReadOnly {
                if let Err(e) = mprotect_range(target_ptr, region_len, libc::PROT_READ) {
                    unsafe { libc::munmap(target_ptr as *mut libc::c_void, region_len) };
                    self.next_shared_offset += region_len_aligned;
                    return Err(e);
                }
                let slot_offset = (slot as usize) * page_size;
                let slot_ptr = unsafe { target_ptr.add(slot_offset) };
                if let Err(e) =
                    mprotect_range(slot_ptr, page_size, libc::PROT_READ | libc::PROT_WRITE)
                {
                    unsafe { libc::munmap(target_ptr as *mut libc::c_void, region_len) };
                    self.next_shared_offset += region_len_aligned;
                    return Err(e);
                }
            }
        } else if prot == RegionProt::ReadOnly
            && let Err(e) = mprotect_range(target_ptr, region_len, libc::PROT_READ)
        {
            unsafe { libc::munmap(target_ptr as *mut libc::c_void, region_len) };
            self.next_shared_offset += region_len_aligned;
            return Err(e);
        }

        self.shared_ranges.push(SharedRange {
            page_offset,
            region_id,
            len: region_len as u32,
            prot,
            reader_slot,
            region_ptr,
            waiters,
        });

        Ok(page_offset)
    }

    /// Unmaps shared pages from the guest address space for the given region.
    ///
    /// Calls `munmap` to release the shared mapping, then re-establishes the
    /// `PROT_NONE` virtual address reservation so the range remains reserved
    /// (but inaccessible) in the guest's address space.
    pub(crate) fn unmap_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        let page_size = PAGE_SIZE_BYTES as usize;

        // Find the range to unmap.
        let range = self
            .shared_ranges
            .iter()
            .find(|r| r.region_id == region_id)
            .cloned()
            .ok_or_else(|| {
                WasmError::Runtime(format!(
                    "shared region {} not mapped in this memory",
                    region_id.raw()
                ))
            })?;

        let byte_offset = (range.page_offset as usize) * page_size;
        let target_ptr = unsafe { self.ptr.add(byte_offset) };
        let region_len = range.len as usize;

        // Unmap the shared pages. This removes the MAP_SHARED mapping so the
        // guest can no longer access the shared region's physical pages.
        // SAFETY: target_ptr was previously mapped via mmap(MAP_FIXED) with
        // region_len bytes.
        if unsafe { libc::munmap(target_ptr as *mut libc::c_void, region_len) } != 0 {
            return Err(WasmError::Runtime(format!(
                "munmap failed for shared region: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Re-establish the PROT_NONE reservation so the VA range stays
        // reserved (but inaccessible) in the guest's address space.
        // SAFETY: target_ptr is within our pre-reserved VA range and
        // region_len bytes are available (the munmap above freed them).
        let restored = unsafe {
            libc::mmap(
                target_ptr as *mut libc::c_void,
                region_len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if restored == libc::MAP_FAILED {
            return Err(WasmError::Runtime(format!(
                "mmap(PROT_NONE) failed to restore VA reservation after unmap: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Remove from tracking.
        self.shared_ranges.retain(|r| r.region_id != region_id);

        Ok(())
    }
}

// SAFETY: Memory manages an mmap'd region that is not aliased.
// Access is synchronised via the Mutex wrapper in the runtime.
unsafe impl Send for Memory {}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("mem_type", &self.mem_type)
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .field("shared_ranges", &self.shared_ranges.len())
            .finish()
    }
}

impl Clone for Memory {
    fn clone(&self) -> Self {
        let new_mem = Memory::try_new(self.mem_type.clone())
            .expect("cloned memory allocation should succeed");

        // Grow to match the current size
        let current_pages = self.size();
        let mut result = new_mem;
        if current_pages > result.size() {
            let delta = current_pages - result.size();
            result
                .grow(delta)
                .expect("cloned memory grow should succeed");
        }

        // Copy owned page data
        if self.len > 0 {
            // SAFETY: both pointers are valid for self.len bytes and non-overlapping
            // (they are separate mmap allocations).
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr, result.ptr, self.len);
            }
        }

        // Clone does NOT copy shared range metadata — the clone has no
        // shared mappings. Callers must re-attach explicitly if needed.
        result.shared_ranges = Vec::new();
        result.next_shared_offset = result.capacity;
        result
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.capacity > 0 {
            // SAFETY: ptr was allocated via mmap with `capacity +
            // GUARD_RESERVE_BYTES` bytes (see `try_new`).
            unsafe {
                libc::munmap(
                    self.ptr as *mut libc::c_void,
                    self.capacity + GUARD_RESERVE_BYTES,
                );
            }
        }
    }
}

/// Perform an mmap allocation for the full virtual address range.
fn mmap_reserve(capacity: usize) -> std::result::Result<*mut u8, WasmError> {
    // SAFETY: We're reserving virtual address space with PROT_NONE.
    // This is a standard pattern for pre-reserving VA ranges.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            capacity,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(WasmError::Instantiate(format!(
            "mmap failed to reserve {} bytes of virtual address space",
            capacity
        )));
    }
    Ok(ptr as *mut u8)
}

/// Make a range of memory accessible with the given protection.
fn mprotect_range(ptr: *mut u8, len: usize, prot: i32) -> std::result::Result<(), WasmError> {
    if len == 0 {
        return Ok(());
    }
    // SAFETY: ptr must point to a valid mmap'd region of at least `len` bytes.
    let ret = unsafe { libc::mprotect(ptr as *mut libc::c_void, len, prot) };
    if ret != 0 {
        return Err(WasmError::Runtime(format!(
            "mprotect failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runtime::Limits;

    use super::*;

    #[test]
    fn test_memory_creation() {
        let mem_type = MemoryType::new(Limits::Min(1));
        let mem = Memory::new(mem_type).unwrap();
        assert_eq!(mem.size(), 1);
    }

    #[test]
    fn test_memory_grow() {
        let mut mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        assert_eq!(mem.size(), 1);
        let old = mem.grow(1).unwrap();
        assert_eq!(old, 1);
        assert_eq!(mem.size(), 2);
    }

    #[test]
    fn test_memory_read_write() {
        let mut mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        mem.write_i32(0, 42).unwrap();
        assert_eq!(mem.read_i32(0).unwrap(), 42);
    }

    #[test]
    fn test_memory_out_of_bounds() {
        let mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        assert!(mem.read(65536, &mut [0]).is_err());
    }

    #[test]
    fn test_memory_zero_length_access_at_end_is_allowed() {
        let mut mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        let mut empty = [];

        mem.read(65536, &mut empty).unwrap();
        mem.write(65536, &[]).unwrap();
    }

    #[test]
    fn test_memory_mmap_backed() {
        let mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        // Verify the pointer is non-null and capacity is set
        assert!(!mem.as_ptr().is_null());
        assert!(mem.capacity_bytes() >= PAGE_SIZE_BYTES as usize);
    }

    #[test]
    fn test_memory_grow_extends_accessible_range() {
        let mut mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        assert_eq!(mem.size(), 1);
        assert_eq!(mem.owned_len_bytes(), PAGE_SIZE_BYTES as usize);

        mem.grow(2).unwrap();
        assert_eq!(mem.size(), 3);
        assert_eq!(mem.owned_len_bytes(), 3 * PAGE_SIZE_BYTES as usize);

        // Should be able to read/write in the grown region
        mem.write_u32(PAGE_SIZE_BYTES + 100, 0xDEAD_BEEF).unwrap();
        assert_eq!(mem.read_u32(PAGE_SIZE_BYTES + 100).unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    fn test_memory_clone_deep_copy() {
        let mut mem = Memory::new(MemoryType::new(Limits::Min(1))).unwrap();
        mem.write_i32(0, 42).unwrap();

        let mut cloned = mem.clone();
        assert_eq!(cloned.read_i32(0).unwrap(), 42);

        // Modifying the clone should not affect the original
        cloned.write_i32(0, 99).unwrap();
        assert_eq!(mem.read_i32(0).unwrap(), 42);
        assert_eq!(cloned.read_i32(0).unwrap(), 99);
    }
}
