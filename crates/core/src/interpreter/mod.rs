//! WebAssembly interpreter implementation.
//!
//! This module provides the interpreter execution engine for WebAssembly bytecode.
//! The interpreter executes WebAssembly instructions directly without compilation.
//!
//! # Components
//!
//! - [`Interpreter`] - Main interpreter implementation with execution control
//! - [`OperandStack`] - Stack for WebAssembly values
//! - [`ControlStack`] - Stack for control flow frames (blocks, loops, functions)

pub use exec::Interpreter;
pub use stack::{ControlFrame, ControlStack, FrameKind, OperandStack};

/// Interpreter execution support.
pub mod exec;
/// Interpreter stack types.
pub mod stack;
