/// Result type alias for WebAssembly operations.
pub type Result<T> = std::result::Result<T, WasmError>;

/// WebAssembly trap codes.
///
/// These codes indicate the specific reason for a WebAssembly trap, which
/// typically terminates execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapCode {
    /// Execution reached an unreachable instruction.
    Unreachable,
    /// Memory access outside bounds.
    MemoryOutOfBounds,
    /// Memory growth exceeded maximum.
    MemoryLimitExceeded,
    /// Table access outside bounds.
    TableOutOfBounds,
    /// Indirect call type mismatch.
    IndirectCallTypeMismatch,
    /// Stack overflow.
    StackOverflow,
    /// Execution budget exceeded (metering).
    ExecutionBudgetExceeded,
    /// Integer overflow in arithmetic operation.
    IntegerOverflow,
    /// Integer division by zero.
    IntegerDivisionByZero,
    /// Invalid conversion to integer (e.g., NaN).
    InvalidConversionToInt,
    /// Call indirect on null table entry.
    CallIndirectNull,
    /// Null reference used where non-null required.
    NullReference,
    /// Trap triggered by host.
    HostTrap,
}

/// WebAssembly errors.
///
/// Represents errors that can occur during validation, loading, instantiation,
/// or execution of WebAssembly modules. Uses `thiserror` for structured error
/// variants with typed fields. The `Runtime` and `Instantiate` variants keep
/// their single-string shape for embedder compatibility.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WasmError {
    /// Module validation failed.
    #[error("Validation error: {0}")]
    Validation(String),
    /// Module loading failed.
    #[error("Load error: {0}")]
    Load(String),
    /// Module instantiation failed.
    #[error("Instantiate error: {0}")]
    Instantiate(String),
    /// Runtime error during execution.
    #[error("Runtime error: {0}")]
    Runtime(String),
    /// Execution trapped.
    #[error("Trap: {0:?}")]
    Trap(TrapCode),
    /// Unexpected end of data during decoding.
    #[error("Unexpected end of data")]
    UnexpectedEof,
    /// A declared limit was exceeded (e.g. locals, tables, br_table count).
    #[error("Limit exceeded: {0}")]
    LimitExceeded(String),
    /// Other error.
    #[error("Error: {0}")]
    Other(String),
}

impl From<std::io::Error> for WasmError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            WasmError::UnexpectedEof
        } else {
            WasmError::Load(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = WasmError::Validation("type mismatch".to_string());
        assert_eq!(format!("{}", err), "Validation error: type mismatch");
    }

    #[test]
    fn test_trap_code() {
        assert_eq!(
            format!("{:?}", TrapCode::MemoryOutOfBounds),
            "MemoryOutOfBounds"
        );
    }

    #[test]
    fn test_result_alias() {
        let result: Result<i32> = Ok(42);
        assert!(matches!(result, Ok(42)));
    }
}
