//! WebAssembly module loading and validation.
//!
//! This module provides utilities for parsing and validating WebAssembly binary
//! modules.
//!
//! # Components
//!
//! - [`Parser`] - High-level WebAssembly module parser
//! - [`BinaryReader`] - Low-level binary format reader
//! - [`Validator`] - WebAssembly module validator

pub use parser::Parser;
pub use reader::BinaryReader;
pub use validator::Validator;

/// Binary WebAssembly parser support.
pub mod parser;
/// Binary reader APIs.
pub mod reader;
/// Validation APIs.
pub mod validator;
