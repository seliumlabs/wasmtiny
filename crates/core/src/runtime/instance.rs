use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use super::{
    ExportKind, FunctionType, Global, ImportKind, InstanceMeter, InstanceStats, Memory, Module,
    RefType, Result, SharedMemoryRegistry, SharedRegionId, Table, TrapCode, ValType, WasmError,
    WasmValue,
};
use parking_lot::Mutex as ParkingMutex;

use crate::{loader::BinaryReader, memory::RegionProt};

/// Type alias for a shared (thread-safe) global reference.
pub type SharedGlobal = Arc<Mutex<Global>>;
/// Type alias for a shared (thread-safe) memory reference.
pub type SharedMemory = Arc<Mutex<Memory>>;
/// Type alias for a shared (thread-safe) table reference.
pub type SharedTable = Arc<Mutex<Table>>;

/// Trait for host-provided functions callable from WebAssembly.
///
/// Implement this trait to create host functions that can be imported into
/// WebAssembly modules.
///
/// # Example
///
/// ```
/// use wasmtiny::runtime::{WasmValue, Result, FunctionType, ValType, NumType, HostCaller, HostFunc};
///
/// struct Add;
///
/// impl HostFunc for Add {
///     fn call(&self, _caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
///         let a = args[0].i32()?;
///         let b = args[1].i32()?;
///         Ok(vec![WasmValue::I32(a + b)])
///     }
///
///     fn function_type(&self) -> Option<&FunctionType> {
///         static TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
///         Some(TYPE.get_or_init(|| FunctionType::new(
///             vec![ValType::Num(NumType::I32), ValType::Num(NumType::I32)],
///             vec![ValType::Num(NumType::I32)]
///         )))
///     }
/// }
/// ```
pub trait HostFunc: Send + Sync + 'static {
    /// Executes the host function synchronously.
    fn call(&self, caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>>;

    /// Returns the declared function signature for this host function.
    fn function_type(&self) -> Option<&FunctionType>;
}

struct TypedHostFunc {
    inner: Arc<dyn HostFunc>,
    func_type: FunctionType,
}

#[derive(Clone)]
pub(crate) struct GuestFuncTarget {
    pub module: Arc<Module>,
    pub func_idx: u32,
}

/// A WebAssembly instance.
///
/// An instance is an instantiated module with runtime state including memories,
/// tables, globals, and exported functions.
pub struct Instance {
    module: Arc<Module>,
    store: Arc<Mutex<Store>>,
    shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
    /// The memory values.
    pub memories: Vec<SharedMemory>,
    /// The table values.
    pub tables: Vec<SharedTable>,
    /// The global values.
    pub globals: Vec<SharedGlobal>,
    /// Shared region IDs currently attached to this instance's memory.
    attached_regions: Vec<SharedRegionId>,
    funcs: Vec<Arc<dyn HostFunc>>,
    exports: HashMap<String, Extern>,
    import_bindings: Vec<Option<Extern>>,
    elem_segments_active: Vec<bool>,
    data_segments_active: Vec<bool>,
    func_ref_handles: Vec<u32>,
    /// Per-instance instruction meter and configurable budgets. Shared (an
    /// `Arc`) so the interpreter can charge it without holding the instance
    /// lock, and reused across `invoke_function` calls via the cached
    /// instance so counts accumulate over the instance's lifetime.
    meter: Arc<InstanceMeter>,
}

/// The WebAssembly store.
///
/// A store holds all runtime state including instantiated instances and
/// registered native (host) functions. It is shared among instances to enable
/// inter-module communication.
#[derive(Default)]
/// Shared runtime store for instances and host resources.
pub struct Store {
    /// Instances currently owned by this store.
    pub instances: Vec<Instance>,
    native_funcs: Vec<(Arc<dyn HostFunc>, FunctionType, Option<GuestFuncTarget>)>,
    shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
}

/// Context for a host function invocation, including the calling instance.
pub struct HostCaller<'a> {
    store: &'a mut Store,
    memories: &'a [SharedMemory],
}

/// An external value that can be imported or exported.
///
/// Represents a function, table, memory, or global that can be passed between
/// the host and WebAssembly.
#[derive(Clone)]
/// A host or guest value exposed through imports and exports.
pub enum Extern {
    /// A guest-exported function binding.
    Func(GuestFuncBinding),
    /// A host-provided function.
    HostFunc(Arc<dyn HostFunc>),
    /// A shared table value.
    Table(SharedTable),
    /// A shared linear memory value.
    Memory(SharedMemory),
    /// A shared global value.
    Global(SharedGlobal),
}

#[derive(Clone)]
pub struct GuestFuncBinding {
    pub module: Arc<Module>,
    pub imports: Vec<(String, String, Extern)>,
    pub func_idx: u32,
    pub func_type: FunctionType,
}

struct GuestFuncRefHost {
    module: Arc<Module>,
    imports: Vec<(String, String, Extern)>,
    func_idx: u32,
    func_type: FunctionType,
}

impl TypedHostFunc {
    fn new(inner: Arc<dyn HostFunc>, func_type: FunctionType) -> Self {
        Self { inner, func_type }
    }
}

impl HostFunc for TypedHostFunc {
    fn call(&self, caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        self.inner.call(caller, args)
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(&self.func_type)
    }
}

impl Instance {
    /// Returns the underlying module.
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// Returns the shared memory registry (crate-internal).
    #[allow(dead_code)]
    pub(crate) fn shared_memory_registry(&self) -> Arc<ParkingMutex<SharedMemoryRegistry>> {
        self.shared_memory.clone()
    }

    /// Creates a new `Instance`.
    pub fn new(module: Arc<Module>) -> Result<Self> {
        Self::new_with_store(module, Arc::new(Mutex::new(Store::new())))
    }

    /// Creates a new instance backed by the provided store.
    pub fn new_with_store(module: Arc<Module>, store: Arc<Mutex<Store>>) -> Result<Self> {
        let shared_memory = store
            .lock()
            .map_err(poisoned_lock)?
            .shared_memory_registry();
        let mut instance = Self::empty(module, store, shared_memory);
        instance.instantiate_host_funcs();
        instance.validate_imports_satisfied()?;
        instance.instantiate_func_refs()?;
        instance.instantiate_defined_state()?;
        instance.instantiate_exports();
        Ok(instance)
    }

    /// Returns this value configured with imports.
    pub fn with_imports(module: Arc<Module>, imports: &[(&str, &str, Extern)]) -> Result<Self> {
        Self::with_imports_and_store(module, imports, Arc::new(Mutex::new(Store::new())))
    }

    /// Returns this value configured with imports and store.
    pub fn with_imports_and_store(
        module: Arc<Module>,
        imports: &[(&str, &str, Extern)],
        store: Arc<Mutex<Store>>,
    ) -> Result<Self> {
        let shared_memory = store
            .lock()
            .map_err(poisoned_lock)?
            .shared_memory_registry();
        let mut instance = Self::empty(module, store, shared_memory);
        instance.instantiate_host_funcs();
        let mut used = vec![false; imports.len()];
        let import_decls = instance.module.imports.clone();
        for (import_idx, import) in import_decls.iter().enumerate() {
            if let Some((provided_idx, (_, _, extern_))) =
                imports
                    .iter()
                    .enumerate()
                    .find(|(provided_idx, (module_name, name, _))| {
                        !used[*provided_idx]
                            && *module_name == import.module.as_str()
                            && *name == import.name.as_str()
                    })
            {
                used[provided_idx] = true;
                instance.add_import_at(import_idx, extern_)?;
            }
        }
        instance.validate_imports_satisfied()?;
        instance.instantiate_func_refs()?;
        instance.instantiate_defined_state()?;
        instance.instantiate_exports();
        Ok(instance)
    }

    fn empty(
        module: Arc<Module>,
        store: Arc<Mutex<Store>>,
        shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
    ) -> Self {
        let import_count = module.imports.len();
        let elem_segments_active = module
            .elems
            .iter()
            .map(|segment| matches!(segment.kind, super::ElemKind::Passive))
            .collect();
        // Passive data segments are available to `memory.init` until dropped.
        // Active segments are applied at instantiation and count as dropped.
        let data_segments_active = module
            .data
            .iter()
            .map(|segment| matches!(segment.kind, super::DataKind::Passive))
            .collect();
        Self {
            module,
            store,
            shared_memory,
            memories: Vec::new(),
            tables: Vec::new(),
            globals: Vec::new(),
            attached_regions: Vec::new(),
            funcs: Vec::new(),
            exports: HashMap::new(),
            import_bindings: vec![None; import_count],
            elem_segments_active,
            data_segments_active,
            func_ref_handles: Vec::new(),
            meter: Arc::new(InstanceMeter::new()),
        }
    }

