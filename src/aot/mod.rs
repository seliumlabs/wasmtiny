//! Ahead-of-time (`.aot`) execution support for the wasmtiny runtime.
//!
//! This module loads finish-linked `.aot` artifacts, verifies their integrity,
//! maps their machine code executable, and (in later stages) executes it.
//! It mirrors — but never links — the `wasmtiny-aotc` compiler crate.

pub mod code;
mod context;
pub mod exec;
mod format;
pub mod loader;
mod reader;
pub mod store;
mod traps;
pub mod verifier;

pub use code::ExecutableCode;
pub use exec::AotInstance;
pub use loader::{AotFunction, AotLoader, AotModule};
pub use store::{AotExtern, AotStore, AotTable};
