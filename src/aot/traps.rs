//! Signal-based trap recovery: a process-global, async-signal-safe trap
//! registry and a per-call `sigsetjmp`/`siglongjmp` recovery boundary.
//!
//! Compiled webassembly lowers traps to `ud2`/`udf` (and guard-page faults to
//! SIGSEGV/SIGBUS). The handler installed here maps the faulting PC to a typed
//! [`TrapCode`] through a lock-free registry keyed on code-image ranges, then
//! returns control to the faulting thread's `invoke_function` call.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, OnceLock};

use crate::runtime::{Result, TrapCode, WasmError};

/// The platform `sigjmp_buf`. A plain buffer matching the C layout (an `int`
/// array); sized generously since the exact layout is libc-private —
/// `sigsetjmp` only writes into the space it needs.
type SigJmpBuf = [u64; 64];

// `sigsetjmp`/`siglongjmp` are macros on some platforms; the linkable symbols
// differ by OS. They are called directly (never through a wrapper): `setjmp`
// must run in a frame that is still live when the matching `longjmp` fires.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
))]
unsafe extern "C" {
    #[link_name = "sigsetjmp"]
    fn platform_sigsetjmp(env: *mut SigJmpBuf, savemask: libc::c_int) -> libc::c_int;
    #[link_name = "siglongjmp"]
    fn platform_siglongjmp(env: *mut SigJmpBuf, val: libc::c_int) -> !;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "__sigsetjmp"]
    fn platform_sigsetjmp(env: *mut SigJmpBuf, savemask: libc::c_int) -> libc::c_int;
    #[link_name = "__siglongjmp"]
    fn platform_siglongjmp(env: *mut SigJmpBuf, val: libc::c_int) -> !;
}

/// A single catch frame on the thread-local recovery stack.
struct CatchFrame {
    jmp: SigJmpBuf,
    trap: Cell<Option<TrapCode>>,
}

impl CatchFrame {
    fn new() -> Self {
        // SAFETY: an all-zero buffer is valid storage that `sigsetjmp` fully
        // initialises before any read.
        unsafe {
            Self {
                jmp: std::mem::zeroed(),
                trap: Cell::new(None),
            }
        }
    }
}

thread_local! {
    /// The per-thread stack of active recovery frames. Initialised by the
    /// first `catch_traps` call on a thread (before any native code runs).
    static CATCHES: RefCell<Vec<CatchFrame>> = const { RefCell::new(Vec::new()) };
}

/// A registered code range and its trap sites, sorted by offset.
struct RegistryNode {
    start: usize,
    end: usize,
    alive: AtomicBool,
    trap_offsets: Arc<Vec<(u32, TrapCode)>>,
    next: AtomicPtr<RegistryNode>,
}

static HEAD: AtomicPtr<RegistryNode> = AtomicPtr::new(std::ptr::null_mut());

/// The faulting program counter's classification.
enum FaultKind {
    /// A registered trap site was hit.
    Trap(TrapCode),
    /// The PC is inside registered code but not at a trap site (guard page).
    InCode,
    /// The PC is not in any registered range.
    NotOurs,
}

/// A handle that keeps a code range registered for its lifetime.
pub struct RegisteredCode {
    node: *mut RegistryNode,
}

impl RegisteredCode {
    /// Registers `len` bytes of code starting at `start`; `trap_offsets` maps
    /// byte offsets (relative to `start`) to trap codes and must be sorted.
    pub fn new(start: *const u8, len: usize, trap_offsets: Arc<Vec<(u32, TrapCode)>>) -> Self {
        let start_addr = start as usize;
        let node = Box::into_raw(Box::new(RegistryNode {
            start: start_addr,
            end: start_addr + len,
            alive: AtomicBool::new(true),
            trap_offsets,
            next: AtomicPtr::new(std::ptr::null_mut()),
        }));

        let mut head = HEAD.load(Ordering::Acquire);
        loop {
            // SAFETY: `node` is owned here; publishing it is the point of CAS.
            unsafe {
                (*node).next.store(head, Ordering::Release);
            }
            match HEAD.compare_exchange_weak(head, node, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => head = actual,
            }
        }

        Self { node }
    }
}