    fn instantiate_host_funcs(&mut self) {
        let import_func_count = self
            .module
            .imports
            .iter()
            .filter(|i| matches!(i.kind, ImportKind::Func(_)))
            .count();

        self.funcs = (0..import_func_count)
            .map(|_| {
                Arc::new(|_: &mut HostCaller<'_>, _: &[WasmValue]| {
                    Err(WasmError::Runtime(
                        "uninitialized host function".to_string(),
                    ))
                }) as Arc<dyn HostFunc>
            })
            .collect();
    }

    fn instantiate_func_refs(&mut self) -> Result<()> {
        let imports = self.ordered_imports();
        self.func_ref_handles.clear();

        for func_idx in 0..self.module.func_count() {
            let Some(func_type) = self.module.func_type(func_idx).cloned() else {
                self.func_ref_handles.push(0);
                continue;
            };
            let native_idx = if func_idx < self.funcs.len() as u32 {
                let host = self.funcs.get(func_idx as usize).cloned().ok_or_else(|| {
                    WasmError::Instantiate(format!("function {} not found", func_idx))
                })?;
                self.store
                    .lock()
                    .map_err(poisoned_lock)?
                    .register_internal_native(
                        Box::new(TypedHostFunc::new(host, func_type.clone())),
                        func_type.clone(),
                    )
            } else {
                self.store
                    .lock()
                    .map_err(poisoned_lock)?
                    .register_internal_guest_native(
                        Box::new(GuestFuncRefHost {
                            module: self.module.clone(),
                            imports: imports.clone(),
                            func_idx,
                            func_type: func_type.clone(),
                        }),
                        func_type.clone(),
                        self.module.clone(),
                        func_idx,
                    )
            };
            self.func_ref_handles.push(native_idx);
        }

        Ok(())
    }

    fn instantiate_defined_state(&mut self) -> Result<()> {
        let mut const_globals = self.const_expr_globals();

        for memory_type in &self.module.memories {
            let memory = Memory::try_new(memory_type.clone())?;
            self.memories.push(Arc::new(Mutex::new(memory)));
        }

        for table_type in &self.module.tables {
            self.tables
                .push(Arc::new(Mutex::new(Table::new(table_type.clone()))));
        }

        for (index, global_type) in self.module.globals.iter().enumerate() {
            let init = self.module.global_inits.get(index).ok_or_else(|| {
                WasmError::Instantiate(format!("missing init for global {}", index))
            })?;
            let value = evaluate_const_expr(init, &const_globals, &self.func_ref_handles)?;
            let global = Arc::new(Mutex::new(Global::new(global_type.clone(), value)?));
            self.globals.push(global.clone());
            const_globals.push((!global_type.mutable).then_some(global));
        }

        self.initialise_data_segments(&const_globals)?;
        self.initialise_elem_segments(&const_globals)?;

        Ok(())
    }

    fn const_expr_globals(&self) -> Vec<Option<SharedGlobal>> {
        let mut globals = Vec::new();
        let mut imported_global_idx = 0usize;

        for import in &self.module.imports {
            if let ImportKind::Global(global_type) = &import.kind {
                let global = self.globals.get(imported_global_idx).cloned();
                imported_global_idx += 1;

                if global_type.mutable {
                    globals.push(None);
                } else {
                    globals.push(global);
                }
            }
        }

        globals
    }

    fn ordered_imports(&self) -> Vec<(String, String, Extern)> {
        self.module
            .imports
            .iter()
            .zip(self.import_bindings.iter())
            .filter_map(|(import, binding)| {
                binding
                    .as_ref()
                    .map(|extern_| (import.module.clone(), import.name.clone(), extern_.clone()))
            })
            .collect()
    }

    pub(crate) fn func_ref_handle(&self, func_idx: u32) -> Result<u32> {
        self.func_ref_handles
            .get(func_idx as usize)
            .copied()
            .ok_or_else(|| WasmError::Runtime(format!("function ref {} not found", func_idx)))
    }

    pub(crate) fn native_func_ref_parts(
        &self,
        native_idx: u32,
    ) -> Result<(Arc<dyn HostFunc>, FunctionType, Option<GuestFuncTarget>)> {
        let store = self.store.lock().map_err(poisoned_lock)?;
        store
            .get_native_func(native_idx)
            .map(|(func, func_type, target)| (func.clone(), func_type.clone(), target.cloned()))
            .ok_or_else(|| WasmError::Runtime(format!("native function {} not found", native_idx)))
    }

    pub(crate) fn call_cloned_host_func(
        &self,
        func: Arc<dyn HostFunc>,
        args: &[WasmValue],
    ) -> Result<Vec<WasmValue>> {
        let mut store = self.store.lock().map_err(poisoned_lock)?;
        let mut caller = HostCaller::new(&mut store, &self.memories);
        func.call(&mut caller, args)
    }

    fn validate_imports_satisfied(&self) -> Result<()> {
        for (index, (import, binding)) in self
            .module
            .imports
            .iter()
            .zip(self.import_bindings.iter())
            .enumerate()
        {
            if binding.is_none() {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} at index {} is not satisfied",
                    import.module, import.name, index
                )));
            }
        }

        Ok(())
    }

    fn initialise_data_segments(&mut self, const_globals: &[Option<SharedGlobal>]) -> Result<()> {
        for segment in &self.module.data {
            let super::DataKind::Active { memory_idx, offset } = &segment.kind else {
                continue;
            };
            let offset = evaluate_const_expr(offset, const_globals, &self.func_ref_handles)?;
            let WasmValue::I32(offset) = offset else {
                return Err(WasmError::Instantiate(
                    "data segment offset must evaluate to i32".to_string(),
                ));
            };

            let memory = self.memories.get_mut(*memory_idx as usize).ok_or_else(|| {
                WasmError::Instantiate(format!("memory {} not found", memory_idx))
            })?;
            memory
                .lock()
                .map_err(poisoned_lock)?
                .write(offset as u32, &segment.init)?;
        }

        Ok(())
    }

    fn initialise_elem_segments(&mut self, const_globals: &[Option<SharedGlobal>]) -> Result<()> {
        for segment in &self.module.elems {
            let super::ElemKind::Active { table_idx, offset } = &segment.kind else {
                continue;
            };
            let offset = evaluate_const_expr(offset, const_globals, &self.func_ref_handles)?;
            let WasmValue::I32(offset) = offset else {
                return Err(WasmError::Instantiate(
                    "element segment offset must evaluate to i32".to_string(),
                ));
            };

            let table = self
                .tables
                .get_mut(*table_idx as usize)
                .ok_or_else(|| WasmError::Instantiate(format!("table {} not found", table_idx)))?;

            let segment_len = segment.init.len() as u32;
            let offset_u32 =
                u32::try_from(offset).map_err(|_| WasmError::Trap(TrapCode::TableOutOfBounds))?;
            let table_size = table.lock().map_err(poisoned_lock)?.size();
            let end = offset_u32
                .checked_add(segment_len)
                .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
            if end > table_size || (segment_len == 0 && offset_u32 > table_size) {
                return Err(WasmError::Trap(TrapCode::TableOutOfBounds));
            }

            for (index, expr) in segment.init.iter().enumerate() {
                let value = evaluate_const_expr(expr, const_globals, &self.func_ref_handles)?;
                table
                    .lock()
                    .map_err(poisoned_lock)?
                    .set(offset_u32 + index as u32, value)?;
            }
        }

        Ok(())
    }

    fn instantiate_exports(&mut self) {
        for export in &self.module.exports {
            let extern_ = match export.kind {
                ExportKind::Func(idx) => self.module.func_type(idx).cloned().map(|func_type| {
                    Extern::Func(GuestFuncBinding {
                        module: Arc::new(self.module.as_ref().clone()),
                        imports: self.ordered_imports(),
                        func_idx: idx,
                        func_type,
                    })
                }),
                ExportKind::Table(idx) => self.tables.get(idx as usize).cloned().map(Extern::Table),
                ExportKind::Memory(idx) => {
                    self.memories.get(idx as usize).cloned().map(Extern::Memory)
                }
                ExportKind::Global(idx) => {
                    self.globals.get(idx as usize).cloned().map(Extern::Global)
                }
                ExportKind::Tag(_) => None,
            };

            if let Some(extern_) = extern_ {
                self.exports.insert(export.name.clone(), extern_);
            }
        }
    }

    pub fn table_init(
        &mut self,
        table_idx: u32,
        elem_idx: u32,
        dst: u32,
        src: u32,
        len: u32,
    ) -> Result<()> {
        let segment =
            self.module.elems.get(elem_idx as usize).ok_or_else(|| {
                WasmError::Runtime(format!("element segment {} not found", elem_idx))
            })?;
        let available = self
            .elem_segments_active
            .get(elem_idx as usize)
            .copied()
            .unwrap_or(false);
        let segment_len = if available {
            segment.init.len() as u32
        } else {
            0
        };

        let src_end = src
            .checked_add(len)
            .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
        let dst_end = dst
            .checked_add(len)
            .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
        if src_end > segment_len {
            return Err(WasmError::Trap(TrapCode::TableOutOfBounds));
        }

        let const_globals = self.all_const_expr_globals();
        let table = self
            .tables
            .get(table_idx as usize)
            .ok_or_else(|| WasmError::Runtime(format!("table {} not found", table_idx)))?
            .clone();
        let table_size = table.lock().map_err(poisoned_lock)?.size();
        if dst_end > table_size {
            return Err(WasmError::Trap(TrapCode::TableOutOfBounds));
        }

        for offset in 0..len {
            let expr = &segment.init[(src + offset) as usize];
            let value = evaluate_const_expr(expr, &const_globals, &self.func_ref_handles)?;
            table
                .lock()
                .map_err(poisoned_lock)?
                .set(dst + offset, value)?;
        }

        Ok(())
    }

    pub fn elem_drop(&mut self, elem_idx: u32) -> Result<()> {
        let active = self
            .elem_segments_active
            .get_mut(elem_idx as usize)
            .ok_or_else(|| WasmError::Runtime(format!("element segment {} not found", elem_idx)))?;
        *active = false;
        Ok(())
    }

    /// Applies `memory.init`: copies `len` bytes from a passive data segment
    /// into memory at `dst`, trapping on out-of-bounds access.
    pub fn memory_init(
        &mut self,
        data_idx: u32,
        memory_idx: u32,
        dst: u32,
        src: u32,
        len: u32,
    ) -> Result<()> {
        let segment =
            self.module.data.get(data_idx as usize).ok_or_else(|| {
                WasmError::Runtime(format!("data segment {} not found", data_idx))
            })?;
        let available = self
            .data_segments_active
            .get(data_idx as usize)
            .copied()
            .unwrap_or(false);
        let segment_len = if available {
            segment.init.len() as u32
        } else {
            0
        };

        let src_end = src
            .checked_add(len)
            .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;
        if src_end > segment_len {
            return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
        }

        let bytes = if available {
            segment.init[src as usize..src_end as usize].to_vec()
        } else {
            Vec::new()
        };
        let memory = self
            .memories
            .get(memory_idx as usize)
            .ok_or_else(|| WasmError::Runtime(format!("memory {} not found", memory_idx)))?
            .clone();
        memory.lock().map_err(poisoned_lock)?.write(dst, &bytes)
    }

    /// Drops a passive data segment (`data.drop`); subsequent `memory.init`
    /// calls treat it as empty.
    pub fn data_drop(&mut self, data_idx: u32) -> Result<()> {
        let active = self
            .data_segments_active
            .get_mut(data_idx as usize)
            .ok_or_else(|| WasmError::Runtime(format!("data segment {} not found", data_idx)))?;
        *active = false;
        Ok(())
    }

    fn all_const_expr_globals(&self) -> Vec<Option<SharedGlobal>> {
        let total_globals = self
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.kind, ImportKind::Global(_)))
            .count()
            + self.module.globals.len();

        (0..total_globals)
            .map(|idx| {
                self.module.global_at(idx as u32).and_then(|global_type| {
                    if global_type.mutable {
                        None
                    } else {
                        self.globals.get(idx).cloned()
                    }
                })
            })
            .collect()
    }

    /// Adds import.
    pub fn add_import(&mut self, module_name: &str, name: &str, extern_: &Extern) -> Result<()> {
        let matching_indices = self
            .module
            .imports
            .iter()
            .enumerate()
            .filter(|(_, import)| import.module == module_name && import.name == name)
            .map(|(idx, _)| idx)
            .collect::<Vec<_>>();

        if matching_indices.is_empty() {
            return Err(WasmError::Instantiate(format!(
                "import {}.{} not found",
                module_name, name
            )));
        }

        let unresolved = matching_indices
            .into_iter()
            .filter(|idx| self.import_bindings[*idx].is_none())
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return Err(WasmError::Instantiate(format!(
                "import {}.{} already registered",
                module_name, name
            )));
        }

        let mut last_error = None;
        for import_idx in unresolved {
            match self.add_import_at(import_idx, extern_) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            WasmError::Instantiate(format!("import {}.{} kind mismatch", module_name, name))
        }))
    }

    /// Returns func.
    pub fn get_func(&self, idx: u32) -> Option<&dyn HostFunc> {
        self.funcs.get(idx as usize).map(|f| f.as_ref())
    }

    /// Returns the memory at the given index.
    pub fn memory(&self, idx: u32) -> Option<&SharedMemory> {
        self.memories.get(idx as usize)
    }

    /// Allocates a shared region and maps it into this instance's memory.
    ///
    /// Returns `(region_id, page_offset)`.
    pub fn allocate_shared_region(
        &mut self,
        size: u32,
        prot: RegionProt,
    ) -> Result<(SharedRegionId, u32)> {
        let memory = self
            .memories
            .first()
            .ok_or_else(|| WasmError::Runtime("no memory to attach shared region to".to_string()))?
            .clone();
        let mut mem = memory.lock().map_err(poisoned_lock)?;
        let result = self
            .shared_memory
            .lock()
            .allocate_region(&mut mem, size, prot)?;
        self.attached_regions.push(result.0);
        Ok(result)
    }

    /// Allocates a shared region without mapping it into any guest memory.
    pub fn allocate_shared_region_standalone(&mut self, size: u32) -> Result<SharedRegionId> {
        self.shared_memory.lock().allocate_region_standalone(size)
    }

    /// Destroys shared region.
    pub fn destroy_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        self.shared_memory.lock().destroy_region(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn shared_region_len(&self, region_id: SharedRegionId) -> Result<u32> {
        self.shared_memory.lock().region_len(region_id)
    }

    /// Attaches an existing shared region to this instance's memory.
    ///
    /// Returns the page offset where the region was mapped.
    pub fn attach_shared_region(
        &mut self,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
    ) -> Result<u32> {
        let memory = self
            .memories
            .first()
            .ok_or_else(|| WasmError::Runtime("no memory to attach shared region to".to_string()))?
            .clone();
        let mut mem = memory.lock().map_err(poisoned_lock)?;
        let page_offset =
            self.shared_memory
                .lock()
                .attach_region(&mut mem, region_id, prot, reader_slot)?;
        self.attached_regions.push(region_id);
        Ok(page_offset)
    }

    /// Detaches a shared region from this instance's memory.
    pub fn detach_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        if !self.attached_regions.contains(&region_id) {
            return Err(WasmError::Runtime(format!(
                "shared region {} is not attached to this instance",
                region_id.raw()
            )));
        }
        let memory = self
            .memories
            .first()
            .ok_or_else(|| {
                WasmError::Runtime("no memory to detach shared region from".to_string())
            })?
            .clone();
        let mut mem = memory.lock().map_err(poisoned_lock)?;
        self.shared_memory
            .lock()
            .detach_region(&mut mem, region_id)?;
        self.attached_regions.retain(|id| *id != region_id);
        Ok(())
    }

    /// Writes data to a shared region from the host side.
    pub fn write_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        self.shared_memory
            .lock()
            .write_to_region(region_id, offset, data)
    }

    /// Reads data from a shared region from the host side.
    pub fn read_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        self.shared_memory
            .lock()
            .read_from_region(region_id, offset, buf)
    }

    /// Grows the selected memory by the requested number of pages.
    pub fn grow_memory(&mut self, idx: u32, delta: u32) -> Result<u32> {
        let memory = self
            .memory(idx)
            .cloned()
            .ok_or_else(|| WasmError::Runtime(format!("memory {} out of bounds", idx)))?;
        let current_pages = self.total_memory_pages()?;
        let new_total = current_pages
            .checked_add(delta)
            .ok_or_else(|| WasmError::Runtime("memory size exceeds maximum allowed".to_string()))?;
        // Enforce the memory budget before the underlying grow extends the
        // accessible range; the budget failure is a distinct trap.
        self.meter.ensure_memory_pages(new_total)?;

        memory.lock().map_err(poisoned_lock)?.grow(delta)
    }

    /// Returns or updates memory grow wasm.
    pub fn memory_grow_wasm(&mut self, idx: u32, delta: i32) -> Result<i32> {
        let memory = self
            .memory(idx)
            .cloned()
            .ok_or_else(|| WasmError::Runtime(format!("memory {} out of bounds", idx)))?;
        let Ok(delta) = u32::try_from(delta) else {
            return Ok(-1);
        };

        let current_pages = self.total_memory_pages()?;
        if current_pages.checked_add(delta).is_none() {
            return Ok(-1);
        };

        // Enforce the memory budget before the underlying grow extends the
        // accessible range. Unlike the module's own declared-maximum failure
        // (which the wasm-level grow reports as -1), a budget overrun is a
        // distinct trap.
        self.meter.ensure_memory_pages(current_pages + delta)?;

        match memory.lock().map_err(poisoned_lock)?.grow(delta) {
            Ok(old_size) => Ok(old_size as i32),
            Err(WasmError::Runtime(_)) | Err(WasmError::Trap(TrapCode::MemoryLimitExceeded)) => {
                Ok(-1)
            }
            Err(error) => Err(error),
        }
    }

    /// Wait32 - atomically loads a 32-bit value and waits if it equals the expected value.
    ///
    /// Returns: 0 = woken, 1 = not equal, 2 = not woken (timeout)
    pub fn wait32(&self, address: u32, expected: i64, timeout: i64) -> Result<i32> {
        let memory = self
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?;

        let memory = memory.lock().map_err(poisoned_lock)?;

        // read_i32 uses ptr_at which checks owned OR shared ranges
        let actual = memory.read_i32(address)? as i64;
        if actual != expected {
            return Ok(1);
        }

        memory.get_waiter(address);
        drop(memory);

        // Nanosecond timeout: negative means wait forever
        let timeout_ns = if timeout < 0 {
            u64::MAX
        } else {
            (timeout as u64).saturating_mul(1)
        };

        let woken = self
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?
            .lock()
            .map_err(poisoned_lock)?
            .wait_on(address, timeout_ns);

        if woken { Ok(0) } else { Ok(2) }
    }

    /// Wait64 - atomically loads a 64-bit value and waits if it equals the expected value.
    ///
    /// Returns: 0 = woken, 1 = not equal, 2 = not woken (timeout)
    pub fn wait64(&self, address: u32, expected: i64, timeout: i64) -> Result<i32> {
        let memory = self
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?;

        let memory = memory.lock().map_err(poisoned_lock)?;

        // read_i64 uses ptr_at which checks owned OR shared ranges
        let actual = memory.read_i64(address)?;
        if actual != expected {
            return Ok(1);
        }

        memory.get_waiter(address);
        drop(memory);

        // Nanosecond timeout: negative means wait forever
        let timeout_ns = if timeout < 0 {
            u64::MAX
        } else {
            (timeout as u64).saturating_mul(1)
        };

        let woken = self
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?
            .lock()
            .map_err(poisoned_lock)?
            .wait_on(address, timeout_ns);

        if woken { Ok(0) } else { Ok(2) }
    }

    /// Returns or updates memory mut.
    pub fn memory_mut(&mut self, idx: u32) -> Option<&mut SharedMemory> {
        self.memories.get_mut(idx as usize)
    }

    /// Returns the table at the given index.
    pub fn table(&self, idx: u32) -> Option<&SharedTable> {
        self.tables.get(idx as usize)
    }

    /// Returns or updates table mut.
    pub fn table_mut(&mut self, idx: u32) -> Option<&mut SharedTable> {
        self.tables.get_mut(idx as usize)
    }

    /// Returns the global at the given index.
    pub fn global(&self, idx: u32) -> Option<&SharedGlobal> {
        self.globals.get(idx as usize)
    }

    /// Returns the global at the given index mutably.
    pub fn global_mut(&mut self, idx: u32) -> Option<&mut SharedGlobal> {
        self.globals.get_mut(idx as usize)
    }

    /// Invokes the target function.
    pub fn call(&mut self, func_idx: u32, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        if let Some(func) = self.get_func(func_idx) {
            let func_type = func.function_type().ok_or_else(|| {
                WasmError::Runtime(format!("function {} type not found", func_idx))
            })?;
            validate_values(args, &func_type.params, "argument")?;
            let mut store = self.store.lock().map_err(poisoned_lock)?;
            let mut caller = HostCaller::new(&mut store, &self.memories);
            let results = func.call(&mut caller, args)?;
            validate_values(&results, &func_type.results, "result")?;
            Ok(results)
        } else {
            Err(WasmError::Runtime(format!(
                "function {} not found",
                func_idx
            )))
        }
    }

    /// Returns the export with the given name, if present.
    pub fn export(&self, name: &str) -> Option<&Extern> {
        self.exports.get(name)
    }

    /// Adds export.
    pub fn add_export(&mut self, name: String, extern_: Extern) {
        self.exports.insert(name, extern_);
    }

    fn add_import_at(&mut self, import_idx: usize, extern_: &Extern) -> Result<()> {
        let import = self.module.imports.get(import_idx).ok_or_else(|| {
            WasmError::Instantiate(format!("import index {} out of bounds", import_idx))
        })?;

        match (&import.kind, extern_) {
            (ImportKind::Func(type_idx), Extern::HostFunc(func)) => {
                let expected = self.module.type_at(*type_idx).ok_or_else(|| {
                    WasmError::Instantiate(format!("type {} not found", type_idx))
                })?;
                if let Some(actual) = func.function_type()
                    && actual != expected
                {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} function type mismatch",
                        import.module, import.name
                    )));
                }
            }
            (ImportKind::Func(type_idx), Extern::Func(func)) => {
                let expected = self.module.type_at(*type_idx).ok_or_else(|| {
                    WasmError::Instantiate(format!("type {} not found", type_idx))
                })?;
                if &func.func_type != expected {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} function type mismatch",
                        import.module, import.name
                    )));
                }
            }
            (ImportKind::Table(expected), Extern::Table(table)) => {
                let table_guard = table.lock().map_err(poisoned_lock)?;
                if !table_matches_required(&table_guard, expected) {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} table type mismatch",
                        import.module, import.name
                    )));
                }
            }
            (ImportKind::Memory(expected), Extern::Memory(memory)) => {
                let memory = memory.lock().map_err(poisoned_lock)?;
                if !memory_matches_required(&memory, expected) {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} memory type mismatch",
                        import.module, import.name
                    )));
                }
            }
            (ImportKind::Global(expected), Extern::Global(global)) => {
                if global.lock().map_err(poisoned_lock)?.type_ != *expected {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} global type mismatch",
                        import.module, import.name
                    )));
                }
            }
            _ => {
                return Err(WasmError::Instantiate(format!(
                    "import {}.{} kind mismatch",
                    import.module, import.name
                )));
            }
        }

        self.import_bindings[import_idx] = Some(extern_.clone());
        self.sync_import_bindings()
    }

    fn sync_import_bindings(&mut self) -> Result<()> {
        self.instantiate_host_funcs();
        self.tables.clear();
        self.memories.clear();
        self.globals.clear();

        for (import_idx, import) in self.module.imports.iter().enumerate() {
            let Some(binding) = self.import_bindings[import_idx].as_ref() else {
                continue;
            };

            match (&import.kind, binding) {
                (ImportKind::Func(type_idx), Extern::HostFunc(func)) => {
                    let expected = self
                        .module
                        .type_at(*type_idx)
                        .ok_or_else(|| {
                            WasmError::Instantiate(format!("type {} not found", type_idx))
                        })?
                        .clone();
                    let slot = self.func_import_slot(import_idx);
                    self.funcs[slot] = Arc::new(TypedHostFunc::new(func.clone(), expected));
                }
                (ImportKind::Func(type_idx), Extern::Func(func)) => {
                    let expected = self
                        .module
                        .type_at(*type_idx)
                        .ok_or_else(|| {
                            WasmError::Instantiate(format!("type {} not found", type_idx))
                        })?
                        .clone();
                    let slot = self.func_import_slot(import_idx);
                    self.funcs[slot] =
                        Arc::new(TypedHostFunc::new(func.clone().into_host_func(), expected));
                }
                (ImportKind::Table(_), Extern::Table(table)) => self.tables.push(table.clone()),
                (ImportKind::Memory(_), Extern::Memory(memory)) => {
                    self.memories.push(memory.clone())
                }
                (ImportKind::Global(_), Extern::Global(global)) => {
                    self.globals.push(global.clone())
                }
                _ => {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} kind mismatch",
                        import.module, import.name
                    )));
                }
            }
        }

        Ok(())
    }

    fn func_import_slot(&self, import_idx: usize) -> usize {
        self.module.imports[..import_idx]
            .iter()
            .filter(|import| matches!(import.kind, ImportKind::Func(_)))
            .count()
    }

    pub(crate) fn total_memory_pages(&self) -> Result<u32> {
        self.memories.iter().try_fold(0u32, |acc, memory| {
            let pages = memory.lock().map_err(poisoned_lock)?.size();
            acc.checked_add(pages)
                .ok_or_else(|| WasmError::Runtime("memory page count overflowed".to_string()))
        })
    }

    /// Returns the list of attached region IDs.
    pub fn attached_regions(&self) -> &[SharedRegionId] {
        &self.attached_regions
    }

    /// Returns the per-instance meter (crate-internal).
    ///
    /// The embedder-facing query/set API is [`stats`](Self::stats),
    /// [`set_execution_budget`](Self::set_execution_budget) and
    /// [`set_memory_budget`](Self::set_memory_budget).
    pub(crate) fn meter(&self) -> &Arc<InstanceMeter> {
        &self.meter
    }

    /// Returns a snapshot of the instance's metering data: executed guest
    /// instructions and committed owned memory pages (shared-region pages
    /// excluded).
    pub fn stats(&self) -> Result<InstanceStats> {
        let pages = self.total_memory_pages()?;
        Ok(self.meter.snapshot(pages))
    }

    /// Sets or resets the per-instance execution budget (maximum instruction
    /// count); `None` means unbounded. The new budget is enforced from the
    /// next executed instruction onward.
    pub fn set_execution_budget(&self, budget: Option<u64>) -> Result<()> {
        self.meter.set_execution_budget(budget)
    }

    /// Sets or resets the per-instance memory budget (maximum committed page
    /// count); `None` means unbounded. The new budget is enforced from the
    /// next growth onward.
    pub fn set_memory_budget(&self, budget: Option<u32>) -> Result<()> {
        self.meter.set_memory_budget(budget)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if self.attached_regions.is_empty() {
            return;
        }

        let regions: Vec<SharedRegionId> = std::mem::take(&mut self.attached_regions);
        let mut shared_memory = self.shared_memory.lock();

        for region_id in regions {
            if let Some(memory) = self.memories.first()
                && let Ok(mut mem) = memory.lock()
            {
                let _ = shared_memory.detach_region(&mut mem, region_id);
            }
        }
    }
}

