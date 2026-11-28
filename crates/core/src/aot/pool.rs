//! Generic worker-pool mechanics for concurrent AOT instance execution.
//!
//! A pool runs one [`AotInstance`] on N OS threads, each entering the same
//! exported worker function with a per-worker argument. The pool owns only
//! generic mechanics:
//!
//! - thread spawn and per-worker entry arguments;
//! - trap-site capture from the engine's thread-local last-trap record;
//! - a monitor that joins every worker and reports completion;
//! - stop delivery through an embedder-supplied [`StopSignal`], retried on a
//!   short cadence because each wake may release exactly one parked worker;
//! - wedge handling: when the pool does not finish within the stop budget it
//!   is detached rather than joined, and the caller reclaims the process.
//!
//! Embedder policy stays out: what a stop *is* (an ABI word, a channel, a
//! flag), how a fault is presented, and what happens after completion all live
//! in the [`StopSignal`] implementation and the completion callback.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::aot::AotInstance;
use crate::runtime::{Result, TrapCode, WasmError, WasmValue};

/// Default time to wait for every worker to join after a stop is requested.
pub const DEFAULT_STOP_BUDGET: Duration = Duration::from_secs(5);

/// The engine's delivery channel for a pool stop request.
///
/// The engine calls [`signal`](Self::signal) once per worker on a fault and
/// repeatedly during a stop, so it must be idempotent and cheap. What it does
/// to the guest — set an ABI stop word, notify a wake word, close a channel —
/// is entirely the embedder's policy.
pub trait StopSignal: Send + Sync + 'static {
    /// Delivers one stop signal to the pool.
    fn signal(&self);
}

/// Configuration for a [`WorkerPool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerPoolConfig {
    /// Number of worker threads to spawn (at least one).
    pub worker_count: usize,
    /// Time to wait for every worker to join after a stop is requested.
    pub stop_budget: Duration,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            worker_count: 1,
            stop_budget: DEFAULT_STOP_BUDGET,
        }
    }
}

/// A worker that did not exit cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerFault {
    /// The worker id passed to the entry argument closure, or `u32::MAX`
    /// when the worker thread panicked before it could report a fault (no
    /// worker id is known in that case).
    pub worker_id: u32,
    /// The engine error rendered as text, without embedder formatting.
    pub message: String,
    /// The trap code, when the failure was a trap (including budget
    /// exhaustion).
    pub trap_code: Option<TrapCode>,
    /// The engine's last trap site: `(wasm function index, code offset)`.
    pub trap_site: Option<(u32, u32)>,
}

/// The pool's completion report, passed to the finish callback exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReport {
    /// The configured worker count.
    pub worker_count: usize,
    /// The first worker fault, if any.
    pub fault: Option<WorkerFault>,
    /// True when a stop was requested before the pool finished.
    pub stopped: bool,
}

/// The outcome of [`WorkerPool::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Every worker joined within the stop budget (or the pool had not
    /// started).
    Joined,
    /// At least one worker was still running after the budget; the pool was
    /// detached and the caller should reclaim the process.
    Wedged,
}

type FinishCallback = Arc<dyn Fn(PoolReport) + Send + Sync + 'static>;
type EntryArgs = Arc<dyn Fn(u32) -> Vec<WasmValue> + Send + Sync + 'static>;