impl Drop for RegisteredCode {
    fn drop(&mut self) {
        // The list node is intentionally leaked (a lock-free immutable list
        // cannot be safely reclaimed under a signal handler); mark it dead so
        // later faults do not resolve against an unmapped image.
        // SAFETY: `node` is the pointer allocated in `new`.
        unsafe {
            (*(self.node)).alive.store(false, Ordering::Release);
        }
    }
}

/// Classifies the given program counter.
fn classify(pc: usize) -> FaultKind {
    let mut node = HEAD.load(Ordering::Acquire);
    while !node.is_null() {
        // SAFETY: nodes are never freed; the pointer came from `Box::into_raw`.
        let current = unsafe { &*node };
        if current.alive.load(Ordering::Acquire) && pc >= current.start && pc < current.end {
            match current
                .trap_offsets
                .binary_search_by_key(&((pc - current.start) as u32), |(offset, _)| *offset)
            {
                Ok(index) => return FaultKind::Trap(current.trap_offsets[index].1),
                Err(_) => return FaultKind::InCode,
            }
        }
        node = current.next.load(Ordering::Acquire);
    }
    FaultKind::NotOurs
}

/// Runs `body` with a recovery boundary: if native code faults, the signal
/// handler transfers control back here and this returns `Err(Trap(code))`.
#[inline(never)]
pub fn catch_traps<F: FnOnce()>(body: F) -> Result<()> {
    // The invoking thread must have the handler and its own alt-stack before
    // any native code runs — `sigaction` is process-wide but `sigaltstack`
    // is per-thread, so a thread that only ever *invokes* (never
    // instantiates) still registers here. Failure fails closed.
    ensure_installed()
        .map_err(|error| WasmError::Runtime(format!("signal machinery unavailable: {error}")))?;
    let frame = CatchFrame::new();
    let jmp_ptr = core::ptr::addr_of!(frame.jmp).cast_mut();
    // `sigsetjmp` returns twice (once directly, once via `siglongjmp`). The
    // Rust compiler does not know this, so force the value opaque to prevent
    // the longjmp branch from being optimised away.
    let armed = std::hint::black_box(unsafe { platform_sigsetjmp(jmp_ptr, 1) });
    if armed == 0 {
        // `setjmp` filled `frame` before the move; the pushed copy carries the
        // same environment, which the handler reads back on a trap.
        CATCHES.with(|c| c.borrow_mut().push(frame));
        body();
        CATCHES.with(|c| {
            c.borrow_mut().pop();
        });
        Ok(())
    } else {
        // Control returned via `siglongjmp`: a trap was delivered to the top
        // frame, which is still on the stack (its `pop` never ran).
        let trap = CATCHES.with(|c| {
            let frames = c.borrow();
            frames.last().and_then(|frame| frame.trap.get())
        });
        CATCHES.with(|c| {
            c.borrow_mut().pop();
        });
        Err(WasmError::Trap(trap.unwrap_or(TrapCode::HostTrap)))
    }
}

/// Returns the lowest stack address native code may use on this thread.
///
/// The limit is thread-relative (base of the thread's stack plus a margin) so
/// that every instance invoked on the thread — including cross-module callees
/// that carry their own `vmctx` — sees a consistent, correct bound.
pub fn thread_stack_limit(margin: usize) -> usize {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        // SAFETY: `pthread_self()` is always valid; `_np` accessors only query.
        let bottom = unsafe {
            let top = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
            let size = libc::pthread_get_stacksize_np(libc::pthread_self());
            top - size
        };
        bottom.saturating_add(margin)
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `pthread_getattr_np` requires an initialised `pthread_attr_t`
        // (zeroed qualifies on glibc/musl); all calls only query the thread.
        let bottom = unsafe {
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
                return stack_limit_from_probe(margin);
            }
            let mut base: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut size: libc::size_t = 0;
            if libc::pthread_attr_getstack(&attr, &mut base, &mut size) != 0 {
                return stack_limit_from_probe(margin);
            }
            base as usize
        };
        bottom.saturating_add(margin)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "linux"
    )))]
    {
        stack_limit_from_probe(margin)
    }
}