impl Store {
    /// Creates a new `Store`.
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
            native_funcs: Vec::new(),
            shared_memory: Arc::new(ParkingMutex::new(SharedMemoryRegistry::default())),
        }
    }

    /// Returns a clone of the shared memory registry Arc.
    pub fn shared_memory_registry(&self) -> Arc<ParkingMutex<SharedMemoryRegistry>> {
        self.shared_memory.clone()
    }

    /// Creates a `Store` backed by an existing shared memory registry.
    ///
    /// This allows multiple stores (and their instances) to share the same
    /// `SharedMemoryRegistry`, making shared regions visible across all of them.
    pub fn with_shared_registry(registry: Arc<ParkingMutex<SharedMemoryRegistry>>) -> Self {
        Self {
            instances: Vec::new(),
            native_funcs: Vec::new(),
            shared_memory: registry,
        }
    }

    pub(crate) fn register_internal_native(
        &mut self,
        func: Box<dyn HostFunc>,
        func_type: FunctionType,
    ) -> u32 {
        let idx = self.native_funcs.len() as u32;
        self.native_funcs.push((Arc::from(func), func_type, None));
        idx
    }

    pub(crate) fn register_internal_guest_native(
        &mut self,
        func: Box<dyn HostFunc>,
        func_type: FunctionType,
        module: Arc<Module>,
        func_idx: u32,
    ) -> u32 {
        let idx = self.native_funcs.len() as u32;
        self.native_funcs.push((
            Arc::from(func),
            func_type,
            Some(GuestFuncTarget { module, func_idx }),
        ));
        idx
    }

    pub(crate) fn get_native_func(
        &self,
        idx: u32,
    ) -> Option<(&Arc<dyn HostFunc>, &FunctionType, Option<&GuestFuncTarget>)> {
        self.native_funcs
            .get(idx as usize)
            .map(|(func, func_type, target)| (func, func_type, target.as_ref()))
    }

    /// Allocates a shared region without mapping it into any guest memory.
    pub fn allocate_shared_region(&mut self, size: u32) -> Result<SharedRegionId> {
        self.shared_memory.lock().allocate_region_standalone(size)
    }

    /// Destroys shared region.
    pub fn destroy_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        self.shared_memory.lock().destroy_region(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn shared_region_len(&self, region_id: SharedRegionId) -> Result<u32> {
        self.shared_memory.lock().region_len(region_id)
    }

    /// Writes data to a shared region from the host side.
    pub fn write_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        self.shared_memory
            .lock()
            .write_to_region(region_id, offset, data)
    }

    /// Reads data from a shared region from the host side.
    pub fn read_shared_region(
        &self,
        region_id: SharedRegionId,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        self.shared_memory
            .lock()
            .read_from_region(region_id, offset, buf)
    }
}

