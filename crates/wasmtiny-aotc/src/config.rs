//! Compiler configuration.

use target_lexicon::Triple;

/// Configuration controlling a compilation run.
#[derive(Debug, Clone)]
pub struct CompilerConfig {
    /// The target triple to compile for. Defaults to the host triple.
    pub target: Triple,
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
        Ok(Self { target: triple })
    }
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            target: Triple::host(),
        }
    }
}