/// Fallback: approximate the least-safe stack address from a fresh stack
/// probe. Only used where the platform offers no thread-stack introspection;
/// cross-module instances are set at their own instantiation, close enough
/// for single-threaded invocations.
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
)))]
fn stack_limit_from_probe(margin: usize) -> usize {
    let mut marker: usize = 0;
    let probe = core::ptr::addr_of_mut!(marker) as usize;
    probe.saturating_sub(margin)
}

/// The signals the trap machinery owns, in the order used by the saved
/// previous-handler table.
const SIGNALS: [libc::c_int; 3] = [libc::SIGILL, libc::SIGSEGV, libc::SIGBUS];

/// The handler the embedder (or a sanitizer, or the Rust runtime) had
/// installed before us, so faults outside our code can be forwarded rather
/// than silently destroying process-wide handlers.
#[derive(Clone, Copy)]
struct PreviousHandler {
    /// `sa_sigaction` as a raw address (`SIG_DFL`/`SIG_IGN` preserved).
    handler: usize,
    /// The saved `sa_flags` (to know whether `SA_SIGINFO` applies).
    flags: libc::c_int,
}

static PREVIOUS_HANDLERS: OnceLock<[Option<PreviousHandler>; 3]> = OnceLock::new();

thread_local! {
    /// Whether this thread has a signal alt-stack registered. `sigaltstack`
    /// is per-thread while `sigaction` is process-wide, so every thread that
    /// enters native code must register its own.
    static ALTSTACK_READY: Cell<bool> = const { Cell::new(false) };
}

/// Installs the trap machinery: the process-wide signal handlers (once) and
/// the invoking thread's dedicated alt-stack.
///
/// Fails closed: if the OS cannot provide the machinery, instantiation and
/// invocation return an error instead of proceeding without trap recovery —
/// traps would otherwise take down the host process.
pub fn ensure_installed() -> std::result::Result<(), String> {
    static HANDLERS: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    HANDLERS.get_or_init(install_handlers).clone()?;
    ensure_thread_altstack()
}

/// Installs the process-wide handlers once per process.
fn install_handlers() -> std::result::Result<(), String> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handle_signal as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;

    let mut previous: [Option<PreviousHandler>; 3] = [None; 3];
    for (position, signal) in SIGNALS.iter().enumerate() {
        // SAFETY: `action` is fully initialised; `sigemptyset` clears the mask.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `old` is a valid out-pointer.
        if unsafe { libc::sigaction(*signal, &action, &mut old) } != 0 {
            return Err(format!(
                "sigaction for signal {signal} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        previous[position] = Some(PreviousHandler {
            handler: old.sa_sigaction,
            flags: old.sa_flags,
        });
    }
    let _ = PREVIOUS_HANDLERS.set(previous);
    Ok(())
}