impl<'a> HostCaller<'a> {
    pub(crate) fn new(store: &'a mut Store, memories: &'a [SharedMemory]) -> Self {
        Self { store, memories }
    }

    /// Returns the caller's linear memory at the given index.
    pub fn memory(&self, index: u32) -> Option<SharedMemory> {
        self.memories.get(index as usize).cloned()
    }

    /// Returns the runtime store backing this host call.
    pub fn store(&mut self) -> &mut Store {
        self.store
    }
}

impl std::fmt::Debug for Extern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Extern::Func(func) => f.debug_tuple("Func").field(&func.func_idx).finish(),
            Extern::HostFunc(_) => f.write_str("HostFunc(..)"),
            Extern::Table(table) => f.debug_tuple("Table").field(table).finish(),
            Extern::Memory(memory) => f.debug_tuple("Memory").field(memory).finish(),
            Extern::Global(global) => f.debug_tuple("Global").field(global).finish(),
        }
    }
}

impl GuestFuncBinding {
    pub(crate) fn into_host_func(self) -> Arc<dyn HostFunc> {
        Arc::new(GuestFuncRefHost {
            module: self.module,
            imports: self.imports,
            func_idx: self.func_idx,
            func_type: self.func_type,
        })
    }
}

impl HostFunc for GuestFuncRefHost {
    fn call(&self, _caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        let imports = self
            .imports
            .iter()
            .map(|(module, name, extern_)| {
                let extern_ = match extern_ {
                    Extern::Table(table) => Extern::Table(Arc::new(Mutex::new(
                        table.lock().map_err(poisoned_lock)?.clone(),
                    ))),
                    _ => extern_.clone(),
                };
                Ok((module.as_str(), name.as_str(), extern_))
            })
            .collect::<Result<Vec<_>>>()?;
        let instance = Arc::new(Mutex::new(Instance::with_imports(
            self.module.clone(),
            &imports,
        )?));
        let mut interpreter = crate::interpreter::Interpreter::with_instance(instance);
        interpreter.execute_function(&self.module, self.func_idx, args)
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(&self.func_type)
    }
}

