use std::sync::atomic::{AtomicU64, Ordering};

use super::{ExportType, FunctionType, GlobalType, Import, ImportKind, MemoryType, TableType};

static NEXT_MODULE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
/// A local declaration in a function body.
pub struct Local {
    /// Number of locals with this type.
    pub count: u32,
    /// Value type of the declared locals.
    pub type_: super::ValType,
}

#[derive(Debug, Clone)]
/// A function defined in the module.
pub struct Func {
    /// Index into the module type section.
    pub type_idx: u32,
    /// Locals declared by the function body.
    pub locals: Vec<Local>,
    /// Raw function body bytes.
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
/// The placement mode for a data segment.
pub enum DataKind {
    /// An active data segment initialised into a target memory.
    Active {
        /// The target memory index.
        memory_idx: u32,
        /// The encoded constant-expression offset.
        offset: Vec<u8>,
    },
    /// A passive data segment applied by bulk-memory instructions.
    Passive,
}

#[derive(Debug, Clone)]
/// A data segment declared by the module.
pub struct DataSegment {
    /// Whether the segment is active or passive.
    pub kind: DataKind,
    /// Initial bytes stored in the segment.
    pub init: Vec<u8>,
}

#[derive(Debug, Clone)]
/// The placement mode for an element segment.
pub enum ElemKind {
    /// An active element segment initialised into a target table.
    Active {
        /// The target table index.
        table_idx: u32,
        /// The encoded constant-expression offset.
        offset: Vec<u8>,
    },
    /// A passive element segment applied by bulk-memory instructions.
    Passive,
    /// A declarative element segment used only for validation.
    Declarative,
}

#[derive(Debug, Clone)]
/// An element segment declared by the module.
pub struct ElemSegment {
    /// Whether the segment is active, passive, or declarative.
    pub kind: ElemKind,
    /// Reference type stored by the segment.
    pub type_: super::RefType,
    /// Whether the element segment permits null references.
    pub nullable: bool,
    /// Encoded initialiser expressions for each element.
    pub init: Vec<Vec<u8>>,
    /// Whether this segment was synthesised from a table declaration initialiser.
    pub generated_by_table_init: bool,
}

#[derive(Debug, Clone)]
/// Parsed WebAssembly module.
pub struct Module {
    /// Stable identity assigned at construction; preserved by `Clone`.
    ///
    /// Modules are deep-cloned liberally by the runtime (per-invoke module
    /// `Arc`s, guest-native targets), so pointer equality cannot be used to
    /// test module identity.
    pub id: u64,
    /// Function signatures declared in the type section.
    pub types: Vec<FunctionType>,
    /// Functions defined by the module.
    pub funcs: Vec<Func>,
    /// Table types declared by the module.
    pub tables: Vec<TableType>,
    /// Memory types declared by the module.
    pub memories: Vec<MemoryType>,
    /// Global types declared by the module.
    pub globals: Vec<GlobalType>,
    /// Initialiser expressions for defined globals.
    pub global_inits: Vec<Vec<u8>>,
    /// Exports declared by the module.
    pub exports: Vec<ExportType>,
    /// Imports required by the module.
    pub imports: Vec<Import>,
    /// The optional start function index.
    pub start: Option<u32>,
    /// Data segments declared by the module.
    pub data: Vec<DataSegment>,
    /// Element segments declared by the module.
    pub elems: Vec<ElemSegment>,
    /// DataCount section value (number of data segments), if present.
    pub data_count: Option<u32>,
}

impl Module {
    /// Creates a new `Module`.
    pub fn new() -> Self {
        Self {
            id: NEXT_MODULE_ID.fetch_add(1, Ordering::Relaxed),
            types: Vec::new(),
            funcs: Vec::new(),
            tables: Vec::new(),
            memories: Vec::new(),
            globals: Vec::new(),
            global_inits: Vec::new(),
            exports: Vec::new(),
            imports: Vec::new(),
            start: None,
            data: Vec::new(),
            elems: Vec::new(),
            data_count: None,
        }
    }

    /// Returns the function type at the given index.
    pub fn type_at(&self, idx: u32) -> Option<&FunctionType> {
        self.types.get(idx as usize)
    }