/// Registers a dedicated signal stack for the current thread.
///
/// A stack-exhaustion SIGSEGV can only be recovered if the handler itself
/// does not run on the exhausted stack, so every thread that enters native
/// code needs its own alt-stack (the mapping lives for the thread's lifetime
/// and is never reclaimed — 64 KiB per invoking thread).
fn ensure_thread_altstack() -> std::result::Result<(), String> {
    if ALTSTACK_READY.get() {
        return Ok(());
    }
    const ALT_STACK_SIZE: usize = 64 * 1024;
    let alt_stack = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ALT_STACK_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if alt_stack == libc::MAP_FAILED {
        return Err(format!(
            "mmap for signal stack failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stack = libc::stack_t {
        ss_sp: alt_stack,
        ss_size: ALT_STACK_SIZE,
        ss_flags: 0,
    };
    // SAFETY: `alt_stack` is a valid RW mapping of `ALT_STACK_SIZE` bytes.
    if unsafe { libc::sigaltstack(&stack, std::ptr::null_mut()) } != 0 {
        return Err(format!(
            "sigaltstack failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    ALTSTACK_READY.set(true);
    Ok(())
}

/// The process-global signal handler.
unsafe extern "C" fn handle_signal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut core::ffi::c_void,
) {
    let pc = unsafe { pc_from_context(context) };
    match classify(pc) {
        FaultKind::NotOurs => unsafe { forward_to_previous(signal, info, context) },
        FaultKind::Trap(code) => deliver(code),
        FaultKind::InCode => deliver(TrapCode::MemoryOutOfBounds),
    }
}

/// Forwards a fault outside registered wasm code to whatever handled the
/// signal before us — an embedder's handler, a sanitizer, the Rust runtime —
/// instead of silently destroying it. Falls back to the default action
/// (crash) when there was no previous handler.
unsafe fn forward_to_previous(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut core::ffi::c_void,
) {
    let previous = PREVIOUS_HANDLERS
        .get()
        .and_then(|table| {
            SIGNALS
                .iter()
                .position(|candidate| *candidate == signal)
                .and_then(|position| table[position])
        })
        .unwrap_or(PreviousHandler {
            handler: libc::SIG_DFL,
            flags: 0,
        });

    if previous.handler == libc::SIG_DFL {
        // No previous handler: restore the default action and re-raise so
        // the fault takes its normal course.
        // SAFETY: `signal` resets our handler to the default; `raise`
        // re-delivers on this thread.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    } else if previous.handler == libc::SIG_IGN {
        // The process wanted this signal ignored before us: keep that.
    } else if previous.flags & libc::SA_SIGINFO != 0 {
        // SAFETY: the address came from a previously installed `sigaction`
        // with `SA_SIGINFO`; the three-argument calling convention applies.
        unsafe {
            let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut core::ffi::c_void) =
                std::mem::transmute(previous.handler);
            handler(signal, info, context);
        }
    } else {
        // SAFETY: the address came from a previously installed `sigaction`
        // without `SA_SIGINFO`; the one-argument convention applies.
        unsafe {
            let handler: extern "C" fn(libc::c_int) = std::mem::transmute(previous.handler);
            handler(signal);
        }
    }
}

/// Delivers `code` to the currently armed recovery frame on this thread.
fn deliver(code: TrapCode) -> ! {
    let jmp_ptr = CATCHES.with(|c| {
        c.try_borrow().ok().and_then(|frames| {
            frames.last().map(|frame| {
                frame.trap.set(Some(code));
                core::ptr::addr_of!(frame.jmp).cast_mut()
            })
        })
    });

    match jmp_ptr {
        Some(jmp) => unsafe {
            platform_siglongjmp(jmp, 1);
        },
        None => std::process::abort(),
    }
}

// ---------------------------------------------------------------------------
// Per-target program-counter recovery from a `ucontext_t`.
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn pc_from_context(context: *mut core::ffi::c_void) -> usize {
    unsafe {
        let uc = &*(context as *const libc::ucontext_t);
        (*uc.uc_mcontext).__ss.__rip as usize
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe fn pc_from_context(context: *mut core::ffi::c_void) -> usize {
    unsafe {
        let uc = &*(context as *const libc::ucontext_t);
        (*uc.uc_mcontext).__ss.__pc as usize
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn pc_from_context(context: *mut core::ffi::c_void) -> usize {
    unsafe {
        let uc = &*(context as *const libc::ucontext_t);
        uc.uc_mcontext.gregs[libc::REG_RIP as usize] as usize
    }
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
unsafe fn pc_from_context(context: *mut core::ffi::c_void) -> usize {
    unsafe {
        let uc = &*(context as *const libc::ucontext_t);
        uc.uc_mcontext.pc as usize
    }
}

#[cfg(not(any(
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
unsafe fn pc_from_context(_context: *mut core::ffi::c_void) -> usize {
    // Unsupported target: faults are never classified as ours and re-raise.
    usize::MAX
}
