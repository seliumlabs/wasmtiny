//! Compiler configuration.

use target_lexicon::Triple;

/// Configuration controlling a compilation run.
#[derive(Debug, Clone)]
pub struct CompilerConfig {
    /// The target triple to compile for. Defaults to the host triple.
    pub target: Triple,
    /// Global index of the module's shadow-stack pointer
    /// (`__stack_pointer`), when the module does **not** export it under that
    /// name (rustc/LLD can GC or hide the export in release builds).
    ///
    /// The compiler routes `global.get`/`global.set` for this index through
    /// the vmctx `stack_pointer` field, and the artifact records the index so
    /// the runtime gives each concurrent invocation a private stack slot.
    /// Defaults to auto-detection of an exported `__stack_pointer`.
    pub shadow_stack_global: Option<u32>,
}

impl CompilerConfig {
    /// Creates a new configuration targeting the current host.
    pub fn host() -> Self {
        Self::default()
    }

    /// Creates a new configuration for an explicit target triple string.
    pub fn for_target(triple: &str) -> crate::error::CompileResult<Self> {
        let triple = triple
            .parse::<Triple>()
            .map_err(|err: target_lexicon::ParseError| {
                crate::error::CompileError::Isa(format!("invalid target triple: {err}"))
            })?;
        Ok(Self {
            target: triple,
            shadow_stack_global: None,
        })
    }
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            target: Triple::host(),
            shadow_stack_global: None,
        }
    }
}
