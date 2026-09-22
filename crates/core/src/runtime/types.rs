use super::{Result, WasmValue};

/// WebAssembly numeric types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Num type.
pub enum NumType {
    /// 32-bit integer type.
    I32,
    /// 64-bit integer type.
    I64,
    /// 32-bit floating-point type (IEEE 754).
    F32,
    /// 64-bit floating-point type (IEEE 754).
    F64,
}

/// WebAssembly reference types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Ref type.
pub enum RefType {
    /// Function reference type.
    FuncRef,
    /// External reference type.
    ExternRef,
}

/// WebAssembly value types (numeric or reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Val type.
pub enum ValType {
    /// A numeric type ([`NumType`]).
    Num(NumType),
    /// A reference type ([`RefType`]).
    Ref(RefType),
}

/// WebAssembly function signature.
///
/// A function type defines the parameter types and result types of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Function type.
pub struct FunctionType {
    /// Parameter types.
    pub params: Vec<ValType>,
    /// Result types.
    pub results: Vec<ValType>,
}

/// Limits for memory pages or table elements.
///
/// Specifies the minimum and optionally maximum size for resources like
/// memory pages or table element counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Limits.
pub enum Limits {
    /// Minimum only (no maximum).
    Min(u32),
    /// Minimum and maximum bounds.
    MinMax(u32, u32),
}

/// WebAssembly table type.
///
/// Defines the element type and size limits for a table.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Table type.
pub struct TableType {
    /// The element type of the table.
    pub elem_type: RefType,
    /// Whether the table accepts null references.
    pub nullable: bool,
    /// Size limits for the table.
    pub limits: Limits,
}

/// WebAssembly memory type.
///
/// Defines the page size limits for a linear memory.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Memory type.
pub struct MemoryType {
    /// Size limits in pages (64 KiB per page).
    pub limits: Limits,
    /// Whether this memory is shared (atomic operations allowed).
    pub shared: bool,
}

/// WebAssembly global type.
///
/// Defines the type and mutability of a global.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Global type.
pub struct GlobalType {
    /// The value type of the global.
    pub content_type: ValType,
    /// Whether the global is mutable.
    pub mutable: bool,
}

/// A WebAssembly table instance.
///
/// Tables store function references or external references and support
/// dynamic indexing.
#[derive(Debug, Clone, PartialEq)]
/// Table.
pub struct Table {
    /// The table type.
    pub type_: TableType,
    /// The table elements.
    pub data: Vec<WasmValue>,
}

/// A WebAssembly global.
///
/// Globals hold a single value that can be either immutable or mutable.
/// They can be accessed from WebAssembly code and (if mutable) modified.
#[derive(Debug, Clone)]
/// Global.
pub struct Global {
    /// The global type.
    pub type_: GlobalType,
    /// The current value.
    pub value: super::WasmValue,
}

impl RefType {
    /// Decodes this value from its compact byte representation.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x70 => RefType::FuncRef,
            0x6F => RefType::ExternRef,
            _ => RefType::FuncRef,
        }
    }
}

impl ValType {
    /// Returns whether numeric.
    pub fn is_numeric(&self) -> bool {
        matches!(self, ValType::Num(_))
    }

    /// Returns whether reference.
    pub fn is_reference(&self) -> bool {
        matches!(self, ValType::Ref(_))
    }

    /// Returns the numeric type, if this is a numeric value type.
    pub fn as_num_type(&self) -> Option<NumType> {
        match self {
            ValType::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// Returns the reference type, if this is a reference value type.
    pub fn as_ref_type(&self) -> Option<RefType> {
        match self {
            ValType::Ref(r) => Some(*r),
            _ => None,
        }
    }

    /// Returns a stable byte representation suitable for hashing.
    pub fn hash_bytes(&self) -> Vec<u8> {
        match self {
            ValType::Num(nt) => vec![0, *nt as u8],
            ValType::Ref(rt) => vec![1, *rt as u8],
        }
    }
}

impl FunctionType {
    /// Creates a new `FunctionType`.
    pub fn new(params: Vec<ValType>, results: Vec<ValType>) -> Self {
        Self { params, results }
    }

    /// Creates an empty function type.
    pub fn empty() -> Self {
        Self {
            params: Vec::new(),
            results: Vec::new(),
        }
    }
}

impl Limits {
    /// Returns the minimum bound.
    pub fn min(&self) -> u32 {
        match self {
            Limits::Min(min) => *min,
            Limits::MinMax(min, _) => *min,
        }
    }

    /// Returns the maximum bound, if one is present.
    pub fn max(&self) -> Option<u32> {
        match self {
            Limits::Min(_) => None,
            Limits::MinMax(_, max) => Some(*max),
        }
    }

    /// Returns whether this type satisfies the required type.
    pub fn matches_required(&self, required: &Limits) -> bool {
        if self.min() < required.min() {
            return false;
        }

        match (self.max(), required.max()) {
            (_, None) => true,
            (Some(actual), Some(required)) => actual <= required,
            (None, Some(_)) => false,
        }
    }
}

impl TableType {
    /// Creates a new `TableType`.
    pub fn new(elem_type: RefType, limits: Limits) -> Self {
        Self {
            elem_type,
            nullable: true,
            limits,
        }
    }

    /// Creates a new `TableType` with explicit nullability.
    pub fn new_with_nullability(elem_type: RefType, nullable: bool, limits: Limits) -> Self {
        Self {
            elem_type,
            nullable,
            limits,
        }
    }

    /// Returns whether this type satisfies the required type.
    pub fn matches_required(&self, required: &TableType) -> bool {
        self.elem_type == required.elem_type
            && (self.nullable == required.nullable || (!self.nullable && required.nullable))
            && self.limits.matches_required(&required.limits)
    }
}

impl MemoryType {
    /// Creates a new `MemoryType`.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            shared: false,
        }
    }

