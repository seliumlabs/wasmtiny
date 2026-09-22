//! A tiny WebAssembly runtime written in Rust.
//!
//! This library provides an API for loading, instantiating, and
//! executing WebAssembly modules ahead-of-time, plus an optional
//! feature-gated classic interpreter.
//!
//! # Basic Usage (AOT path)
//!
//! ```ignore
//! use wasmtiny::aot::{AotInstance, AotLoader};
//!
//! // Load a compiled .aot artifact
//! let module = AotLoader::new().load(&aot_bytes)?;
//! let mut instance = AotInstance::new(&module)?;
//!
//! // Call a function
//! let result = instance.invoke(0, &[WasmValue::I32(1), WasmValue::I32(2)])?;
//! assert_eq!(result, vec![WasmValue::I32(3)]);
//! ```

pub use application::WasmApplication;
pub use memory::RegionProt;
pub use runtime::ExportType;
pub use runtime::FunctionType;
pub use runtime::Global;
pub use runtime::GlobalType;
pub use runtime::Instance;
pub use runtime::InstanceMeter;
pub use runtime::InstanceStats;
pub use runtime::Memory;
pub use runtime::MemoryType;
pub use runtime::Module;
pub use runtime::NumType;
pub use runtime::RefType;
pub use runtime::SharedRegionId;
pub use runtime::Table;
pub use runtime::TableType;
pub use runtime::TrapCode;
pub use runtime::ValType;
pub use runtime::WasmError;
pub use runtime::WasmValue;

/// Ahead-of-time execution: artifact loading, verification and native dispatch.
#[cfg(feature = "aot")]
pub mod aot;
/// Application APIs.
pub mod application;
pub mod engine;
/// Interpreter APIs.
pub mod interpreter;
/// Loader-related APIs.
pub mod loader;
/// Memory APIs.
pub mod memory;
/// Runtime-related APIs.
pub mod runtime;
/// Sandbox-escape testing build support. Compiled only with the
/// `security-test` feature; absent from default and release builds.
#[cfg(feature = "security-test")]
pub mod security_test;
