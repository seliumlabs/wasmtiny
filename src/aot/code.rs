//! Executable-code mapping with W^X discipline.
//!
//! Compiled code is mapped read-write, copied in, the instruction cache is
//! flushed where required, and only then flipped read-execute. The mapping is
//! never simultaneously writable and executable, and it is unmapped on drop.

use crate::runtime::{Result, WasmError};

/// A mapped, read-execute region of machine code.
pub struct ExecutableCode {
    base: *mut u8,
    len: usize,
}

impl ExecutableCode {
    /// Maps the provided machine-code bytes read-execute.
    pub fn from_bytes(code: &[u8]) -> Result<Self> {
        if code.is_empty() {
            return Ok(Self {
                base: std::ptr::null_mut(),
                len: 0,
            });
        }

        let len = code.len();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(map_error("mmap"));
        }

        // SAFETY: `map` is a freshly mapped RW region of `len` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), map as *mut u8, len);
        }

        flush_icache(map as *mut u8, len);

        // Flip to RX — the mapping is never RWX at any point.
        if unsafe { libc::mprotect(map, len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            // SAFETY: `map` was mapped above.
            unsafe {
                libc::munmap(map, len);
            }
            return Err(map_error("mprotect"));
        }

        Ok(Self {
            base: map as *mut u8,
            len,
        })
    }

    /// Returns the base address of the executable mapping.
    pub fn entry(&self) -> *const u8 {
        self.base
    }

    /// Returns the length of the mapped region in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true when the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// SAFETY: the mapping owns its own reserved address range; the pointer is
// never aliased. Mapped memory is not moved, so it is also `Send`+`Sync`.
unsafe impl Send for ExecutableCode {}

unsafe impl Sync for ExecutableCode {}

impl Drop for ExecutableCode {
    fn drop(&mut self) {
        if !self.base.is_null() && self.len > 0 {
            // SAFETY: `base`/`len` describe a mapping created in `from_bytes`.
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.len);
            }
        }
    }
}

/// Flushes the instruction cache for newly written code, as required by each
/// target. x86 is coherent (no-op); aarch64 on Apple uses
/// `sys_icache_invalidate`, elsewhere the compiler-provided `__clear_cache`.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn flush_icache(_base: *mut u8, _len: usize) {}

#[cfg(all(
    target_arch = "aarch64",
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )
))]
fn flush_icache(base: *mut u8, len: usize) {
    // SAFETY: `base`..`base+len` is the freshly written code range.
    unsafe {
        sys_icache_invalidate(base, len);
    }
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut u8, len: usize);
    }
}

#[cfg(all(
    target_arch = "aarch64",
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))
))]
fn flush_icache(base: *mut u8, len: usize) {
    unsafe {
        // SAFETY: range is the freshly written code slice.
        __clear_cache(base, base.add(len));
    }
    unsafe extern "C" {
        fn __clear_cache(begin: *mut u8, end: *mut u8);
    }
}

/// Not-yet-supported architecture for code caching.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn flush_icache(_base: *mut u8, _len: usize) {}

fn map_error(operation: &str) -> WasmError {
    WasmError::Load(format!(
        "{operation} failed: {}",
        std::io::Error::last_os_error()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ret` instruction for the host ISA, as `extern "C" fn()`.
    #[cfg(target_arch = "aarch64")]
    const RET_BYTES: [u8; 4] = [0xC0, 0x03, 0x5F, 0xD6];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    const RET_BYTES: [u8; 1] = [0xC3];

    type RetFn = unsafe extern "C" fn();

    #[test]
    fn code_executes_from_rx_mapping() {
        let code = ExecutableCode::from_bytes(&RET_BYTES).expect("mapping succeeds");
        assert_eq!(code.len(), RET_BYTES.len());
        assert!(!code.is_empty());

        let f: RetFn = unsafe { std::mem::transmute(code.entry()) };
        unsafe { f() }; // returns without crashing => executable
    }

    #[test]
    fn empty_mapping_is_inert() {
        let code = ExecutableCode::from_bytes(&[]).expect("empty mapping succeeds");
        assert!(code.is_empty());
    }
}