impl<F> HostFunc for F
where
    F: Fn(&mut HostCaller<'_>, &[WasmValue]) -> Result<Vec<WasmValue>> + Send + Sync + 'static,
{
    fn call(&self, caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        self(caller, args)
    }

    fn function_type(&self) -> Option<&FunctionType> {
        None
    }
}

pub(crate) fn evaluate_const_expr(
    expr: &[u8],
    globals: &[Option<SharedGlobal>],
    func_refs: &[u32],
) -> Result<WasmValue> {
    let mut reader = BinaryReader::from_slice(expr);
    let mut stack = Vec::new();

    loop {
        let opcode = reader.read_u8().map_err(io_to_load_error)?;
        match opcode {
            0x0B => break,
            0x23 => {
                let idx = reader.read_uleb128().map_err(io_to_load_error)?;
                let value = globals
                    .get(idx as usize)
                    .and_then(|global| global.as_ref())
                    .ok_or_else(|| {
                        WasmError::Instantiate(format!(
                            "global {} is not allowed in constant expressions",
                            idx
                        ))
                    })?
                    .lock()
                    .map_err(poisoned_lock)?
                    .get();
                stack.push(value);
            }
            0x41 => stack.push(WasmValue::I32(
                reader.read_sleb128().map_err(io_to_load_error)?,
            )),
            0x42 => stack.push(WasmValue::I64(
                reader.read_sleb128_i64().map_err(io_to_load_error)?,
            )),
            0x43 => stack.push(WasmValue::F32(reader.read_f32().map_err(io_to_load_error)?)),
            0x44 => stack.push(WasmValue::F64(reader.read_f64().map_err(io_to_load_error)?)),
            0xD0 => stack.push(match reader.read_u8().map_err(io_to_load_error)? {
                0x70 => WasmValue::NullRef(RefType::FuncRef),
                0x6F => WasmValue::NullRef(RefType::ExternRef),
                0x63 | 0x64 => match reader.read_sleb128_i64().map_err(io_to_load_error)? {
                    -0x10 | -0x14 => WasmValue::NullRef(RefType::FuncRef),
                    -0x11 | -0x13 => WasmValue::NullRef(RefType::ExternRef),
                    idx if idx >= 0 => WasmValue::NullRef(RefType::FuncRef),
                    heap_type => {
                        return Err(WasmError::Instantiate(format!(
                            "invalid ref.null heap type: {}",
                            heap_type
                        )));
                    }
                },
                byte if byte < 0x40 => WasmValue::NullRef(RefType::FuncRef),
                value => {
                    return Err(WasmError::Instantiate(format!(
                        "invalid ref.null type: {:02x}",
                        value
                    )));
                }
            }),
            0xD2 => {
                let func_idx = reader.read_uleb128().map_err(io_to_load_error)? as usize;
                let handle = *func_refs.get(func_idx).ok_or_else(|| {
                    WasmError::Instantiate(format!("function ref {} not found", func_idx))
                })?;
                stack.push(WasmValue::native_func_ref(handle));
            }
            0x6A => {
                let rhs = pop_const_i32(&mut stack)?;
                let lhs = pop_const_i32(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_add(rhs)));
            }
            0x6B => {
                let rhs = pop_const_i32(&mut stack)?;
                let lhs = pop_const_i32(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_sub(rhs)));
            }
            0x6C => {
                let rhs = pop_const_i32(&mut stack)?;
                let lhs = pop_const_i32(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_mul(rhs)));
            }
            0x7C => {
                let rhs = pop_const_i64(&mut stack)?;
                let lhs = pop_const_i64(&mut stack)?;
                stack.push(WasmValue::I64(lhs.wrapping_add(rhs)));
            }
            0x7D => {
                let rhs = pop_const_i64(&mut stack)?;
                let lhs = pop_const_i64(&mut stack)?;
                stack.push(WasmValue::I64(lhs.wrapping_sub(rhs)));
            }
            0x7E => {
                let rhs = pop_const_i64(&mut stack)?;
                let lhs = pop_const_i64(&mut stack)?;
                stack.push(WasmValue::I64(lhs.wrapping_mul(rhs)));
            }
            value => {
                return Err(WasmError::Instantiate(format!(
                    "unsupported const expr opcode: {:02x}",
                    value
                )));
            }
        }
    }

    if reader.remaining() != 0 {
        return Err(WasmError::Instantiate(
            "constant expression has trailing bytes".to_string(),
        ));
    }

    match stack.as_slice() {
        [value] => Ok(*value),
        _ => Err(WasmError::Instantiate(
            "constant expression must leave exactly one value".to_string(),
        )),
    }
}