/// A pool of OS threads entering one shared AOT instance.
pub struct WorkerPool {
    instance: Arc<AotInstance>,
    entry: u32,
    config: WorkerPoolConfig,
    stop_signal: Arc<dyn StopSignal>,
    entry_args: EntryArgs,
    on_finished: FinishCallback,
    /// Set once teardown has been claimed, so the monitor's report and the
    /// caller's stop path never double-teardown.
    stopped: Arc<AtomicBool>,
    /// First worker fault, if any.
    fault: Arc<Mutex<Option<WorkerFault>>>,
    /// Set once the monitor has finished joining the workers.
    monitor_done: Arc<AtomicBool>,
    /// The monitor thread; taken by [`stop`](Self::stop) so it is never joined
    /// from inside its own callback.
    monitor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WorkerPool {
    /// Creates a pool. Nothing is spawned until [`start`](Self::start).
    ///
    /// `entry` is the function index every worker invokes. `entry_args`
    /// produces the arguments for a given worker id. `on_finished` runs on the
    /// monitor thread exactly once when every worker has joined.
    pub fn new<F, G>(
        instance: Arc<AotInstance>,
        entry: u32,
        config: WorkerPoolConfig,
        stop_signal: Arc<dyn StopSignal>,
        entry_args: F,
        on_finished: G,
    ) -> Self
    where
        F: Fn(u32) -> Vec<WasmValue> + Send + Sync + 'static,
        G: Fn(PoolReport) + Send + Sync + 'static,
    {
        Self {
            instance,
            entry,
            config,
            stop_signal,
            entry_args: Arc::new(entry_args),
            on_finished: Arc::new(on_finished),
            stopped: Arc::new(AtomicBool::new(false)),
            fault: Arc::new(Mutex::new(None)),
            monitor_done: Arc::new(AtomicBool::new(false)),
            monitor: Mutex::new(None),
        }
    }

    /// The configured worker count.
    pub fn worker_count(&self) -> usize {
        self.config.worker_count
    }

    /// True once every worker has joined.
    pub fn is_finished(&self) -> bool {
        self.monitor_done.load(Ordering::SeqCst)
    }

    /// Spawns the workers and the monitor.
    ///
    /// A partial spawn still starts the monitor so workers that did start are
    /// joined; the spawn error is returned after that bookkeeping is in place.
    pub fn start(&mut self) -> Result<()> {
        let count = self.config.worker_count.max(1);
        let mut workers = Vec::with_capacity(count);
        let mut spawn_error: Option<std::io::Error> = None;

        for worker_id in 0..count as u32 {
            let instance = Arc::clone(&self.instance);
            let fault = Arc::clone(&self.fault);
            let stop_signal = Arc::clone(&self.stop_signal);
            let entry_args = Arc::clone(&self.entry_args);
            let entry = self.entry;
            let spawned = std::thread::Builder::new()
                .name(format!("wasmtiny-worker-{worker_id}"))
                .spawn(move || {
                    run_worker(
                        worker_id,
                        count,
                        instance,
                        entry,
                        entry_args,
                        stop_signal,
                        fault,
                    );
                });
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    spawn_error = Some(error);
                    break;
                }
            }
        }

        if spawn_error.is_some() {
            // Workers already started must observe the stop rather than park
            // forever on a pool that will never be completed.
            self.stop_signal.signal();
        }

