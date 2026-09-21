//! Sandbox-escape testing build support (`security-test` feature).
//!
//! Everything in this module exists to let the corpus harness judge
//! adversarial guest binaries from *outside* the sandbox. It is
//! compiled only when the `security-test` feature is enabled and is
//! absent from default and release builds by construction.
//!
//! The load-bearing assumption (see `docs/threat-model.md`): guest
//! payloads are untrusted *binaries*. No guarantee here depends on how
//! the guest was compiled — judgement is made from observed runtime
//! behaviour only.
//!
//! Verdict protocol: the runner prints a single `VERDICT: <kind>
//! <detail>` line to stdout and exits with a code the harness can
//! classify without trusting stdout:
//!
//! | exit code | verdict  | meaning                                  |
//! |-----------|----------|------------------------------------------|
//! | 0         | clean    | guest completed without error             |
//! | 3         | trapped  | load rejected, or runtime trap/error      |
//! | other     | crashed  | panic, abort, or signal death (harness)   |
//! | timeout   | hung     | exceeded the harness deadline (harness)   |

use std::path::Path;

use crate::{
    RegionProt, WasmApplication, WasmValue, runtime::FunctionType, runtime::HostCaller,
    runtime::HostFunc, runtime::NumType, runtime::ValType,
};
#[cfg(feature = "aot")]
use crate::{
    aot::{AotInstance, AotLoader},
    runtime::ExportKind,
};

/// Fixed verdict line written by the alarm handler. Pre-formatted so
/// the handler only calls async-signal-safe functions.
const BUDGET_MSG: &[u8] = b"VERDICT: trapped budget-exhausted\n";
/// Exit code for a `clean` verdict.
pub const EXIT_CLEAN: i32 = 0;
/// Exit code for a `trapped` verdict (includes load-time rejection).
pub const EXIT_TRAPPED: i32 = 3;
/// Minimal valid module with one memory page, embedded so the
/// shared-region fuzz target needs no seed corpus.
const MINIMAL_MEMORY_MODULE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic + version
    0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 memory, min 1 page
];

/// Verdict kinds the runner itself can report. `crashed` and `hung` are
/// classified by the harness from process state, never self-reported —
/// a crashed process cannot be trusted to report anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Guest completed without error.
    Clean,
    /// Load rejected, or the runtime trapped/errored the guest.
    Trapped,
}

/// Host function that models a realistic host-call attack surface: it
/// trusts guest-supplied `(offset, len)` enough to use them, but must
/// survive adversarial values (huge lengths, negative offsets, lengths
/// that straddle the memory end) without panicking or reading host
/// memory out of bounds.
struct HostAbuseFunc;

/// Options for one corpus fixture execution.
#[derive(Debug, Default, Clone)]
pub struct FixtureOptions {
    /// Register the `env.host_abuse` host import (TM-04 attack surface).
    pub host_abuse: bool,
    /// Allocate and attach a shared region of this many 64 KiB pages
    /// before invoking the entry function; its guest base address is
    /// prepended to the entry arguments (TM-06).
    pub region_pages: u32,
    /// Entry function name; empty means run the start function.
    pub entry: String,
    /// i32 arguments for the entry function.
    pub i32_args: Vec<i32>,
}

/// A deterministic xorshift PRNG so fuzz bursts reproduce exactly from
/// a fixed seed (self-contained constraint: no external fuzzer).
#[derive(Debug, Clone)]
pub struct Prng {
    state: u64,
}

impl Verdict {
    /// The stable wire tag used in `VERDICT:` lines.
    pub fn tag(self) -> &'static str {
        match self {
            Verdict::Clean => "clean",
            Verdict::Trapped => "trapped",
        }
    }

    /// The exit code for this verdict.
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Clean => EXIT_CLEAN,
            Verdict::Trapped => EXIT_TRAPPED,
        }
    }
}

