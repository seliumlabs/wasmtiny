//! Shared AOT store: the store-wide fat-call-target table and canonical
//! signature-id registry that let `call_indirect` and cross-module imports
//! dispatch across module boundaries.

use std::sync::{Arc, Mutex};

use super::context::{FuncDesc, TableCells};

use crate::runtime::{FunctionType, Global, HostFunc, Memory, Result, TableType, WasmError};

/// A shared AOT store reference — modules instantiated against the same store
/// can alias tables and dispatch `call_indirect` across module boundaries.
pub type SharedAotStore = Arc<Mutex<AotStore>>;
/// A shared AOT table reference, used to alias a table across modules.
pub type SharedAotTable = Arc<Mutex<AotTable>>;

/// Implementation reservation cap for tables without a declared maximum
/// (or with a maximum above this): `table.grow` beyond the reservation fails
/// with `-1`, which the specification permits ("may fail for any reason").
pub const MAX_TABLE_ELEMENTS: u32 = 1 << 20;

/// A raw-cell AOT function table.
pub struct AotTable {
    /// Store-native funcref handles (`0` is the null entry).
    ///
    /// The buffer is capacity-reserved at creation and never reallocated —
    /// only `len` advances within the reservation — so the `base` published
    /// to compiled code stays valid for the table's lifetime. The entire
    /// reservation is initialised to the null handle before execution.
    pub cells: Vec<u32>,
    /// Shared cell state published to compiled code; kept in sync with
    /// `cells` by every mutation path (all of which hold this table's mutex).
    pub holder: TableCells,
    /// The declared table type.
    pub type_: crate::runtime::TableType,
}

/// A value an AOT instantiation can bind to an import.
pub enum AotExtern {
    /// A host-provided function.
    HostFunc(Arc<dyn HostFunc>),
    /// A guest function, referenced by its store-wide native handle.
    Func(u32),
    /// A shared AOT table.
    Table(SharedAotTable),
    /// A shared linear memory.
    Memory(Arc<Mutex<Memory>>),
    /// A global value.
    Global(Global),
}

/// The store-wide state shared by all modules instantiated against it.
pub struct AotStore {
    store_funcs: Vec<FuncDesc>,
    type_registry: Vec<FunctionType>,
}

impl AotTable {
    /// Creates a table with `initial` null entries, reserving storage up to
    /// the declared maximum (or [`MAX_TABLE_ELEMENTS`] when the maximum is
    /// unbounded or larger).
    ///
    /// Reserving upfront is what makes the element base pointer immutable,
    /// which in turn makes growth visible to already-compiled code without
    /// any relocation.
    pub fn with_initial(type_: TableType, initial: u32) -> Result<Self> {
        let reservation = type_
            .limits
            .max()
            .unwrap_or(MAX_TABLE_ELEMENTS)
            .clamp(initial, MAX_TABLE_ELEMENTS);
        let mut cells = Vec::new();
        cells.try_reserve_exact(reservation as usize).map_err(|_| {
            WasmError::Instantiate(format!("cannot reserve {reservation} table entries"))
        })?;
        // Zero the whole reservation so a racy bound read can never expose
        // uninitialised memory to compiled code (see `TableCells`).
        cells.resize(reservation as usize, 0);
        cells.truncate(initial as usize);
        Ok(Self {
            holder: TableCells {
                base: cells.as_ptr() as *mut u8,
                len: initial,
                _pad: 0,
            },
            cells,
            type_,
        })
    }
}

// SAFETY: the only raw pointer in `AotTable` is `holder.base`, which points
// at `cells`' own backing buffer for the table's whole lifetime (never
// reallocated; see `with_initial`). The `Arc<Mutex<AotTable>>` sharing
// pattern is exactly the designed cross-instance contract: compiled code
// on any thread reads `holder` (see the `TableCells` concurrency contract),
// and every mutation path holds the table's mutex.
unsafe impl Send for AotTable {}

unsafe impl Sync for AotTable {}

impl AotStore {
    /// Creates an empty store.
    ///
    /// Handle `0` is reserved as the null entry, so table cells can use `0` as
    /// the null sentinel without colliding with a real function handle.
    pub fn new() -> Self {
        Self {
            store_funcs: vec![FuncDesc {
                entry: std::ptr::null(),
                vmctx: std::ptr::null(),
                type_id: 0,
                _pad: 0,
            }],
            type_registry: Vec::new(),
        }
    }

    /// Creates an empty shared store.
    pub fn shared() -> SharedAotStore {
        Arc::new(Mutex::new(Self::new()))
    }

    /// Returns the canonical signature id for `ty`, registering it lazily.
    pub(crate) fn canonical_type_id(&mut self, ty: &FunctionType) -> u32 {
        if let Some(position) = self.type_registry.iter().position(|t| t == ty) {
            return position as u32;
        }
        let id = self.type_registry.len() as u32;
        self.type_registry.push(ty.clone());
        id
    }

    /// Registers a fat call target and returns its store-native handle.
    pub(crate) fn push_func(&mut self, desc: FuncDesc) -> u32 {
        let handle = self.store_funcs.len() as u32;
        self.store_funcs.push(desc);
        handle
    }

    /// Returns the descriptor for a previously issued handle.
    pub(crate) fn func_desc(&self, handle: u32) -> Option<&FuncDesc> {
        self.store_funcs.get(handle as usize)
    }

    /// The store-wide descriptor table pointer (`vmctx.store_funcs`).
    pub(crate) fn funcs_ptr(&self) -> *const FuncDesc {
        if self.store_funcs.is_empty() {
            std::ptr::NonNull::<FuncDesc>::dangling().as_ptr()
        } else {
            self.store_funcs.as_ptr()
        }
    }
}

impl Default for AotStore {
    fn default() -> Self {
        Self::new()
    }
}