fn io_to_load_error(error: std::io::Error) -> WasmError {
    WasmError::Load(error.to_string())
}

fn memory_matches_required(actual: &Memory, required: &crate::runtime::MemoryType) -> bool {
    actual.size() >= required.limits.min()
        && match (actual.type_().limits.max(), required.limits.max()) {
            (_, None) => true,
            (Some(actual_max), Some(required_max)) => actual_max <= required_max,
            (None, Some(_)) => false,
        }
}

fn poisoned_lock<T>(_: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>) -> WasmError {
    WasmError::Runtime("instance lock poisoned".to_string())
}

fn pop_const_i32(stack: &mut Vec<WasmValue>) -> Result<i32> {
    match stack.pop() {
        Some(WasmValue::I32(value)) => Ok(value),
        Some(value) => Err(WasmError::Instantiate(format!(
            "constant expression expected i32, got {:?}",
            value.val_type()
        ))),
        None => Err(WasmError::Instantiate(
            "constant expression stack underflow".to_string(),
        )),
    }
}

fn pop_const_i64(stack: &mut Vec<WasmValue>) -> Result<i64> {
    match stack.pop() {
        Some(WasmValue::I64(value)) => Ok(value),
        Some(value) => Err(WasmError::Instantiate(format!(
            "constant expression expected i64, got {:?}",
            value.val_type()
        ))),
        None => Err(WasmError::Instantiate(
            "constant expression stack underflow".to_string(),
        )),
    }
}

fn table_matches_required(actual: &Table, required: &crate::runtime::TableType) -> bool {
    actual.type_.elem_type == required.elem_type
        && (actual.type_.nullable == required.nullable
            || (!actual.type_.nullable && required.nullable))
        && actual.size() >= required.limits.min()
        && match (actual.type_.limits.max(), required.limits.max()) {
            (_, None) => true,
            (Some(actual_max), Some(required_max)) => actual_max <= required_max,
            (None, Some(_)) => false,
        }
}