    /// Returns or updates table at.
    pub fn table_at(&self, idx: u32) -> Option<&TableType> {
        let mut import_table_count = 0u32;
        for import in &self.imports {
            if let ImportKind::Table(ref table_type) = import.kind {
                if import_table_count == idx {
                    return Some(table_type);
                }
                import_table_count += 1;
            }
        }

        let local_idx = idx.checked_sub(import_table_count)?;
        self.tables.get(local_idx as usize)
    }

    /// Returns or updates memory at.
    pub fn memory_at(&self, idx: u32) -> Option<&MemoryType> {
        let mut import_memory_count = 0u32;
        for import in &self.imports {
            if let ImportKind::Memory(ref memory_type) = import.kind {
                if import_memory_count == idx {
                    return Some(memory_type);
                }
                import_memory_count += 1;
            }
        }

        let local_idx = idx.checked_sub(import_memory_count)?;
        self.memories.get(local_idx as usize)
    }

    /// Returns the global type at the given index.
    pub fn global_at(&self, idx: u32) -> Option<&GlobalType> {
        let mut import_global_count = 0u32;
        for import in &self.imports {
            if let ImportKind::Global(ref global_type) = import.kind {
                if import_global_count == idx {
                    return Some(global_type);
                }
                import_global_count += 1;
            }
        }

        let local_idx = idx.checked_sub(import_global_count)?;
        self.globals.get(local_idx as usize)
    }

    /// Returns the function signature for the given function index.
    pub fn func_type(&self, func_idx: u32) -> Option<&FunctionType> {
        let mut import_func_count = 0u32;
        for import in &self.imports {
            if let ImportKind::Func(type_idx) = import.kind {
                if import_func_count == func_idx {
                    return self.type_at(type_idx);
                }
                import_func_count += 1;
            }
        }

        let local_idx = func_idx.checked_sub(import_func_count)?;
        let func = self.defined_func_at(local_idx)?;
        self.type_at(func.type_idx)
    }

    /// Returns the total number of functions, including imports.
    pub fn func_count(&self) -> u32 {
        let import_count = self
            .imports
            .iter()
            .filter(|i| matches!(i.kind, ImportKind::Func(_)))
            .count() as u32;
        import_count + self.funcs.len() as u32
    }

    pub(crate) fn defined_func_at(&self, idx: u32) -> Option<&Func> {
        self.funcs.get(idx as usize)
    }
}

impl Default for Module {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Func, FunctionType, GlobalType, Import, ImportKind, MemoryType, Module, TableType,
    };
    use crate::runtime::Limits;
    use crate::{NumType, RefType, ValType};

    #[test]
    fn test_module_creation() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I32)],
        ));

        assert_eq!(module.types.len(), 1);
    }

    #[test]
    fn test_func_type_lookup() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I32)],
        ));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![],
        });

        assert!(module.func_type(0).is_some());
        assert!(module.func_type(1).is_none());
    }

    #[test]
    fn test_imported_type_accessors_use_combined_index_space() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: ImportKind::Table(TableType::new(RefType::FuncRef, Limits::Min(1))),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: ImportKind::Memory(MemoryType::new(Limits::Min(2))),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: ImportKind::Global(GlobalType::new(ValType::Num(NumType::I32), false)),
        });
        module
            .tables
            .push(TableType::new(RefType::ExternRef, Limits::Min(3)));
        module.memories.push(MemoryType::new(Limits::Min(4)));
        module
            .globals
            .push(GlobalType::new(ValType::Num(NumType::I64), true));

        assert_eq!(module.table_at(0).unwrap().elem_type, RefType::FuncRef);
        assert_eq!(module.table_at(1).unwrap().elem_type, RefType::ExternRef);
        assert_eq!(module.memory_at(0).unwrap().limits.min(), 2);
        assert_eq!(module.memory_at(1).unwrap().limits.min(), 4);
        assert_eq!(
            module.global_at(0).unwrap().content_type,
            ValType::Num(NumType::I32)
        );
        assert_eq!(
            module.global_at(1).unwrap().content_type,
            ValType::Num(NumType::I64)
        );
    }
}