        let monitor = self.spawn_monitor(workers);
        match monitor {
            Ok(handle) => {
                *self.monitor.lock() = Some(handle);
                match spawn_error {
                    Some(error) => Err(WasmError::Runtime(format!(
                        "worker thread spawn failed: {error}"
                    ))),
                    None => Ok(()),
                }
            }
            Err(error) => {
                self.stop_signal.signal();
                Err(WasmError::Runtime(format!(
                    "worker monitor spawn failed: {error}"
                )))
            }
        }
    }

    fn spawn_monitor(
        &self,
        workers: Vec<std::thread::JoinHandle<()>>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        let fault = Arc::clone(&self.fault);
        let stopped = Arc::clone(&self.stopped);
        let monitor_done = Arc::clone(&self.monitor_done);
        let on_finished = Arc::clone(&self.on_finished);
        let worker_count = self.config.worker_count.max(1);

        std::thread::Builder::new()
            .name("wasmtiny-worker-monitor".to_string())
            .spawn(move || {
                for worker in workers {
                    if let Err(panic) = worker.join() {
                        // A worker's closure folds every trap into `fault`; a
                        // join error means the thread panicked before doing so.
                        let detail = panic
                            .downcast_ref::<&str>()
                            .map(|message| (*message).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "worker thread panicked".to_string());
                        let mut slot = fault.lock();
                        if slot.is_none() {
                            *slot = Some(WorkerFault {
                                worker_id: u32::MAX,
                                message: detail,
                                trap_code: None,
                                trap_site: None,
                            });
                        }
                    }
                }
                // Announce completion before the report so the caller's stop
                // path never waits on a monitor that is inside its callback.
                monitor_done.store(true, Ordering::SeqCst);
                let fault = fault.lock().clone();
                on_finished(PoolReport {
                    worker_count,
                    fault,
                    stopped: stopped.load(Ordering::SeqCst),
                });
            })
    }

    /// Signals the pool to return and waits up to the stop budget for every
    /// worker to join. A wedged pool is detached, never joined.
    ///
    /// Idempotent with the monitor: the caller claims the stop, the monitor
    /// reports the completion. The stop signal is re-delivered on a short
    /// cadence, because each notify may release exactly one parked worker and
    /// an N-worker pool needs N deliveries.
    pub fn stop(&self) -> StopOutcome {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            self.stop_signal.signal();
        }

        let Some(monitor) = self.monitor.lock().take() else {
            return StopOutcome::Joined;
        };
        if self.monitor_done.load(Ordering::SeqCst) {
            drop(monitor);
            return StopOutcome::Joined;
        }

        let deadline = Instant::now() + self.config.stop_budget;
        while !self.monitor_done.load(Ordering::SeqCst) && Instant::now() < deadline {
            self.stop_signal.signal();
            std::thread::sleep(Duration::from_millis(2));
        }

        // Drop the handle either way — never join: the monitor either finished
        // its join loop (it is inside the callback) or is wedged. Joining a
        // wedged monitor would hang teardown.
        drop(monitor);
        if self.monitor_done.load(Ordering::SeqCst) {
            StopOutcome::Joined
        } else {
            StopOutcome::Wedged
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    worker_id: u32,
    worker_count: usize,
    instance: Arc<AotInstance>,
    entry: u32,
    entry_args: EntryArgs,
    stop_signal: Arc<dyn StopSignal>,
    fault: Arc<Mutex<Option<WorkerFault>>>,
) {
    let args = entry_args(worker_id);
    let Err(error) = instance.invoke_shared(entry, &args) else {
        return;
    };

    // Capture the trap site before anything else: it is thread-local and only
    // valid immediately after the failed invocation.
    let trap_site = instance.last_trap_site();
    let trap_code = match &error {
        WasmError::Trap(code) => Some(*code),
        _ => trap_site.map(|(_, _, code)| code),
    };
    let detail = WorkerFault {
        worker_id,
        message: error.to_string(),
        trap_code,
        trap_site: trap_site.map(|(function, offset, _)| (function, offset)),
    };

    // A faulted worker stops the rest of the pool. Each notify may release
    // exactly one parked worker, so deliver one per worker; the stop signal is
    // expected to make a worker that has not parked yet return immediately.
    for _ in 0..worker_count {
        stop_signal.signal();
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut slot = fault.lock();
    if slot.is_none() {
        *slot = Some(detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aot::{AotInstance, AotLoader};

    fn instance(wat: &str) -> (Arc<AotInstance>, u32) {
        let wasm = wat::parse_str(wat).expect("compile wat");
        let artifact =
            wasmtiny_aotc::compile_artifact(&wasm, &wasmtiny_aotc::CompilerConfig::host())
                .expect("compile artifact");
        let module = AotLoader::new().load(&artifact).expect("load artifact");
        let instance = Arc::new(AotInstance::new(&module).expect("instantiate"));
        let entry = instance
            .export_func_index("worker")
            .expect("worker export present");
        (instance, entry)
    }

    fn worker_args(worker_id: u32) -> Vec<WasmValue> {
        vec![WasmValue::I32(worker_id as i32)]
    }

    fn wait_for(predicate: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    struct CountingSignal(Arc<Mutex<usize>>);

    impl StopSignal for CountingSignal {
        fn signal(&self) {
            *self.0.lock() += 1;
        }
    }

    #[test]
    fn clean_exit_reports_no_fault() {
        let (instance, entry) = instance(
            r#"(module
                (func (export "worker") (param i32)))"#,
        );
        let report = Arc::new(Mutex::new(None));
        let receiver = Arc::clone(&report);
        let pool = WorkerPool::new(
            instance,
            entry,
            WorkerPoolConfig {
                worker_count: 2,
                stop_budget: Duration::from_secs(1),
            },
            Arc::new(CountingSignal(Arc::new(Mutex::new(0)))),
            worker_args,
            move |report| *receiver.lock() = Some(report),
        );
        let mut pool = pool;
        pool.start().expect("start pool");
        assert!(wait_for(|| pool.is_finished()), "pool must finish");
        let report = report.lock().clone().expect("report delivered");
        assert_eq!(report.worker_count, 2);
        assert!(report.fault.is_none());
        assert!(!report.stopped);
    }

    #[test]
    fn per_worker_argument_is_passed() {
        let (instance, entry) = instance(
            r#"(module
                (func (export "worker") (param i32)))"#,
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let pool = WorkerPool::new(
            instance,
            entry,
            WorkerPoolConfig {
                worker_count: 3,
                stop_budget: Duration::from_secs(1),
            },
            Arc::new(CountingSignal(Arc::new(Mutex::new(0)))),
            move |worker_id| {
                recorder.lock().push(worker_id);
                vec![WasmValue::I32(worker_id as i32)]
            },
            |_| {},
        );
        let mut pool = pool;
        pool.start().expect("start pool");
        assert!(wait_for(|| pool.is_finished()), "pool must finish");
        let mut seen = seen.lock().clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn trap_is_captured_with_site() {
        let (instance, entry) = instance(
            r#"(module
                (func (export "worker") (param i32)
                    unreachable))"#,
        );
        let report = Arc::new(Mutex::new(None));
        let receiver = Arc::clone(&report);
        let pool = WorkerPool::new(
            instance,
            entry,
            WorkerPoolConfig {
                worker_count: 1,
                stop_budget: Duration::from_secs(1),
            },
            Arc::new(CountingSignal(Arc::new(Mutex::new(0)))),
            worker_args,
            move |report| *receiver.lock() = Some(report),
        );
        let mut pool = pool;
        pool.start().expect("start pool");
        assert!(wait_for(|| pool.is_finished()), "pool must finish");
        let report = report.lock().clone().expect("report delivered");
        let fault = report.fault.expect("fault recorded");
        assert_eq!(fault.worker_id, 0);
        assert_eq!(fault.trap_code, Some(TrapCode::Unreachable));
        assert!(fault.trap_site.is_some(), "trap site captured");
    }

    #[test]
    fn stop_joins_a_running_worker() {
        // The worker spins until the test writes a non-zero byte to memory
        // address 0; the release happens after `stop()` has claimed the stop.
        let (instance, entry) = instance(
            r#"(module
                (memory 1)
                (func (export "worker") (param i32)
                    (loop
                        (br_if 0 (i32.eqz (i32.load8_u (i32.const 0)))))))"#,
        );
        let memory = instance.memory_handle(0).expect("memory present");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            memory
                .lock()
                .expect("memory lock")
                .write(0, &[1])
                .expect("release worker");
        });

        let report = Arc::new(Mutex::new(None));
        let receiver = Arc::clone(&report);
        let signal = Arc::new(Mutex::new(0));
        let pool = WorkerPool::new(
            instance,
            entry,
            WorkerPoolConfig {
                worker_count: 1,
                stop_budget: Duration::from_secs(5),
            },
            Arc::new(CountingSignal(Arc::clone(&signal))),
            worker_args,
            move |report| *receiver.lock() = Some(report),
        );
        let mut pool = pool;
        pool.start().expect("start pool");
        assert_eq!(pool.stop(), StopOutcome::Joined);
        release.join().expect("release thread");
        assert!(*signal.lock() > 0, "stop was signalled");
        assert!(wait_for(|| report.lock().is_some()), "report delivered");
        assert!(report.lock().clone().expect("report").stopped);
    }

    #[test]
    fn wedged_worker_is_detached_after_budget() {
        let (instance, entry) = instance(
            r#"(module
                (func (export "worker") (param i32)
                    (loop br 0)))"#,
        );
        let signal = Arc::new(Mutex::new(0));
        let pool = WorkerPool::new(
            instance,
            entry,
            WorkerPoolConfig {
                worker_count: 1,
                stop_budget: Duration::from_millis(50),
            },
            Arc::new(CountingSignal(Arc::clone(&signal))),
            worker_args,
            |_| {},
        );
        let mut pool = pool;
        pool.start().expect("start pool");
        assert_eq!(pool.stop(), StopOutcome::Wedged);
        assert!(*signal.lock() > 0, "stop was signalled");
        assert!(!pool.is_finished(), "wedged pool never finished");
    }
}