impl HostFunc for HostAbuseFunc {
    fn call(
        &self,
        caller: &mut HostCaller<'_>,
        args: &[WasmValue],
    ) -> crate::runtime::Result<Vec<WasmValue>> {
        let (offset, len) = match (args.first(), args.get(1)) {
            (Some(crate::WasmValue::I32(o)), Some(crate::WasmValue::I32(l))) => (*o, *l),
            _ => {
                return Err(crate::runtime::WasmError::Runtime(
                    "host_abuse: bad argument shape".to_string(),
                ));
            }
        };
        // A hostile length must never drive unbounded host allocation,
        // even before the runtime's bounds check runs.
        if len < 0 || len as u32 > (1 << 20) {
            return Err(crate::runtime::WasmError::Runtime(
                "host_abuse: length rejected".to_string(),
            ));
        }
        let len = len as u32 as usize;
        let memory = caller.memory(0).ok_or_else(|| {
            crate::runtime::WasmError::Runtime("host_abuse: no memory".to_string())
        })?;
        let mut buf = vec![0u8; len];
        // Bounds-checked guest-memory read: adversarial offsets must
        // error here, not read host memory.
        let guard = memory.lock().map_err(|_| {
            crate::runtime::WasmError::Runtime("host_abuse: memory lock poisoned".to_string())
        })?;
        guard
            .read(offset as u32, &mut buf)
            .map_err(|e| crate::runtime::WasmError::Runtime(format!("host_abuse: {e}")))?;
        let status = if buf.len() >= 4 {
            i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        } else {
            0
        };
        Ok(vec![crate::WasmValue::I32(status)])
    }

    fn function_type(&self) -> Option<&FunctionType> {
        None
    }
}

