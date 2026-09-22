/// The kind of an export.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Export kind.
pub enum ExportKind {
    /// A function export (index into function section).
    Func(u32),
    /// A table export (index into table section).
    Table(u32),
    /// A memory export (index into memory section).
    Memory(u32),
    /// A global export (index into global section).
    Global(u32),
    /// A tag export (index into tag section).
    Tag(u32),
}

/// Export type descriptor.
///
/// Describes an exported WebAssembly entity with its name and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Export type.
pub struct ExportType {
    /// The name of the export.
    pub name: String,
    /// The kind of the export (function, table, memory, global, or tag).
    pub kind: ExportKind,
}

impl ExportType {
    /// Creates a function export descriptor.
    pub fn new_func(name: String, idx: u32) -> Self {
        Self {
            name,
            kind: ExportKind::Func(idx),
        }
    }

    /// Creates a memory export descriptor.
    pub fn new_memory(name: String, idx: u32) -> Self {
        Self {
            name,
            kind: ExportKind::Memory(idx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_func() {
        let export = ExportType::new_func("add".to_string(), 0);
        assert_eq!(export.name, "add");
        assert!(matches!(export.kind, ExportKind::Func(0)));
    }
}
