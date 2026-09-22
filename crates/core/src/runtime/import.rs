/// The kind of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Import kind.
pub enum ImportKind {
    /// A function import (type index).
    Func(u32),
    /// A table import (table type).
    Table(crate::runtime::TableType),
    /// A memory import (memory type).
    Memory(crate::runtime::MemoryType),
    /// A global import (global type).
    Global(crate::runtime::GlobalType),
    /// A tag import (attribute, type index).
    Tag(u8, u32),
}

/// Import descriptor from the WebAssembly module.
///
/// Represents an import from an external module (e.g., host functions, memories).
#[derive(Debug, Clone, PartialEq, Eq)]
/// Import.
pub struct Import {
    /// The module name of the import.
    pub module: String,
    /// The field name of the import.
    pub name: String,
    /// The kind of import (function, table, memory, or global).
    pub kind: ImportKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import() {
        let import = Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(1),
            )),
        };
        assert_eq!(import.module, "env");
        assert_eq!(import.name, "memory");
    }
}