impl Prng {
    /// Creates a PRNG. A zero state is remixed to avoid the fixed point.
    pub fn new(seed: u64) -> Self {
        Prng {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Random value below `bound` (0 is returned for a 0 bound).
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    /// True with probability 1/2.
    pub fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 0
    }
}

/// Applies hard resource caps so a malicious guest can at most fail
/// the job, not the machine: address space (`RLIMIT_AS`), open files,
/// and processes.
///
/// `memory_mb` bounds total address space. Note macOS does not enforce
/// `RLIMIT_AS` for `mmap`; on macOS the caps are best-effort and the
/// guest-facing containment comes from the runtime's own memory limits
/// (declared module memory maxima, `MemoryLimitExceeded` traps) and
/// the harness-side rusage canary.
pub fn apply_resource_caps(memory_mb: u64) -> Result<(), String> {
    #[cfg(unix)]
    {
        // The resource constants use the platform's `rlimit_resource_t`
        // (`u32` on Linux, `i32` on macOS), so let inference produce the
        // right type instead of hardcoding `c_int`.
        let limits = [
            (libc::RLIMIT_AS, (memory_mb * 1024 * 1024) as libc::rlim_t),
            (libc::RLIMIT_NOFILE, 256 as libc::rlim_t),
            (libc::RLIMIT_NPROC, 64 as libc::rlim_t),
        ];
        for (res, limit) in limits {
            let lim = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            // SAFETY: `lim` is a valid rlimit struct for this platform
            // and `res` is a constant resource identifier.
            let rc = unsafe { libc::setrlimit(res, &lim) };
            if rc != 0 {
                if res == libc::RLIMIT_AS {
                    // macOS rejects RLIMIT_AS outright and does not
                    // enforce it for mmap; the cap is best-effort
                    // there (containment comes from runtime memory
                    // limits and the harness rusage canary). Warn but
                    // continue.
                    eprintln!("warning: setrlimit(RLIMIT_AS) not enforced on this platform");
                } else {
                    return Err(format!("setrlimit({res}) failed: rc={rc}"));
                }
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = memory_mb;
        Err("resource caps are only implemented on unix".to_string())
    }
}

/// Prints the verdict line and returns the process exit code.
pub fn finish(verdict: Verdict, detail: &str) -> i32 {
    println!("VERDICT: {} {}", verdict.tag(), detail);
    verdict.exit_code()
}

/// Fuzz target: interpreter dispatch on mutated-but-loadable modules
/// plus adversarial argument values (TM-08). Traps and errors are
/// acceptable outcomes; runtime panics are findings.
pub fn fuzz_execute(bytes: &[u8]) {
    let mut app = WasmApplication::new();
    let Ok(idx) = app.load_module_from_memory(bytes) else {
        return;
    };
    if app.instantiate(idx).is_err() {
        return;
    }
    // Adversarial argument values derived from the input itself.
    let arg = |i: usize| {
        let b = bytes.get(i..i + 4).unwrap_or(&[0; 4]);
        WasmValue::I32(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let _ = app.call_function(idx, "run", &[arg(0), arg(4)]);
    let _ = app.call_function(idx, "main", &[arg(8)]);
    let _ = app.execute_start(idx);
}

/// Fuzz target: AOT artifact load + native dispatch on mutated artifacts
/// and adversarial argument values. Traps and errors are acceptable
/// outcomes; runtime panics are findings.
#[cfg(feature = "aot")]
pub fn fuzz_execute_aot(bytes: &[u8]) {
    let module = match AotLoader::new().load(bytes) {
        Ok(module) => module,
        Err(_) => return,
    };
    let Ok(mut instance) = AotInstance::new(&module) else {
        return;
    };

    // Adversarial argument values derived from the input itself.
    let arg = |i: usize| {
        let b = bytes.get(i..i + 4).unwrap_or(&[0; 4]);
        WasmValue::I32(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };

    let func_index = |name: &str| {
        module
            .exports
            .iter()
            .find(|export| export.name == name)
            .and_then(|export| match export.kind {
                ExportKind::Func(idx) => Some(idx),
                _ => None,
            })
    };

    if let Some(idx) = func_index("run") {
        let _ = instance.invoke(idx, &[arg(0), arg(4)]);
    }
    if let Some(idx) = func_index("main") {
        let _ = instance.invoke(idx, &[arg(8)]);
    }
}

/// Fuzz target: module loader + validator (TM-07). Any `Ok`/`Err`
/// outcome is acceptable; a panic, abort, or non-termination is a
/// finding.
pub fn fuzz_load(bytes: &[u8]) {
    let mut app = WasmApplication::new();
    let _ = app.load_module_from_memory(bytes);
}

/// Fuzz target: AOT artifact loader + verifier. Any `Ok`/`Err` outcome is
/// acceptable; a panic is a finding.
#[cfg(feature = "aot")]
pub fn fuzz_load_aot(bytes: &[u8]) {
    let _ = AotLoader::new().load(bytes);
}

/// Fuzz target: shared-region API surface — allocate, attach, write,
/// read, detach, destroy with adversarial sizes and offsets (TM-09).
/// `Ok`/`Err` outcomes are acceptable; panics are findings.
pub fn fuzz_shared_region(bytes: &[u8]) {
    let u32_at = |i: usize| -> u32 {
        let b = bytes.get(i..i + 4).unwrap_or(&[0; 4]);
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };

    let mut app = WasmApplication::new();
    let Ok(idx) = app.load_module_from_memory(MINIMAL_MEMORY_MODULE) else {
        return;
    };
    if app.instantiate(idx).is_err() {
        return;
    }

    // Sizes and offsets come from fuzzed bytes; clamp sizes to keep
    // the burst fast while still exercising the boundary.
    let size = (u32_at(0) % (4 * 64 * 1024)) + 1;
    let Ok((region_id, _base)) = app.allocate_shared_region(idx, size, RegionProt::ReadWrite)
    else {
        return;
    };
    let offset = u32_at(4) % (size.max(1));
    let data = bytes.iter().copied().take(16).collect::<Vec<u8>>();
    let _ = app.write_shared_region(idx, region_id, offset as usize, &data);
    let mut buf = vec![0u8; data.len()];
    let _ = app.read_shared_region(idx, region_id, offset as usize, &mut buf);
    let _ = app.detach_shared_region(idx, region_id);
    let _ = app.destroy_shared_region(idx, region_id);
}

/// Installs a wall-clock budget timer. When it fires, the process
/// prints a `trapped budget-exhausted` verdict line and exits with the
/// trap exit code. The harness deadline is always strictly longer, so
/// a fixture that reaches the harness timeout ignored its budget —
/// that is a `hung` verdict and a failed run.
pub fn install_budget_timer(budget_ms: u64) {
    let secs = (budget_ms / 1000) as libc::time_t;
    let usecs = (budget_ms % 1000) as libc::suseconds_t;
    let itv = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: secs,
            tv_usec: usecs,
        },
    };
    // SAFETY: `on_budget_alarm` is an extern "C" fn usable as a signal
    // handler; `itv` is a valid itimerval.
    unsafe {
        libc::signal(
            libc::SIGALRM,
            on_budget_alarm as *const () as libc::sighandler_t,
        );
        libc::setitimer(libc::ITIMER_REAL, &itv, std::ptr::null_mut());
    }
}

/// Applies one random mutation to `input`, producing a new vector.
pub fn mutate(prng: &mut Prng, input: &[u8], seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut out = input.to_vec();
    match prng.below(6) {
        0 => {
            // Flip a bit.
            if let Some(i) = out.len().checked_sub(1) {
                let idx = prng.below(i + 1);
                out[idx] ^= 1 << prng.below(8);
            }
        }
        1 => {
            // Overwrite a byte with a random (often structural) value.
            if let Some(i) = out.len().checked_sub(1) {
                let idx = prng.below(i + 1);
                let choices = [0x00, 0x01, 0x7F, 0x80, 0xFF, 0x0B, 0x0A, 0x05];
                out[idx] = choices[prng.below(choices.len())];
            }
        }
        2 => {
            // Truncate.
            if !out.is_empty() {
                out.truncate(prng.below(out.len()));
            }
        }
        3 => {
            // Extend with bytes from a random seed (splicing).
            if let Some(seed) = seeds
                .get(prng.below(seeds.len().max(1)))
                .or_else(|| seeds.first())
            {
                let at = prng.below(out.len() + 1);
                let take = prng.below(seed.len().saturating_add(1));
                let start = seed.len().saturating_sub(take);
                let end = at.min(out.len());
                let mut spliced = Vec::with_capacity(out.len() + take);
                spliced.extend_from_slice(&out[..end]);
                spliced.extend_from_slice(&seed[start..]);
                spliced.extend_from_slice(&out[end..]);
                out = spliced;
            }
        }
        4 => {
            // Insert a random byte.
            let at = prng.below(out.len() + 1);
            out.insert(at, (prng.next_u64() & 0xFF) as u8);
        }
        _ => {
            // Delete a byte.
            if let Some(i) = out.len().checked_sub(1) {
                out.remove(prng.below(i + 1));
            }
        }
    }
    out
}

/// Counts open file descriptors via the platform fd directory.
/// `/dev/fd` on macOS, `/proc/self/fd` on Linux. Reading the directory
/// itself holds one fd open during iteration; that is consistent
/// before/after so deltas remain meaningful.
pub fn open_fd_count() -> usize {
    #[cfg(target_os = "macos")]
    let dir = "/dev/fd";
    #[cfg(not(target_os = "macos"))]
    let dir = "/proc/self/fd";
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

/// Runs one fixture inside this process. Prints the verdict line and
/// returns the process exit code. Panics are *not* caught: a panicking
/// runner exits non-zero with code 101 and the harness classifies it
/// as `crashed` — that is the correct verdict for a runtime defect.
pub fn run_fixture(wasm_path: &Path, opts: &FixtureOptions) -> i32 {
    let mut app = WasmApplication::new();

    let module_idx = match app.load_module_from_file(wasm_path) {
        Ok(idx) => idx,
        Err(e) => return finish(Verdict::Trapped, &format!("load-rejected: {e}")),
    };

    if opts.host_abuse {
        let func_type = FunctionType::new(
            vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I32)],
        );
        if let Err(e) = app.register_host_function(
            module_idx,
            "env",
            "host_abuse",
            Box::new(HostAbuseFunc),
            func_type,
        ) {
            return finish(Verdict::Trapped, &format!("hostfn-rejected: {e}"));
        }
    }

    if let Err(e) = app.instantiate(module_idx) {
        return finish(Verdict::Trapped, &format!("instantiate-rejected: {e}"));
    }

    let mut args: Vec<WasmValue> = Vec::new();
    if opts.region_pages > 0 {
        let size = opts.region_pages.saturating_mul(64 * 1024);
        match app.allocate_shared_region(module_idx, size, RegionProt::ReadWrite) {
            Ok((_region_id, page_offset)) => {
                // Prepend the guest-visible base address of the region.
                args.push(WasmValue::I32((page_offset * 64 * 1024) as i32));
            }
            Err(e) => {
                return finish(Verdict::Trapped, &format!("region-denied: {e}"));
            }
        }
    }
    args.extend(opts.i32_args.iter().map(|v| WasmValue::I32(*v)));

    let result = if opts.entry.is_empty() {
        app.execute_start(module_idx).map(|_| ())
    } else {
        app.call_function(module_idx, &opts.entry, &args)
            .map(|_| ())
    };

    match result {
        Ok(()) => finish(Verdict::Clean, "guest completed"),
        Err(e) => finish(Verdict::Trapped, &format!("runtime: {e}")),
    }
}

/// Runs one AOT fixture inside this process: loads and verifies a `.aot`
/// artifact, instantiates it (replaying segments and running the start
/// function), and invokes the requested entry export. Same verdict
/// protocol as [`run_fixture`]; panics are likewise *not* caught (a
/// crashing runner is the `crashed` verdict).
///
/// The `host_abuse` and `region_pages` options are embedder-interpreter
/// plumbing and are rejected here; the AOT corpus pass never requests
/// them.
#[cfg(feature = "aot")]
pub fn run_fixture_aot(aot_path: &Path, opts: &FixtureOptions) -> i32 {
    if opts.host_abuse || opts.region_pages > 0 {
        return finish(
            Verdict::Trapped,
            "aot-runner: host_abuse/region_pages are unsupported on the AOT path",
        );
    }

    let bytes = match std::fs::read(aot_path) {
        Ok(bytes) => bytes,
        Err(e) => return finish(Verdict::Trapped, &format!("read-rejected: {e}")),
    };
    let module = match AotLoader::new().load(&bytes) {
        Ok(module) => module,
        Err(e) => return finish(Verdict::Trapped, &format!("load-rejected: {e}")),
    };
    // Instantiation replays data/elem segments and runs the start
    // function; a trapping start fails instantiation here, mapping to
    // the trapped verdict just like the interpreter path.
    let mut instance = match AotInstance::new(&module) {
        Ok(instance) => instance,
        Err(e) => return finish(Verdict::Trapped, &format!("instantiate-rejected: {e}")),
    };

    if opts.entry.is_empty() {
        return finish(Verdict::Clean, "guest completed");
    }

    let args: Vec<WasmValue> = opts.i32_args.iter().map(|v| WasmValue::I32(*v)).collect();
    match instance.invoke_export(&opts.entry, &args) {
        Ok(_) => finish(Verdict::Clean, "guest completed"),
        Err(e) => finish(Verdict::Trapped, &format!("runtime: {e}")),
    }
}

extern "C" fn on_budget_alarm(_sig: libc::c_int) {
    // Async-signal-safe only: write(2) with a fixed buffer, then _exit.
    // The guest burned its wall-clock budget; that is a containment
    // outcome, not a runtime bug, so it maps to the trapped verdict.
    // SAFETY: writing a fixed buffer to stdout and terminating.
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            BUDGET_MSG.as_ptr().cast::<libc::c_void>(),
            BUDGET_MSG.len(),
        );
        libc::_exit(EXIT_TRAPPED);
    }
}
