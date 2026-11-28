//! Ahead-of-time (`.aot`) execution support for the wasmtiny runtime.
//!
//! This module loads finish-linked `.aot` artifacts, verifies their integrity,
//! maps their machine code executable, and (in later stages) executes it.
//! It mirrors — but never links — the `wasmtiny-aotc` compiler crate.

pub use code::ExecutableCode;
pub use exec::{AotInstance, InstanceOptions};
pub use loader::{AotFunction, AotLoader, AotModule};
pub use pool::{
    DEFAULT_STOP_BUDGET, PoolReport, StopOutcome, StopSignal, WorkerFault, WorkerPool,
    WorkerPoolConfig,
};
pub use store::{AotExtern, AotStore, AotTable};

pub mod code;
mod context;
pub mod exec;
mod format;
pub mod loader;
pub mod pool;
mod reader;
pub mod store;
mod traps;
pub mod verifier;