fn validate_values(values: &[WasmValue], expected: &[ValType], kind: &str) -> Result<()> {
    if values.len() != expected.len() {
        return Err(WasmError::Runtime(format!(
            "{} count mismatch: expected {}, got {}",
            kind,
            expected.len(),
            values.len()
        )));
    }

    for (index, (value, expected_type)) in values.iter().zip(expected.iter()).enumerate() {
        if value.val_type() != *expected_type {
            return Err(WasmError::Runtime(format!(
                "{} {} type mismatch: expected {:?}, got {:?}",
                kind,
                index,
                expected_type,
                value.val_type()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::PAGE_SIZE_BYTES;
    use crate::runtime::{
        Func, GlobalType, Import, Limits, MemoryType, TableType, TrapCode, ValType,
    };

    #[test]
    fn test_imported_state_is_shared() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: ImportKind::Memory(MemoryType::new(Limits::Min(1))),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: ImportKind::Table(TableType::new(RefType::FuncRef, Limits::Min(1))),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: ImportKind::Global(GlobalType::new(
                ValType::Num(crate::runtime::NumType::I32),
                true,
            )),
        });

        let memory = Arc::new(Mutex::new(
            Memory::new(MemoryType::new(Limits::Min(1))).unwrap(),
        ));
        let table = Arc::new(Mutex::new(Table::new(TableType::new(
            RefType::FuncRef,
            Limits::Min(1),
        ))));
        let global = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(crate::runtime::NumType::I32), true),
                WasmValue::I32(1),
            )
            .unwrap(),
        ));

        let mut instance = Instance::with_imports(
            Arc::new(module),
            &[
                ("env", "memory", Extern::Memory(memory.clone())),
                ("env", "table", Extern::Table(table.clone())),
                ("env", "global", Extern::Global(global.clone())),
            ],
        )
        .unwrap();

        instance
            .memory_mut(0)
            .unwrap()
            .lock()
            .unwrap()
            .write_u8(0, 9)
            .unwrap();
        instance
            .table_mut(0)
            .unwrap()
            .lock()
            .unwrap()
            .set(0, WasmValue::FuncRef(7))
            .unwrap();
        instance
            .global_mut(0)
            .unwrap()
            .lock()
            .unwrap()
            .set(WasmValue::I32(42))
            .unwrap();

        assert_eq!(memory.lock().unwrap().read_u8(0).unwrap(), 9);
        assert_eq!(table.lock().unwrap().get(0), Some(WasmValue::FuncRef(7)));
        assert_eq!(global.lock().unwrap().get(), WasmValue::I32(42));
    }

    #[test]
    fn test_missing_imports_fail_instantiation() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: ImportKind::Memory(MemoryType::new(Limits::Min(1))),
        });

        let result = Instance::with_imports(Arc::new(module), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_imports_are_bound_in_module_order() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "first".to_string(),
            kind: ImportKind::Global(GlobalType::new(
                ValType::Num(crate::runtime::NumType::I32),
                false,
            )),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "second".to_string(),
            kind: ImportKind::Global(GlobalType::new(
                ValType::Num(crate::runtime::NumType::I32),
                false,
            )),
        });
        module.types.push(FunctionType::new(
            vec![],
            vec![ValType::Num(crate::runtime::NumType::I32)],
        ));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x23, 0x00, 0x0B],
        });

        let first = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(crate::runtime::NumType::I32), false),
                WasmValue::I32(7),
            )
            .unwrap(),
        ));
        let second = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(crate::runtime::NumType::I32), false),
                WasmValue::I32(11),
            )
            .unwrap(),
        ));

        let instance = Arc::new(Mutex::new(
            Instance::with_imports(
                Arc::new(module.clone()),
                &[
                    ("env", "second", Extern::Global(second)),
                    ("env", "first", Extern::Global(first)),
                ],
            )
            .unwrap(),
        ));

        let mut interpreter = crate::interpreter::Interpreter::with_instance(instance);
        let results = interpreter.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(7)]);
    }

    #[test]
    fn test_memory_and_table_imports_accept_compatible_subtypes() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: ImportKind::Memory(MemoryType::new(Limits::MinMax(1, 4))),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: ImportKind::Table(TableType::new(RefType::FuncRef, Limits::MinMax(1, 4))),
        });

        let memory = Arc::new(Mutex::new(
            Memory::new(MemoryType::new(Limits::MinMax(2, 3))).unwrap(),
        ));
        let table = Arc::new(Mutex::new(Table::new(TableType::new(
            RefType::FuncRef,
            Limits::MinMax(2, 3),
        ))));

        let instance = Instance::with_imports(
            Arc::new(module),
            &[
                ("env", "memory", Extern::Memory(memory)),
                ("env", "table", Extern::Table(table)),
            ],
        );

        assert!(instance.is_ok());
    }

    #[test]
    fn test_with_imports_accepts_untyped_host_func() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(crate::runtime::NumType::I32)],
            vec![ValType::Num(crate::runtime::NumType::I32)],
        ));
        module.imports.push(Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: ImportKind::Func(0),
        });

        let mut instance = Instance::with_imports(
            Arc::new(module),
            &[(
                "env",
                "host",
                Extern::HostFunc(Arc::new(|_: &mut HostCaller<'_>, args: &[WasmValue]| {
                    Ok(vec![args[0]])
                })),
            )],
        )
        .unwrap();

        let results = instance.call(0, &[WasmValue::I32(9)]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(9)]);
    }

    #[test]
    fn test_host_call_rejects_argument_type_mismatch() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(crate::runtime::NumType::I32)],
            vec![],
        ));
        module.imports.push(Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: ImportKind::Func(0),
        });

        let mut instance = Instance::with_imports(
            Arc::new(module),
            &[(
                "env",
                "host",
                Extern::HostFunc(Arc::new(|_: &mut HostCaller<'_>, _: &[WasmValue]| {
                    Ok(vec![])
                })),
            )],
        )
        .unwrap();

        let error = instance.call(0, &[WasmValue::F64(1.0)]).unwrap_err();
        assert!(
            matches!(error, WasmError::Runtime(message) if message.contains("argument 0 type mismatch"))
        );
    }

    #[test]
    fn test_host_call_rejects_result_type_mismatch() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![],
            vec![ValType::Num(crate::runtime::NumType::I32)],
        ));
        module.imports.push(Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: ImportKind::Func(0),
        });

        let mut instance = Instance::with_imports(
            Arc::new(module),
            &[(
                "env",
                "host",
                Extern::HostFunc(Arc::new(|_: &mut HostCaller<'_>, _: &[WasmValue]| {
                    Ok(vec![])
                })),
            )],
        )
        .unwrap();

        let error = instance.call(0, &[]).unwrap_err();
        assert!(
            matches!(error, WasmError::Runtime(message) if message.contains("result count mismatch"))
        );
    }

    #[test]
    fn test_duplicate_named_imports_bind_by_occurrence() {
        let mut module = Module::new();
        module.imports.push(Import {
            module: "env".to_string(),
            name: "shared".to_string(),
            kind: ImportKind::Global(GlobalType::new(
                ValType::Num(crate::runtime::NumType::I32),
                false,
            )),
        });
        module.imports.push(Import {
            module: "env".to_string(),
            name: "shared".to_string(),
            kind: ImportKind::Global(GlobalType::new(
                ValType::Num(crate::runtime::NumType::I32),
                false,
            )),
        });

        let first = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(crate::runtime::NumType::I32), false),
                WasmValue::I32(1),
            )
            .unwrap(),
        ));
        let second = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(crate::runtime::NumType::I32), false),
                WasmValue::I32(2),
            )
            .unwrap(),
        ));

        let instance = Instance::with_imports(
            Arc::new(module),
            &[
                ("env", "shared", Extern::Global(first)),
                ("env", "shared", Extern::Global(second)),
            ],
        )
        .unwrap();

        assert_eq!(
            instance.global(0).unwrap().lock().unwrap().get(),
            WasmValue::I32(1)
        );
        assert_eq!(
            instance.global(1).unwrap().lock().unwrap().get(),
            WasmValue::I32(2)
        );
    }

    fn module_with_memory() -> Module {
        let mut module = Module::new();
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module
    }

    #[test]
    fn test_shared_region_visibility_across_instances() {
        use crate::memory::RegionProt;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());
        let mut first = Instance::new_with_store(module.clone(), store.clone()).unwrap();
        let mut second = Instance::new_with_store(module, store).unwrap();

        // Allocate region in first instance's memory
        let (region_id, _first_page_offset) = first
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        // Attach to second instance
        let _second_page_offset = second
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap();

        // Write via host-side API
        first
            .write_shared_region(region_id, 4, &[1, 2, 3, 4])
            .unwrap();

        // Read from second instance via host-side API
        let mut buf = [0u8; 4];
        second.read_shared_region(region_id, 4, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        // Write i32 via host-side API
        second
            .write_shared_region(region_id, 0, &99i32.to_le_bytes())
            .unwrap();

        // Read back from first instance
        let mut i32_buf = [0u8; 4];
        first
            .read_shared_region(region_id, 0, &mut i32_buf)
            .unwrap();
        assert_eq!(i32::from_le_bytes(i32_buf), 99);
    }

    #[test]
    fn test_shared_region_detach_destroy_and_invalid_access_failures() {
        use crate::memory::RegionProt;

        let module = Arc::new(module_with_memory());
        let mut instance = Instance::new(module).unwrap();

        let (region_id, _page_offset) = instance
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        // Destroy while attached should fail
        let destroy_while_attached = instance.destroy_shared_region(region_id).unwrap_err();
        assert!(matches!(
            destroy_while_attached,
            WasmError::Runtime(message) if message.contains("attached")
        ));

        // Detach
        instance.detach_shared_region(region_id).unwrap();

        // Destroy after detach should succeed
        instance.destroy_shared_region(region_id).unwrap();

        // Attach destroyed region should fail
        let missing_region = instance
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap_err();
        assert!(matches!(
            missing_region,
            WasmError::Runtime(message) if message.contains("shared region")
        ));
    }

    #[test]
    fn test_shared_region_standalone_allocation() {
        let mut store = Store::new();
        let region_id = store.allocate_shared_region(PAGE_SIZE_BYTES).unwrap();

        // Write and read via store
        store
            .write_shared_region(region_id, 0, &17i32.to_le_bytes())
            .unwrap();
        store
            .write_shared_region(region_id, 4, &23i32.to_le_bytes())
            .unwrap();

        let mut buf = [0u8; 8];
        store.read_shared_region(region_id, 0, &mut buf).unwrap();
        let val0 = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let val1 = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(val0, 17);
        assert_eq!(val1, 23);
    }

    #[test]
    fn test_store_shared_region_access_after_detach_fails() {
        use crate::memory::RegionProt;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());
        let mut instance = Instance::new_with_store(module, store.clone()).unwrap();

        let region_id = store
            .lock()
            .unwrap()
            .allocate_shared_region(PAGE_SIZE_BYTES)
            .unwrap();
        let _page_offset = instance
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap();

        instance.write_shared_region(region_id, 0, &[5]).unwrap();
        instance.detach_shared_region(region_id).unwrap();

        // After detach, the region still exists in the store but is not mapped
        // in this instance. Host-side read/write still works via the store.
        let mut buf = [0u8; 1];
        store
            .lock()
            .unwrap()
            .read_shared_region(region_id, 0, &mut buf)
            .unwrap();
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn test_instance_drop_detaches_shared_regions() {
        use crate::memory::RegionProt;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());

        let region_id = {
            let mut instance = Instance::new_with_store(module, store.clone()).unwrap();
            let (region_id, _page_offset) = instance
                .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
                .unwrap();
            region_id
        };

        // After instance drop, the region should have no attachments
        store
            .lock()
            .unwrap()
            .destroy_shared_region(region_id)
            .unwrap();
    }

    #[test]
    fn test_reader_slot_write_protection() {
        use crate::memory::RegionProt;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());
        let mut instance = Instance::new_with_store(module, store.clone()).unwrap();

        // Allocate a 2-page shared region standalone
        let region_id = store
            .lock()
            .unwrap()
            .allocate_shared_region(2 * PAGE_SIZE_BYTES)
            .unwrap();

        // Attach with ReadOnly protection and reader_slot = 1
        // This means page 1 is writable, page 0 is read-only
        let page_offset = instance
            .attach_shared_region(region_id, RegionProt::ReadOnly, Some(1))
            .unwrap();

        // Get the memory to test writes
        let memory = instance.memory(0).unwrap().clone();
        let mut mem = memory.lock().unwrap();

        // Calculate byte offsets
        let page0_offset = page_offset * PAGE_SIZE_BYTES;
        let page1_offset = (page_offset + 1) * PAGE_SIZE_BYTES;

        // Writing to page 1 (reader_slot) should succeed
        mem.write_u32(page1_offset, 0xDEAD_BEEF).unwrap();
        assert_eq!(mem.read_u32(page1_offset).unwrap(), 0xDEAD_BEEF);

        // Writing to page 0 (read-only) should fail with MemoryOutOfBounds
        let result = mem.write_u32(page0_offset, 0xCAFEBABE);
        assert!(matches!(
            result,
            Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
        ));

        // Reading from page 0 should still work (PROT_READ)
        assert_eq!(mem.read_u32(page0_offset).unwrap(), 0);
    }

    #[test]
    fn test_cross_instance_wait_notify() {
        use crate::memory::RegionProt;
        use std::thread;
        use std::time::Duration;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());

        // Create two instances sharing the same store
        let mut instance1 = Instance::new_with_store(module.clone(), store.clone()).unwrap();
        let mut instance2 = Instance::new_with_store(module.clone(), store.clone()).unwrap();

        // Allocate a shared region from instance1
        let (region_id, page_offset) = instance1
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        // Attach the same region to instance2
        let _page_offset2 = instance2
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap();

        // Calculate the byte address in the shared region
        let shared_addr = page_offset * PAGE_SIZE_BYTES;

        // Write an initial value to the shared memory
        instance1
            .write_shared_region(region_id, 0, &42i32.to_le_bytes())
            .unwrap();

        // Spawn a thread that will notify after a short delay
        let notify_handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));

            // Use instance2 which has the shared region attached
            let memory = instance2.memory(0).unwrap().clone();
            let mem = memory.lock().unwrap();
            mem.notify(shared_addr, 1).unwrap();
        });

        // Wait on the shared address with 1-second nanosecond timeout
        let result = instance1.wait32(shared_addr, 42, 1_000_000_000).unwrap();

        // Result should be 0 (woken) not 2 (timeout)
        assert_eq!(result, 0, "wait32 should have been woken by notify");

        notify_handle.join().unwrap();
    }

    #[test]
    fn test_concurrent_writers_with_backoff() {
        use crate::memory::RegionProt;
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::thread;

        let store = Arc::new(Mutex::new(Store::new()));
        let module = Arc::new(module_with_memory());

        // Create two instances sharing the same store
        let mut instance1 = Instance::new_with_store(module.clone(), store.clone()).unwrap();
        let mut instance2 = Instance::new_with_store(module, store.clone()).unwrap();

        // Allocate a shared region
        let (region_id, _page_offset) = instance1
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        // Attach to instance2
        instance2
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap();

        // Initialize counter to 0
        instance1
            .write_shared_region(region_id, 0, &0i32.to_le_bytes())
            .unwrap();

        let iterations = 100;
        let counter = Arc::new(AtomicI32::new(0));

        // Spawn two threads that increment the counter with exponential backoff
        let counter1 = counter.clone();
        let store1 = store.clone();
        let handle1 = thread::spawn(move || {
            let module = Arc::new(module_with_memory());
            let instance = Instance::new_with_store(module, store1).unwrap();

            for _ in 0..iterations {
                let mut backoff = 1;
                loop {
                    // Read current value
                    let mut buf = [0u8; 4];
                    instance.read_shared_region(region_id, 0, &mut buf).unwrap();
                    let current = i32::from_le_bytes(buf);

                    // Try to increment with CAS
                    let new_val = current + 1;
                    if counter1
                        .compare_exchange(current, new_val, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        // Write back to shared memory
                        instance
                            .write_shared_region(region_id, 0, &new_val.to_le_bytes())
                            .unwrap();
                        break;
                    }

                    // Exponential backoff
                    thread::sleep(std::time::Duration::from_micros(backoff));
                    backoff = (backoff * 2).min(1000);
                }
            }
        });

        let counter2 = counter.clone();
        let store2 = store.clone();
        let handle2 = thread::spawn(move || {
            let module = Arc::new(module_with_memory());
            let instance = Instance::new_with_store(module, store2).unwrap();

            for _ in 0..iterations {
                let mut backoff = 1;
                loop {
                    // Read current value
                    let mut buf = [0u8; 4];
                    instance.read_shared_region(region_id, 0, &mut buf).unwrap();
                    let current = i32::from_le_bytes(buf);

                    // Try to increment with CAS
                    let new_val = current + 1;
                    if counter2
                        .compare_exchange(current, new_val, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        // Write back to shared memory
                        instance
                            .write_shared_region(region_id, 0, &new_val.to_le_bytes())
                            .unwrap();
                        break;
                    }

                    // Exponential backoff
                    thread::sleep(std::time::Duration::from_micros(backoff));
                    backoff = (backoff * 2).min(1000);
                }
            }
        });

        handle1.join().unwrap();
        handle2.join().unwrap();

        // Verify final counter value
        let final_val = counter.load(Ordering::SeqCst);
        assert_eq!(
            final_val,
            iterations * 2,
            "Counter should be incremented {} times by each thread",
            iterations
        );

        // Verify shared memory matches
        let mut buf = [0u8; 4];
        instance1
            .read_shared_region(region_id, 0, &mut buf)
            .unwrap();
        let shared_val = i32::from_le_bytes(buf);
        assert_eq!(
            shared_val, final_val,
            "Shared memory should match atomic counter"
        );
    }

    #[test]
    fn test_metering_memory_gauge_excludes_shared_regions() {
        use crate::memory::{PAGE_SIZE_BYTES, RegionProt};

        let module = Arc::new(module_with_memory());
        let mut instance = Instance::new(module).unwrap();

        assert_eq!(instance.stats().unwrap().memory_pages, 1);

        // Owned growth reflects in the gauge.
        instance.grow_memory(0, 2).unwrap();
        assert_eq!(instance.stats().unwrap().memory_pages, 3);

        // Attaching a shared region must not inflate the owned-page gauge.
        let (region_id, _page_offset) = instance
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();
        assert_eq!(instance.stats().unwrap().memory_pages, 3);

        // Detaching leaves the gauge unchanged as well.
        instance.detach_shared_region(region_id).unwrap();
        assert_eq!(instance.stats().unwrap().memory_pages, 3);
    }

    #[test]
    fn test_metering_memory_budget_enforced_at_grow() {
        let module = Arc::new(module_with_memory());
        let mut instance = Instance::new(module).unwrap();
        instance.set_memory_budget(Some(2)).unwrap();

        // Grow within the budget.
        assert_eq!(instance.grow_memory(0, 1).unwrap(), 1);

        // Grow beyond the budget fails with the distinct memory-limit trap
        // and leaves memory untouched.
        let error = instance.grow_memory(0, 1).unwrap_err();
        assert_eq!(error, WasmError::Trap(TrapCode::MemoryLimitExceeded));
        assert_eq!(instance.stats().unwrap().memory_pages, 2);
    }

    #[test]
    fn test_metering_budgets_resettable_mid_life() {
        use crate::runtime::Func;

        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![],
            vec![ValType::Num(crate::runtime::NumType::I32)],
        ));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x2A, 0x0B],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));

        // Unbounded by default: the first invocation completes.
        let mut interp = crate::interpreter::Interpreter::with_instance(instance.clone());
        interp.execute_function(&module, 0, &[]).unwrap();
        let first_count = instance
            .lock()
            .unwrap()
            .stats()
            .unwrap()
            .executed_instructions;

        // Reset the execution budget mid-life to a new ceiling; the next
        // invocation completes under it.
        instance
            .lock()
            .unwrap()
            .set_execution_budget(Some(first_count + 2))
            .unwrap();
        let mut interp = crate::interpreter::Interpreter::with_instance(instance.clone());
        interp.execute_function(&module, 0, &[]).unwrap();

        // Reset to `None` (unbounded) mid-life.
        instance.lock().unwrap().set_execution_budget(None).unwrap();
        let mut interp = crate::interpreter::Interpreter::with_instance(instance.clone());
        interp.execute_function(&module, 0, &[]).unwrap();

        // The count never decreased through any of the resets.
        let final_count = instance
            .lock()
            .unwrap()
            .stats()
            .unwrap()
            .executed_instructions;
        assert!(final_count > first_count);
    }
}
