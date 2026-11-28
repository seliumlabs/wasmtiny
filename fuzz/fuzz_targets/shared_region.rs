#![no_main]

//! Fuzz the shared-region API surface.
//!
//! Allocation, raw writes into the mapping, waiter registration, notify, and
//! destroy must handle arbitrary bytes and derived offsets without panicking
//! or reading out of bounds.

use libfuzzer_sys::fuzz_target;
use wasmtiny::runtime::SharedMemoryRegistry;

const MAX_PAGES: u32 = 64;
const PAGE_SIZE: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let pages = u32::from(data.first().copied().unwrap_or(1)) % MAX_PAGES + 1;
    let mut registry = SharedMemoryRegistry::default();
    let Ok(region_id) = registry.allocate_region_standalone(pages * PAGE_SIZE as u32) else {
        return;
    };
    let Ok(region) = registry.get_region(region_id) else {
        return;
    };

    let payload = data.get(1..).unwrap_or_default();
    let copy_len = payload.len().min(region.len());
    // SAFETY: `region` is the registry's live mapping of `region.len()`
    // bytes, and `copy_len` never exceeds it.
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), region.ptr(), copy_len);
    }

    let offset_seed = payload
        .iter()
        .take(4)
        .enumerate()
        .fold(0u32, |acc, (index, byte)| {
            acc | (u32::from(*byte) << (index * 8))
        });
    let offset = usize::try_from(offset_seed).unwrap_or(0) % region.len();
    let count = u32::from(payload.get(4).copied().unwrap_or(1));

    let _ = registry.register_region_waiter(region_id, offset);
    let _ = registry.notify_region(region_id, offset, count);
    let _ = registry.destroy_region(region_id);
});