    /// Returns the WebAssembly page size in bytes.
    pub fn page_size() -> u32 {
        65536
    }

    /// Returns whether this type satisfies the required type.
    pub fn matches_required(&self, required: &MemoryType) -> bool {
        self.limits.matches_required(&required.limits)
    }
}

impl GlobalType {
    /// Creates a new `GlobalType`.
    pub fn new(content_type: ValType, mutable: bool) -> Self {
        Self {
            content_type,
            mutable,
        }
    }
}

impl Table {
    /// Creates a new `Table`.
    pub fn new(type_: TableType) -> Self {
        let size = type_.limits.min() as usize;
        let default = WasmValue::NullRef(type_.elem_type);
        Self {
            type_,
            data: vec![default; size],
        }
    }

    /// Returns the size.
    pub fn size(&self) -> u32 {
        self.data.len() as u32
    }

    /// Returns the value at the given index.
    pub fn get(&self, idx: u32) -> Option<WasmValue> {
        self.data.get(idx as usize).copied()
    }

    /// Sets the current value.
    pub fn set(&mut self, idx: u32, val: WasmValue) -> Result<()> {
        if idx as usize >= self.data.len() {
            return Err(super::WasmError::Trap(super::TrapCode::TableOutOfBounds));
        }
        if val.val_type() != ValType::Ref(self.type_.elem_type) {
            return Err(super::WasmError::Validation(
                "table element type mismatch".to_string(),
            ));
        }
        if !self.type_.nullable && matches!(val, WasmValue::NullRef(_)) {
            return Err(super::WasmError::Validation(
                "table element nullability mismatch".to_string(),
            ));
        }
        self.data[idx as usize] = val;
        Ok(())
    }

    /// Grows the underlying resource by the requested delta.
    pub fn grow(&mut self, delta: u32) -> Result<u32> {
        let old_size = self.size();
        let new_size = old_size.saturating_add(delta);
        if let Some(max) = self.type_.limits.max()
            && new_size > max
        {
            return Err(super::WasmError::Runtime(
                "table size exceeds maximum".to_string(),
            ));
        }
        self.data
            .resize(new_size as usize, WasmValue::NullRef(self.type_.elem_type));
        Ok(old_size)
    }
}

impl Global {
    /// Creates a new `Global`.
    pub fn new(type_: GlobalType, value: super::WasmValue) -> Result<Self> {
        if type_.content_type != value.val_type() {
            return Err(super::WasmError::Validation(
                "global type mismatch".to_string(),
            ));
        }
        Ok(Self { type_, value })
    }

    /// Returns the value at the given index.
    pub fn get(&self) -> super::WasmValue {
        self.value
    }

    /// Sets the current value.
    pub fn set(&mut self, value: super::WasmValue) -> Result<()> {
        if !self.type_.mutable {
            return Err(super::WasmError::Runtime(
                "global is not mutable".to_string(),
            ));
        }
        if self.type_.content_type != value.val_type() {
            return Err(super::WasmError::Validation(
                "value type mismatch".to_string(),
            ));
        }
        self.value = value;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valtype_is_numeric() {
        assert!(ValType::Num(NumType::I32).is_numeric());
        assert!(ValType::Ref(RefType::FuncRef).is_reference());
    }

    #[test]
    fn test_function_type() {
        let ft = FunctionType::new(
            vec![ValType::Num(NumType::I32), ValType::Num(NumType::I64)],
            vec![ValType::Num(NumType::F32)],
        );
        assert_eq!(ft.params.len(), 2);
        assert_eq!(ft.results.len(), 1);
    }

    #[test]
    fn test_limits() {
        let min = Limits::Min(10);
        assert_eq!(min.min(), 10);
        assert_eq!(min.max(), None);

        let minmax = Limits::MinMax(10, 100);
        assert_eq!(minmax.min(), 10);
        assert_eq!(minmax.max(), Some(100));
    }

    #[test]
    fn test_limits_match_required_subtyping() {
        assert!(Limits::MinMax(2, 3).matches_required(&Limits::MinMax(1, 4)));
        assert!(Limits::MinMax(2, 3).matches_required(&Limits::Min(1)));
        assert!(!Limits::MinMax(1, 5).matches_required(&Limits::MinMax(2, 4)));
        assert!(!Limits::Min(2).matches_required(&Limits::MinMax(1, 4)));
    }

    #[test]
    fn test_table() {
        let table_type = TableType::new(RefType::FuncRef, Limits::Min(10));
        let mut table = Table::new(table_type);
        assert_eq!(table.size(), 10);
        assert_eq!(
            table.get(5),
            Some(super::super::WasmValue::NullRef(RefType::FuncRef))
        );
        table.set(5, super::super::WasmValue::FuncRef(42)).unwrap();
        assert_eq!(table.get(5), Some(super::super::WasmValue::FuncRef(42)));
    }

    #[test]
    fn test_global() {
        let global_type = GlobalType::new(ValType::Num(NumType::I32), true);
        let global = Global::new(global_type, super::super::WasmValue::I32(42)).unwrap();
        assert_eq!(global.get(), super::super::WasmValue::I32(42));
    }

    #[test]
    fn test_global_immutable() {
        let global_type = GlobalType::new(ValType::Num(NumType::I32), false);
        let mut global = Global::new(global_type, super::super::WasmValue::I32(42)).unwrap();
        assert!(global.set(super::super::WasmValue::I32(100)).is_err());
    }
}
