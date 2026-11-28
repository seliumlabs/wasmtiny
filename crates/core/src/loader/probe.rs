//! Fact-only module probing for embedders.
//!
//! Embedders classify guests from facts about their module — declared
//! memories, imports, exports, custom sections, and whether the code contains
//! an atomic notify — but the *policy* built on those facts stays with the
//! embedder. This module owns the facts so no embedder needs its own wasm
//! section cursor, LEB128 reader, or opcode scan.
//!
//! [`probe`] parses and validates the module with the same loader the engine
//! uses, so a module that probes successfully is one the engine would accept,
//! and a malformed module yields `Err` rather than a partial fact set.

use crate::loader::{Parser, Validator};
use crate::runtime::{CustomSection, ExportType, Import, MemoryType, Result};

/// The facts a module probe reports.
///
/// These are observations only. Whether shared memory plus an atomic notify
/// means "fast path capable", or whether a given export selects a worker pool,
/// is embedder policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProbe {
    /// Memory types declared by the module (not imported memories).
    pub memories: Vec<MemoryType>,
    /// Exports declared by the module.
    pub exports: Vec<ExportType>,
    /// Imports required by the module.
    pub imports: Vec<Import>,
    /// Custom sections retained from the module binary, in order.
    pub custom_sections: Vec<CustomSection>,
    /// True when the code section contains the `memory.atomic.notify` opcode
    /// sequence (`0xFE 0x00`).
    ///
    /// Heuristic in one direction only: an unrelated immediate can produce a
    /// false positive, but a genuine notify always produces the sequence.
    pub uses_atomic_notify: bool,
}

/// Probes a WebAssembly module for embedder-relevant facts.
///
/// The module is parsed and validated exactly as the engine loader does, so
/// malformed input returns `Err` and never panics.
pub fn probe(bytes: &[u8]) -> Result<ModuleProbe> {
    let module = Parser::new().parse(bytes)?;
    Validator::new().validate(&module)?;
    Ok(ModuleProbe {
        memories: module.memories,
        exports: module.exports,
        imports: module.imports,
        custom_sections: module.custom_sections,
        uses_atomic_notify: module.uses_atomic_notify,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_shared_memory_imports_exports_and_custom_sections() {
        let bytes = wat::parse_str(
            r#"(module
                (import "env" "clock" (func (result i64)))
                (memory 1 2 shared)
                (func (export "run"))
                (@custom "selium.abi.version" "\01\00\00\00"))"#,
        )
        .expect("compile probe fixture");

        let probe = probe(&bytes).expect("probe valid module");
        assert_eq!(probe.memories.len(), 1);
        assert!(probe.memories[0].shared);
        assert!(probe.imports.iter().any(|import| import.module == "env"));
        assert!(probe.exports.iter().any(|export| export.name == "run"));
        assert_eq!(probe.custom_sections.len(), 1);
        assert_eq!(probe.custom_sections[0].name, "selium.abi.version");
        assert_eq!(probe.custom_sections[0].payload, vec![1, 0, 0, 0]);
    }

    #[test]
    fn probe_detects_atomic_notify() {
        let bytes = wat::parse_str(
            r#"(module
                (memory 1 2 shared)
                (func (export "notify") (param i32) (result i32)
                    (memory.atomic.notify (local.get 0) (i32.const 1))))"#,
        )
        .expect("compile notify fixture");
        assert!(
            probe(&bytes)
                .expect("probe notify module")
                .uses_atomic_notify
        );
    }

    #[test]
    fn probe_reports_absence_of_atomic_notify() {
        let bytes = wat::parse_str(
            r#"(module
                (memory 1 2 shared)
                (func (export "no_notify")))"#,
        )
        .expect("compile non-notify fixture");
        assert!(
            !probe(&bytes)
                .expect("probe non-notify module")
                .uses_atomic_notify
        );
    }

    #[test]
    fn probe_returns_err_for_malformed_input() {
        assert!(probe(b"not a module").is_err());
        assert!(probe(&[0x00, 0x61, 0x73]).is_err());
    }
}
