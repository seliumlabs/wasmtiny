//! Error types for the wasmtiny ahead-of-time compiler.

/// Result alias for compiler operations.
pub type CompileResult<T> = Result<T, CompileError>;

/// Errors produced while compiling `.wasm` into `.aot`.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// The input wasm failed validation.
    #[error("validation failed: {0}")]
    Validation(String),

    /// The input wasm uses a proposal outside the supported feature set.
    #[error("unsupported feature: {0}")]
    Unsupported(String),

    /// WebAssembly-to-CLIF translation failed.
    #[error("translation failed: {0}")]
    Translate(String),

    /// Machine-code generation failed.
    #[error("code generation failed: {0}")]
    Codegen(String),

    /// The requested target ISA could not be created or is unsupported.
    #[error("target ISA error: {0}")]
    Isa(String),

    /// Finish-linking of compiled functions failed.
    #[error("linking failed: {0}")]
    Link(String),

    /// An internal compiler invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<cranelift_wasm::WasmError> for CompileError {
    fn from(err: cranelift_wasm::WasmError) -> Self {
        match err {
            cranelift_wasm::WasmError::Unsupported(msg) => CompileError::Unsupported(msg),
            other => CompileError::Translate(other.to_string()),
        }
    }
}

impl From<wasmparser::BinaryReaderError> for CompileError {
    fn from(err: wasmparser::BinaryReaderError) -> Self {
        CompileError::Validation(err.to_string())
    }
}

impl From<cranelift_codegen::CodegenError> for CompileError {
    fn from(err: cranelift_codegen::CodegenError) -> Self {
        CompileError::Codegen(err.to_string())
    }
}
