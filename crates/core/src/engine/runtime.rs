use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use super::loader::EngineLoader;
use parking_lot::Mutex as ParkingMutex;

use crate::{
    interpreter::Interpreter,
    memory::RegionProt,
    runtime::{
        Extern, FunctionType, Global, HostCaller, HostFunc, ImportKind, Instance, Memory, Module,
        Result, SharedMemory, SharedMemoryRegistry, SharedRegionId, SharedTable, Table, WasmError,
        WasmValue,
    },
};

static NEXT_AOT_MODULE_ID: AtomicU64 = AtomicU64::new(1);

struct TypedHostImport {
    inner: Arc<dyn HostFunc>,
    func_type: FunctionType,
}

#[derive(Debug, Clone)]
/// Export exposed by an ahead-of-time module.
pub enum Export {
    /// A function export identified by function index.
    Function(u32),
    /// A table export identified by table index.
    Table(u32),
    /// A memory export identified by memory index.
    Memory(u32),
    /// A global export identified by global index.
    Global(u32),
}

/// Ahead-of-time module state.
pub struct LoadedModule {
    runtime_id: u64,
    module: Module,
    imports: Vec<Option<Extern>>,
    store: Arc<Mutex<crate::runtime::Store>>,
    shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
    initialisation_error: Option<WasmError>,
    custom_memories: bool,
    custom_tables: bool,
    custom_globals: bool,
    /// Cached instance reused across `invoke_function` calls to avoid
    /// per-call module cloning and instance rebuild.
    cached_instance: Option<Arc<Mutex<Instance>>>,
    /// Defined memories shared with every instance created from this module.
    ///
    /// The same `SharedMemory` is installed into each per-invocation
    /// `Instance`, so `memory.grow` and shared-region mappings performed
    /// during one invocation remain visible to subsequent invocations.
    pub memories: Vec<SharedMemory>,
    /// Defined tables owned by the module.
    pub tables: Vec<SharedTable>,
    /// Defined globals owned by the module.
    pub globals: Vec<Global>,
    attached_regions: Vec<SharedRegionId>,
    /// Export map keyed by export name.
    pub exports: HashMap<String, Export>,
}

/// Ahead-of-time runtime manager.
pub struct Engine {
    loader: EngineLoader,
    /// Modules currently loaded into the runtime.
    pub modules: Vec<Box<LoadedModule>>,
}

impl TypedHostImport {
    fn new(inner: Arc<dyn HostFunc>, func_type: FunctionType) -> Self {
        Self { inner, func_type }
    }
}

impl HostFunc for TypedHostImport {
    fn call(&self, caller: &mut HostCaller<'_>, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        self.inner.call(caller, args)
    }

    fn function_type(&self) -> Option<&FunctionType> {
        Some(&self.func_type)
    }
}

impl LoadedModule {
    /// Creates an ahead-of-time module from a parsed module.
    pub fn from_module(module: &Module) -> Self {
        let store_state = crate::runtime::Store::new();
        let shared_memory = store_state.shared_memory_registry();
        let store = Arc::new(Mutex::new(store_state));
        Self::from_module_parts(module, store, shared_memory)
    }

    /// Creates an ahead-of-time module backed by the provided store.
    pub fn from_module_with_store(
        module: &Module,
        store: Arc<Mutex<crate::runtime::Store>>,
    ) -> Result<Self> {
        let shared_memory = store
            .lock()
            .map_err(poisoned_lock)?
            .shared_memory_registry();
        Ok(Self::from_module_parts(module, store, shared_memory))
    }

    fn from_module_parts(
        module: &Module,
        store: Arc<Mutex<crate::runtime::Store>>,
        shared_memory: Arc<ParkingMutex<SharedMemoryRegistry>>,
    ) -> Self {
        let mut aot_module = Self {
            runtime_id: NEXT_AOT_MODULE_ID.fetch_add(1, Ordering::SeqCst),
            module: module.clone(),
            imports: vec![None; module.imports.len()],
            store,
            shared_memory,
            initialisation_error: None,
            custom_memories: false,
            custom_tables: false,
            custom_globals: false,
            cached_instance: None,
            memories: Vec::new(),
            tables: Vec::new(),
            globals: Vec::new(),
            attached_regions: Vec::new(),
            exports: HashMap::new(),
        };
        aot_module.initialisation_error = aot_module
            .initialise_defined_allocations()
            .err()
            .or_else(|| aot_module.initialise_globals_without_imports().err());
        aot_module
    }

    /// Returns the underlying module.
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// Returns the runtime identifier for this module instance.
    pub fn runtime_id(&self) -> u64 {
        self.runtime_id
    }

    /// Returns the declared imports.
    pub fn imports(&self) -> &[crate::runtime::Import] {
        &self.module.imports
    }

    /// Returns a resolved import binding, if present.
    pub fn import_binding(&self, idx: usize) -> Option<&Extern> {
        self.imports.get(idx).and_then(|binding| binding.as_ref())
    }

    /// Invokes function.
    pub fn invoke_function(&mut self, idx: u32, args: &[WasmValue]) -> Result<Vec<WasmValue>> {
        self.ensure_initialised()?;

        let imported_funcs = self
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.kind, ImportKind::Func(_)))
            .count() as u32;

        // Reuse the cached instance if available; otherwise build one.
        let instance = if let Some(ref cached) = self.cached_instance {
            cached.clone()
        } else {
            let (imported_memories, imported_tables, imported_globals) = self.import_counts();
            let imports = self.ordered_imports();
            let instance: Arc<Mutex<Instance>> =
                Arc::new(Mutex::new(Instance::with_imports_and_store(
                    Arc::new(self.module.clone()),
                    &imports,
                    self.store.clone(),
                )?));
            {
                let mut instance_guard = instance.lock().map_err(poisoned_lock)?;
                for (offset, memory) in self.memories.iter().cloned().enumerate() {
                    let target = imported_memories + offset;
                    if target >= instance_guard.memories.len() {
                        instance_guard.memories.push(memory);
                    } else {
                        instance_guard.memories[target] = memory;
                    }
                }
                for (offset, table) in self.tables.iter().cloned().enumerate() {
                    let target = imported_tables + offset;
                    if target >= instance_guard.tables.len() {
                        instance_guard.tables.push(table);
                    } else {
                        instance_guard.tables[target] = table;
                    }
                }
                for (offset, global) in self.globals.iter().cloned().enumerate() {
                    let target = imported_globals + offset;
                    if target >= instance_guard.globals.len() {
                        instance_guard.globals.push(Arc::new(Mutex::new(global)));
                    } else {
                        instance_guard.globals[target] = Arc::new(Mutex::new(global));
                    }
                }
            }
            instance
        };

        if idx < imported_funcs {
            instance.lock().map_err(poisoned_lock)?.call(idx, args)
        } else {
            let mut interpreter = Interpreter::with_instance(instance.clone());
            interpreter.execute_function(&self.module, idx, args)
        }
    }

    /// Registers import.
    pub fn register_import(&mut self, module: &str, name: &str, extern_: Extern) -> Result<()> {
        self.ensure_initialised()?;
        self.ensure_jit_inactive_for_external_mutation()?;
        let matching_indices = self
            .module
            .imports
            .iter()
            .enumerate()
            .filter(|(_, import)| import.module == module && import.name == name)
            .map(|(idx, _)| idx)
            .collect::<Vec<_>>();
        if matching_indices.is_empty() {
            return Err(WasmError::Instantiate(format!(
                "import {}.{} not found",
                module, name
            )));
        }

        let unresolved = matching_indices
            .into_iter()
            .filter(|idx| self.imports[*idx].is_none())
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return Err(WasmError::Instantiate(format!(
                "import {}.{} already registered",
                module, name
            )));
        }

        let mut last_error = None;
        for import_idx in unresolved {
            match self.validate_import_binding(import_idx, module, name, extern_.clone()) {
                Ok(stored) => {
                    self.imports[import_idx] = Some(stored);
                    if self.imports_ready() {
                        self.materialise_defined_state_from_instance()?;
                    }
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            WasmError::Instantiate(format!("import {}.{} kind mismatch", module, name))
        }))
    }

    /// Registers host import.
    pub fn register_host_import(
        &mut self,
        module: &str,
        name: &str,
        func: Box<dyn HostFunc>,
        func_type: FunctionType,
    ) -> Result<()> {
        let func: Arc<dyn HostFunc> = Arc::from(func);
        if let Some(actual) = func.function_type()
            && actual != &func_type
        {
            return Err(WasmError::Instantiate(format!(
                "import {}.{} function type mismatch",
                module, name
            )));
        }

        self.register_import(
            module,
            name,
            Extern::HostFunc(Arc::new(TypedHostImport::new(func, func_type))),
        )
    }

    /// Registers memory import.
    pub fn register_memory_import(
        &mut self,
        module: &str,
        name: &str,
        memory: Memory,
    ) -> Result<()> {
        self.register_import(module, name, Extern::Memory(Arc::new(Mutex::new(memory))))
    }

    /// Registers table import.
    pub fn register_table_import(&mut self, module: &str, name: &str, table: Table) -> Result<()> {
        self.register_import(module, name, Extern::Table(Arc::new(Mutex::new(table))))
    }

    /// Registers global import.
    pub fn register_global_import(
        &mut self,
        module: &str,
        name: &str,
        global: Global,
    ) -> Result<()> {
        self.register_import(module, name, Extern::Global(Arc::new(Mutex::new(global))))
    }

    /// Instantiates the module and resolves its imports.
    pub fn instantiate(&mut self) -> Result<()> {
        self.ensure_initialised()?;
        self.ensure_jit_inactive_for_external_mutation()?;
        self.materialise_defined_state_from_instance()?;
        Ok(())
    }

    /// Returns export.
    pub fn get_export(&self, name: &str) -> Option<&Export> {
        self.exports.get(name)
    }

    /// Returns the start function index, if one is defined.
    pub fn start_function(&self) -> Option<u32> {
        self.module.start
    }

    /// Sets memory.
    pub fn set_memory(&mut self, memory: Memory) {
        self.custom_memories = true;
        if self.memories.is_empty() {
            self.memories.push(Arc::new(Mutex::new(memory)));
        } else {
            self.memories[0] = Arc::new(Mutex::new(memory));
        }
    }

    /// Returns memory.
    pub fn get_memory(&self) -> Option<Memory> {
        if self.ensure_initialised().is_err() {
            return None;
        }
        if self.import_counts().0 > 0 {
            self.imported_memory(0)
                .and_then(|memory| memory.lock().ok().map(|memory| memory.clone()))
        } else {
            self.memories
                .first()
                .and_then(|memory| memory.lock().ok().map(|memory| memory.clone()))
        }
    }

    /// Allocates a shared region and maps it into this module's memory.
    ///
    /// Returns `(region_id, page_offset)`.
    pub fn allocate_shared_region(
        &mut self,
        size: u32,
        prot: RegionProt,
    ) -> Result<(SharedRegionId, u32)> {
        self.ensure_jit_inactive_for_external_mutation()?;
        let memory = self.memories.first().ok_or_else(|| {
            WasmError::Runtime("no memory to attach shared region to".to_string())
        })?;
        let mut memory = memory.lock().map_err(poisoned_lock)?;
        let result = self
            .shared_memory
            .lock()
            .allocate_region(&mut memory, size, prot)?;
        self.attached_regions.push(result.0);
        Ok(result)
    }

    /// Allocates a shared region without mapping it into any guest memory.
    pub fn allocate_shared_region_standalone(&mut self, size: u32) -> Result<SharedRegionId> {
        self.ensure_jit_inactive_for_external_mutation()?;
        self.shared_memory.lock().allocate_region_standalone(size)
    }

    /// Destroys shared region.
    pub fn destroy_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        self.ensure_jit_inactive_for_external_mutation()?;
        self.shared_memory.lock().destroy_region(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn shared_region_len(&self, region_id: SharedRegionId) -> Result<u32> {
        self.shared_memory.lock().region_len(region_id)
    }

    /// Attaches an existing shared region to this module's memory.
    ///
    /// Returns the page offset where the region was mapped.
    pub fn attach_shared_region(
        &mut self,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
    ) -> Result<u32> {
        self.ensure_jit_inactive_for_external_mutation()?;
        let memory = self.memories.first().ok_or_else(|| {
            WasmError::Runtime("no memory to attach shared region to".to_string())
        })?;
        let mut memory = memory.lock().map_err(poisoned_lock)?;
        let page_offset =
            self.shared_memory
                .lock()
                .attach_region(&mut memory, region_id, prot, reader_slot)?;
        self.attached_regions.push(region_id);
        Ok(page_offset)
    }

    /// Detaches a shared region from this module's memory.
    pub fn detach_shared_region(&mut self, region_id: SharedRegionId) -> Result<()> {
        self.ensure_jit_inactive_for_external_mutation()?;
        if !self.attached_regions.contains(&region_id) {
            return Err(WasmError::Runtime(format!(
                "shared region {} is not attached to this module",
                region_id.raw()
            )));
        }
        let memory = self.memories.first().ok_or_else(|| {
            WasmError::Runtime("no memory to detach shared region from".to_string())
        })?;
        let mut memory = memory.lock().map_err(poisoned_lock)?;
        self.shared_memory
            .lock()
            .detach_region(&mut memory, region_id)?;
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

    /// Returns or updates memory context.
    pub fn memory_context(&mut self) -> Option<(*mut u8, usize)> {
        if self.import_counts().0 > 0 {
            None
        } else {
            self.memories.first().and_then(|memory| {
                memory
                    .lock()
                    .ok()
                    .map(|memory| (memory.as_ptr() as *mut u8, memory.len_bytes()))
            })
        }
    }

    /// Adds table.
    pub fn add_table(&mut self, table: Table) -> u32 {
        self.custom_tables = true;
        let idx = (self.import_counts().1 + self.tables.len()) as u32;
        self.tables.push(Arc::new(Mutex::new(table)));
        idx
    }

    /// Returns table.
    pub fn get_table(&self, idx: u32) -> Option<Table> {
        if self.ensure_initialised().is_err() {
            return None;
        }
        let imported_tables = self.import_counts().1 as u32;
        if idx < imported_tables {
            self.imported_table(idx)
                .and_then(|table| table.lock().ok().map(|table| table.clone()))
        } else {
            self.tables
                .get((idx - imported_tables) as usize)
                .and_then(|table| table.lock().ok().map(|table| table.clone()))
        }
    }

    /// Returns a shared table binding by index.
    pub fn table_binding(&self, idx: u32) -> Option<SharedTable> {
        if self.ensure_initialised().is_err() {
            return None;
        }
        let imported_tables = self.import_counts().1 as u32;
        if idx < imported_tables {
            self.imported_table(idx)
        } else {
            self.tables.get((idx - imported_tables) as usize).cloned()
        }
    }

    /// Returns a shared memory binding by index.
    pub fn memory_binding(&self, idx: u32) -> Option<SharedMemory> {
        if self.ensure_initialised().is_err() {
            return None;
        }
        let imported_memories = self.import_counts().0 as u32;
        if idx < imported_memories {
            self.imported_memory(idx)
        } else {
            self.memories
                .get((idx - imported_memories) as usize)
                .cloned()
        }
    }

    /// Replaces the table at the given index.
    pub fn set_table(&mut self, idx: u32, table: Table) -> Result<()> {
        self.ensure_initialised()?;
        self.ensure_jit_inactive_for_external_mutation()?;
        let imported_tables = self.import_counts().1 as u32;
        if idx < imported_tables {
            let shared = self
                .imported_table(idx)
                .ok_or_else(|| WasmError::Runtime(format!("table {} not found", idx)))?;
            *shared.lock().map_err(poisoned_lock)? = table;
            Ok(())
        } else {
            let slot = (idx - imported_tables) as usize;
            let target = self
                .tables
                .get(slot)
                .ok_or_else(|| WasmError::Runtime(format!("table {} not found", idx)))?;
            *target.lock().map_err(poisoned_lock)? = table;
            Ok(())
        }
    }

    /// Adds global.
    pub fn add_global(&mut self, global: Global) -> u32 {
        self.custom_globals = true;
        let idx = (self.import_counts().2 + self.globals.len()) as u32;
        self.globals.push(global);
        idx
    }

    /// Returns global.
    pub fn get_global(&self, idx: u32) -> Option<Global> {
        if self.ensure_initialised().is_err() {
            return None;
        }
        let imported_globals = self.import_counts().2 as u32;
        if idx < imported_globals {
            self.imported_global(idx)
                .and_then(|global| global.lock().ok().map(|global| global.clone()))
        } else {
            self.globals.get((idx - imported_globals) as usize).cloned()
        }
    }

    /// Returns global mut.
    pub fn get_global_mut(&mut self, idx: u32) -> Option<&mut Global> {
        self.custom_globals = true;
        let imported_globals = self.import_counts().2 as u32;
        if idx < imported_globals {
            None
        } else {
            self.globals.get_mut((idx - imported_globals) as usize)
        }
    }

    /// Returns func count.
    pub fn get_func_count(&self) -> u32 {
        if self.ensure_initialised().is_err() {
            return 0;
        }
        self.module.func_count()
    }

    /// Grows the selected memory by the requested number of pages.
    pub fn grow_memory(&mut self, memory_idx: u32, delta: u32) -> Result<u32> {
        self.ensure_initialised()?;
        self.ensure_jit_inactive_for_external_mutation()?;
        self.resolve_memory_growth_target(memory_idx)?;

        if let Some(memory) = self.imported_memory(memory_idx) {
            return memory.lock().map_err(poisoned_lock)?.grow(delta);
        }

        let imported_memories = self.import_counts().0 as u32;
        self.memories
            .get((memory_idx - imported_memories) as usize)
            .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))?
            .lock()
            .map_err(poisoned_lock)?
            .grow(delta)
    }

    /// Returns or updates memory grow wasm.
    pub fn memory_grow_wasm(&mut self, memory_idx: u32, delta: i32) -> Result<i32> {
        self.ensure_initialised()?;
        self.resolve_memory_growth_target(memory_idx)?;

        let Ok(delta) = u32::try_from(delta) else {
            return Ok(-1);
        };

        let imported_memories = self.import_counts().0 as u32;
        if memory_idx < imported_memories {
            let memory = self
                .imported_memory(memory_idx)
                .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))?;
            return match memory.lock().map_err(poisoned_lock)?.grow(delta) {
                Ok(old_size) => Ok(old_size as i32),
                Err(WasmError::Runtime(_))
                | Err(WasmError::Trap(crate::runtime::TrapCode::MemoryLimitExceeded)) => Ok(-1),
                Err(error) => Err(error),
            };
        }

        let local_idx = (memory_idx - imported_memories) as usize;
        match self
            .memories
            .get(local_idx)
            .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))?
            .lock()
            .map_err(poisoned_lock)?
            .grow(delta)
        {
            Ok(old_size) => Ok(old_size as i32),
            Err(WasmError::Runtime(_))
            | Err(WasmError::Trap(crate::runtime::TrapCode::MemoryLimitExceeded)) => Ok(-1),
            Err(error) => Err(error),
        }
    }

    /// Returns or updates memory size.
    pub fn memory_size(&self, memory_idx: u32) -> Result<i32> {
        self.ensure_initialised()?;
        let imported_memories = self.import_counts().0 as u32;
        if memory_idx < imported_memories {
            let memory = self
                .imported_memory(memory_idx)
                .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))?;
            return Ok(memory.lock().map_err(poisoned_lock)?.size() as i32);
        }

        self.memories
            .get((memory_idx - imported_memories) as usize)
            .and_then(|memory| memory.lock().ok().map(|memory| memory.size() as i32))
            .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))
    }

    /// Returns func type.
    pub fn get_func_type(&self, func_idx: u32) -> Option<&crate::runtime::FunctionType> {
        self.module.func_type(func_idx)
    }

    fn resolve_memory_growth_target(&self, memory_idx: u32) -> Result<()> {
        let imported_memories = self.import_counts().0 as u32;
        if memory_idx < imported_memories {
            self.imported_memory(memory_idx)
                .map(|_| ())
                .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))
        } else {
            self.memories
                .get((memory_idx - imported_memories) as usize)
                .map(|_| ())
                .ok_or_else(|| WasmError::Runtime("memory not found".to_string()))
        }
    }

    fn import_counts(&self) -> (usize, usize, usize) {
        let mut memories = 0usize;
        let mut tables = 0usize;
        let mut globals = 0usize;
        for import in &self.module.imports {
            match import.kind {
                crate::runtime::ImportKind::Memory(_) => memories += 1,
                crate::runtime::ImportKind::Table(_) => tables += 1,
                crate::runtime::ImportKind::Global(_) => globals += 1,
                crate::runtime::ImportKind::Func(_) => {}
                crate::runtime::ImportKind::Tag(..) => {}
            }
        }
        (memories, tables, globals)
    }

    fn imports_ready(&self) -> bool {
        self.imports.iter().all(Option::is_some)
    }

    fn initialise_defined_allocations(&mut self) -> Result<()> {
        if self.memories.is_empty() {
            for memory_type in self.module.memories.iter().cloned() {
                let memory = crate::memory::Memory::new(memory_type)?;
                self.memories.push(Arc::new(Mutex::new(memory)));
            }
        }
        if self.tables.is_empty() {
            self.tables.extend(
                self.module
                    .tables
                    .iter()
                    .cloned()
                    .map(Table::new)
                    .map(|table| Arc::new(Mutex::new(table))),
            );
        }
        Ok(())
    }

    fn initialise_globals_without_imports(&mut self) -> Result<()> {
        if !self.globals.is_empty()
            || self
                .module
                .imports
                .iter()
                .any(|import| matches!(import.kind, ImportKind::Global(_)))
        {
            return Ok(());
        }

        let imported_global_count = self
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.kind, ImportKind::Global(_)))
            .count();
        let mut const_globals = vec![None; imported_global_count];

        for (index, global_type) in self.module.globals.iter().enumerate() {
            let init = self.module.global_inits.get(index).ok_or_else(|| {
                WasmError::Instantiate(format!("missing init for global {}", index))
            })?;
            let value = evaluate_const_expr_with_globals(init, &const_globals)?;
            let global = Global::new(global_type.clone(), value)?;
            self.globals.push(global.clone());
            const_globals.push((!global_type.mutable).then_some(global));
        }

        Ok(())
    }

    fn materialise_defined_state_from_instance(&mut self) -> Result<()> {
        self.ensure_initialised()?;
        let (imported_memories, imported_tables, imported_globals) = self.import_counts();
        let imports = self.ordered_imports();
        let instance = Instance::with_imports_and_store(
            Arc::new(self.module.clone()),
            &imports,
            self.store.clone(),
        )?;

        if !self.custom_memories {
            self.memories = instance
                .memories
                .iter()
                .skip(imported_memories)
                .cloned()
                .collect();
        }
        if !self.custom_tables {
            self.tables = instance
                .tables
                .iter()
                .skip(imported_tables)
                .cloned()
                .collect();
        }
        if !self.custom_globals {
            self.globals = instance
                .globals
                .iter()
                .skip(imported_globals)
                .map(|global| {
                    global
                        .lock()
                        .map_err(poisoned_lock)
                        .map(|global| global.clone())
                })
                .collect::<Result<Vec<_>>>()?;
        }

        // Cache the instance for reuse across invoke_function calls.
        // Install the defined memories/tables/globals into it.
        let instance = Arc::new(Mutex::new(instance));
        {
            let mut guard = instance.lock().map_err(poisoned_lock)?;
            for (offset, memory) in self.memories.iter().cloned().enumerate() {
                let target = imported_memories + offset;
                if target >= guard.memories.len() {
                    guard.memories.push(memory);
                } else {
                    guard.memories[target] = memory;
                }
            }
            for (offset, table) in self.tables.iter().cloned().enumerate() {
                let target = imported_tables + offset;
                if target >= guard.tables.len() {
                    guard.tables.push(table);
                } else {
                    guard.tables[target] = table;
                }
            }
            for (offset, global) in self.globals.iter().cloned().enumerate() {
                let target = imported_globals + offset;
                if target >= guard.globals.len() {
                    guard.globals.push(Arc::new(Mutex::new(global)));
                } else {
                    guard.globals[target] = Arc::new(Mutex::new(global));
                }
            }
        }
        self.cached_instance = Some(instance);

        Ok(())
    }

    fn imported_memory(&self, idx: u32) -> Option<Arc<Mutex<Memory>>> {
        let mut memory_idx = 0u32;
        for (import_idx, import) in self.module.imports.iter().enumerate() {
            if !matches!(import.kind, ImportKind::Memory(_)) {
                continue;
            }
            if memory_idx == idx {
                return match self.imports.get(import_idx)?.as_ref()? {
                    Extern::Memory(memory) => Some(memory.clone()),
                    _ => None,
                };
            }
            memory_idx += 1;
        }
        None
    }

    fn imported_table(&self, idx: u32) -> Option<Arc<Mutex<Table>>> {
        let mut table_idx = 0u32;
        for (import_idx, import) in self.module.imports.iter().enumerate() {
            if !matches!(import.kind, ImportKind::Table(_)) {
                continue;
            }
            if table_idx == idx {
                return match self.imports.get(import_idx)?.as_ref()? {
                    Extern::Table(table) => Some(table.clone()),
                    _ => None,
                };
            }
            table_idx += 1;
        }
        None
    }

    fn imported_global(&self, idx: u32) -> Option<Arc<Mutex<Global>>> {
        let mut global_idx = 0u32;
        for (import_idx, import) in self.module.imports.iter().enumerate() {
            if !matches!(import.kind, ImportKind::Global(_)) {
                continue;
            }
            if global_idx == idx {
                return match self.imports.get(import_idx)?.as_ref()? {
                    Extern::Global(global) => Some(global.clone()),
                    _ => None,
                };
            }
            global_idx += 1;
        }
        None
    }

    fn ordered_imports(&self) -> Vec<(&str, &str, Extern)> {
        self.module
            .imports
            .iter()
            .enumerate()
            .filter_map(|(idx, import)| {
                self.imports[idx]
                    .as_ref()
                    .cloned()
                    .map(|extern_| (import.module.as_str(), import.name.as_str(), extern_))
            })
            .collect()
    }

    fn ensure_initialised(&self) -> Result<()> {
        if let Some(error) = &self.initialisation_error {
            return Err(error.clone());
        }

        Ok(())
    }

    fn ensure_jit_inactive_for_external_mutation(&self) -> Result<()> {
        Ok(())
    }

    fn validate_import_binding(
        &self,
        import_idx: usize,
        module: &str,
        name: &str,
        extern_: Extern,
    ) -> Result<Extern> {
        let import_kind = &self
            .module
            .imports
            .get(import_idx)
            .ok_or_else(|| {
                WasmError::Instantiate(format!("import index {} out of bounds", import_idx))
            })?
            .kind;

        match (import_kind, extern_) {
            (ImportKind::Func(type_idx), Extern::HostFunc(func)) => {
                let func_type = self
                    .module
                    .type_at(*type_idx)
                    .ok_or_else(|| WasmError::Instantiate(format!("type {} not found", type_idx)))?
                    .clone();
                let actual = func.function_type().ok_or_else(|| {
                    WasmError::Instantiate(format!(
                        "import {}.{} host function type is required",
                        module, name
                    ))
                })?;
                if actual != &func_type {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} function type mismatch",
                        module, name
                    )));
                }
                Ok(Extern::HostFunc(Arc::new(TypedHostImport::new(
                    func, func_type,
                ))))
            }
            (ImportKind::Func(type_idx), Extern::Func(func)) => {
                let func_type = self
                    .module
                    .type_at(*type_idx)
                    .ok_or_else(|| WasmError::Instantiate(format!("type {} not found", type_idx)))?
                    .clone();
                if func.func_type != func_type {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} function type mismatch",
                        module, name
                    )));
                }
                Ok(Extern::HostFunc(Arc::new(TypedHostImport::new(
                    func.into_host_func(),
                    func_type,
                ))))
            }
            (ImportKind::Table(expected), Extern::Table(table)) => {
                let table_matches = {
                    let table_guard = table.lock().map_err(poisoned_lock)?;
                    table_matches_required(&table_guard, expected)
                };
                if !table_matches {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} table type mismatch",
                        module, name
                    )));
                }
                Ok(Extern::Table(table))
            }
            (ImportKind::Memory(expected), Extern::Memory(memory)) => {
                let memory_guard = memory.lock().map_err(poisoned_lock)?;
                if !memory_matches_required(&memory_guard, expected) {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} memory type mismatch",
                        module, name
                    )));
                }
                drop(memory_guard);
                Ok(Extern::Memory(memory))
            }
            (ImportKind::Global(expected), Extern::Global(global)) => {
                if global.lock().map_err(poisoned_lock)?.type_ != *expected {
                    return Err(WasmError::Instantiate(format!(
                        "import {}.{} global type mismatch",
                        module, name
                    )));
                }
                Ok(Extern::Global(global))
            }
            _ => Err(WasmError::Instantiate(format!(
                "import {}.{} kind mismatch",
                module, name
            ))),
        }
    }
}

impl std::fmt::Debug for LoadedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedModule")
            .field(
                "imports",
                &self
                    .imports
                    .iter()
                    .filter(|binding| binding.is_some())
                    .count(),
            )
            .field("memories", &self.memories.len())
            .field("tables", &self.tables)
            .field("globals", &self.globals)
            .field("exports", &self.exports)
            .finish()
    }
}

impl Drop for LoadedModule {
    fn drop(&mut self) {
        if self.attached_regions.is_empty() {
            return;
        }

        let regions: Vec<SharedRegionId> = std::mem::take(&mut self.attached_regions);
        let mut shared_memory = self.shared_memory.lock();

        for region_id in regions {
            if let Some(memory) = self.memories.first()
                && let Ok(mut memory) = memory.lock()
            {
                let _ = shared_memory.detach_region(&mut memory, region_id);
            }
        }
    }
}

impl Engine {
    /// Creates a new `Engine`.
    pub fn new() -> Self {
        let shared_store = Arc::new(Mutex::new(crate::runtime::Store::new()));
        Self {
            loader: EngineLoader::with_store(shared_store),
            modules: Vec::new(),
        }
    }

    /// Creates an `Engine` backed by the given store, sharing its
    /// `SharedMemoryRegistry` with all modules loaded through this runtime.
    pub fn with_store(store: Arc<Mutex<crate::runtime::Store>>) -> Self {
        Self {
            loader: EngineLoader::with_store(store),
            modules: Vec::new(),
        }
    }

    /// Loads module.
    pub fn load_module(&mut self, data: &[u8]) -> Result<u32> {
        let module = self.loader.load(data)?;
        let module_idx = self.modules.len() as u32;
        self.modules.push(Box::new(module));
        Ok(module_idx)
    }

    /// Returns module.
    pub fn get_module(&self, idx: u32) -> Option<&LoadedModule> {
        self.modules.get(idx as usize).map(Box::as_ref)
    }

    /// Returns module mut.
    pub fn get_module_mut(&mut self, idx: u32) -> Option<&mut LoadedModule> {
        self.modules.get_mut(idx as usize).map(Box::as_mut)
    }

    /// Invokes the target function.
    pub fn call(
        &mut self,
        module_idx: u32,
        func_idx: u32,
        args: &[WasmValue],
    ) -> Result<Vec<WasmValue>> {
        let module = self
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.invoke_function(func_idx, args)
    }

    /// Returns or updates memory grow.
    pub fn memory_grow(&mut self, module_idx: u32, delta: u32) -> Result<i32> {
        let module = self
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.grow_memory(0, delta).map(|old_size| old_size as i32)
    }

    /// Returns or updates memory size.
    pub fn memory_size(&self, module_idx: u32) -> Result<i32> {
        let module = self
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.memory_size(0)
    }

    /// Returns or updates table grow.
    pub fn table_grow(&mut self, module_idx: u32, table_idx: u32, delta: u32) -> Result<i32> {
        let module = self
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        let imported_tables = module.import_counts().1 as u32;

        if table_idx < imported_tables {
            if let Some(table) = module.imported_table(table_idx) {
                return table
                    .lock()
                    .map_err(poisoned_lock)?
                    .grow(delta)
                    .map(|old_size| old_size as i32)
                    .or(Ok(-1));
            }
            return Err(WasmError::Runtime("table not found".into()));
        }

        if let Some(table) = module
            .tables
            .get_mut((table_idx - imported_tables) as usize)
        {
            table
                .lock()
                .map_err(poisoned_lock)?
                .grow(delta)
                .map(|old_size| old_size as i32)
                .or(Ok(-1))
        } else {
            Err(WasmError::Runtime("table not found".into()))
        }
    }

    /// Returns or updates table size.
    pub fn table_size(&self, module_idx: u32, table_idx: u32) -> Result<i32> {
        let module = self
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        let imported_tables = module.import_counts().1 as u32;

        if table_idx < imported_tables {
            if let Some(table) = module.imported_table(table_idx) {
                return Ok(table.lock().map_err(poisoned_lock)?.size() as i32);
            }
            return Err(WasmError::Runtime("table not found".into()));
        }

        if let Some(table) = module.tables.get((table_idx - imported_tables) as usize) {
            Ok(table.lock().map_err(poisoned_lock)?.size() as i32)
        } else {
            Err(WasmError::Runtime("table not found".into()))
        }
    }

    /// Returns global value.
    pub fn get_global_value(&self, module_idx: u32, global_idx: u32) -> Result<WasmValue> {
        let module = self
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        let imported_globals = module.import_counts().2 as u32;

        if global_idx < imported_globals {
            if let Some(global) = module.imported_global(global_idx) {
                return Ok(global.lock().map_err(poisoned_lock)?.get());
            }
            return Err(WasmError::Runtime("global not found".into()));
        }

        if let Some(global) = module.globals.get((global_idx - imported_globals) as usize) {
            Ok(global.value)
        } else {
            Err(WasmError::Runtime("global not found".into()))
        }
    }

    /// Sets global value.
    pub fn set_global_value(
        &mut self,
        module_idx: u32,
        global_idx: u32,
        value: WasmValue,
    ) -> Result<()> {
        let module = self
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        let imported_globals = module.import_counts().2 as u32;

        if global_idx < imported_globals {
            if let Some(global) = module.imported_global(global_idx) {
                return global.lock().map_err(poisoned_lock)?.set(value);
            }
            return Err(WasmError::Runtime("global not found".into()));
        }

        if let Some(global) = module
            .globals
            .get_mut((global_idx - imported_globals) as usize)
        {
            global.set(value)
        } else {
            Err(WasmError::Runtime("global not found".into()))
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates aot module from wasm.
pub fn create_module_from_wasm(module: &Module) -> LoadedModule {
    LoadedModule::from_module(module)
}

/// Validates aot data.
pub fn validate_wasm(data: &[u8]) -> Result<()> {
    let loader = EngineLoader::new();
    loader.validate(data)
}

fn evaluate_const_expr_with_globals(expr: &[u8], globals: &[Option<Global>]) -> Result<WasmValue> {
    let mut reader = crate::loader::BinaryReader::from_slice(expr);
    let mut stack = Vec::new();

    loop {
        let opcode = reader.read_u8().map_err(WasmError::from)?;
        match opcode {
            0x0B => break,
            0x23 => {
                let idx = reader.read_uleb128().map_err(WasmError::from)? as usize;
                let value = globals
                    .get(idx)
                    .and_then(|global| global.clone())
                    .ok_or_else(|| {
                        WasmError::Instantiate(format!(
                            "global.get requires immutable global {} to be registered",
                            idx
                        ))
                    })?
                    .get();
                stack.push(value);
            }
            0x41 => stack.push(WasmValue::I32(
                reader.read_sleb128().map_err(WasmError::from)?,
            )),
            0x42 => stack.push(WasmValue::I64(
                reader.read_sleb128_i64().map_err(WasmError::from)?,
            )),
            0x43 => stack.push(WasmValue::F32(reader.read_f32().map_err(WasmError::from)?)),
            0x44 => stack.push(WasmValue::F64(reader.read_f64().map_err(WasmError::from)?)),
            0xD0 => stack.push(match reader.read_u8().map_err(WasmError::from)? {
                0x70 => WasmValue::NullRef(crate::runtime::RefType::FuncRef),
                0x6F => WasmValue::NullRef(crate::runtime::RefType::ExternRef),
                value => {
                    return Err(WasmError::Instantiate(format!(
                        "invalid ref.null type: {:02x}",
                        value
                    )));
                }
            }),
            0xD2 => stack.push(WasmValue::FuncRef(
                reader.read_uleb128().map_err(WasmError::from)?,
            )),
            0x6A => {
                let rhs = pop_i32_const(&mut stack)?;
                let lhs = pop_i32_const(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_add(rhs)));
            }
            0x6B => {
                let rhs = pop_i32_const(&mut stack)?;
                let lhs = pop_i32_const(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_sub(rhs)));
            }
            0x6C => {
                let rhs = pop_i32_const(&mut stack)?;
                let lhs = pop_i32_const(&mut stack)?;
                stack.push(WasmValue::I32(lhs.wrapping_mul(rhs)));
            }
            0x7C => {
                let rhs = pop_i64_const(&mut stack)?;
                let lhs = pop_i64_const(&mut stack)?;
                stack.push(WasmValue::I64(lhs.wrapping_add(rhs)));
            }
            0x7D => {
                let rhs = pop_i64_const(&mut stack)?;
                let lhs = pop_i64_const(&mut stack)?;
                stack.push(WasmValue::I64(lhs.wrapping_sub(rhs)));
            }
            0x7E => {
                let rhs = pop_i64_const(&mut stack)?;
                let lhs = pop_i64_const(&mut stack)?;
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

fn pop_i32_const(stack: &mut Vec<WasmValue>) -> Result<i32> {
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

fn pop_i64_const(stack: &mut Vec<WasmValue>) -> Result<i64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RefType;
    use crate::runtime::{GlobalType, Limits, NumType, TableType, ValType};

    struct WrongSigHostFunc;

    impl HostFunc for WrongSigHostFunc {
        fn call(
            &self,
            _caller: &mut HostCaller<'_>,
            _args: &[WasmValue],
        ) -> Result<Vec<WasmValue>> {
            Ok(vec![WasmValue::I32(0)])
        }

        fn function_type(&self) -> Option<&FunctionType> {
            static FUNC_TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
            Some(
                FUNC_TYPE
                    .get_or_init(|| FunctionType::new(vec![ValType::Num(NumType::I32)], vec![])),
            )
        }
    }

    struct EmptyHostFunc;

    impl HostFunc for EmptyHostFunc {
        fn call(
            &self,
            _caller: &mut HostCaller<'_>,
            _args: &[WasmValue],
        ) -> Result<Vec<WasmValue>> {
            Ok(vec![])
        }

        fn function_type(&self) -> Option<&FunctionType> {
            static FUNC_TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
            Some(FUNC_TYPE.get_or_init(FunctionType::empty))
        }
    }

    struct UntypedHostFunc;

    impl HostFunc for UntypedHostFunc {
        fn call(
            &self,
            _caller: &mut HostCaller<'_>,
            _args: &[WasmValue],
        ) -> Result<Vec<WasmValue>> {
            Ok(vec![])
        }

        fn function_type(&self) -> Option<&FunctionType> {
            None
        }
    }

    #[test]
    fn test_aot_module_creation() {
        let module = LoadedModule::from_module(&Module::new());
        assert!(module.memories.is_empty());
    }

    #[test]
    fn test_get_func_count_reports_module_functions() {
        let mut module = Module::new();
        module.types.push(FunctionType::empty());
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });
        module.funcs.push(crate::runtime::Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x0B],
        });

        let aot_module = LoadedModule::from_module(&module);
        assert_eq!(aot_module.get_func_count(), 2);
    }

    #[test]
    fn test_table_management() {
        let mut module = LoadedModule::from_module(&Module::new());
        let table = Table::new(TableType::new(RefType::FuncRef, Limits::Min(10)));
        let idx = module.add_table(table);
        assert_eq!(idx, 0);
        assert!(module.get_table(0).is_some());
    }

    #[test]
    fn test_global_management() {
        let mut module = LoadedModule::from_module(&Module::new());
        let global = Global::new(
            GlobalType::new(ValType::Num(NumType::I32), true),
            WasmValue::I32(42),
        )
        .unwrap();
        let idx = module.add_global(global);
        assert_eq!(idx, 0);
        assert!(module.get_global(0).is_some());
    }

    #[test]
    fn test_runtime() {
        let runtime = Engine::new();
        assert_eq!(runtime.modules.len(), 0);
    }

    #[test]
    fn test_validate_wasm() {
        let valid_data = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
        assert!(validate_wasm(&valid_data).is_ok());

        let invalid_data = vec![0x00, 0x00, 0x00, 0x00];
        assert!(validate_wasm(&invalid_data).is_err());

        let short_data = vec![0x00, 0x61];
        assert!(validate_wasm(&short_data).is_err());

        let truncated_valid_magic = vec![0x00, 0x61, 0x73, 0x6D];
        assert!(validate_wasm(&truncated_valid_magic).is_err());
    }

    #[test]
    fn test_memory_grow() {
        let mut runtime = Engine::new();
        let module_idx = runtime
            .load_module(&[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00])
            .unwrap();

        {
            let aot_module = runtime.get_module_mut(module_idx).unwrap();
            let mem_type = crate::runtime::MemoryType::new(crate::runtime::Limits::Min(1));
            let memory = crate::memory::Memory::new(mem_type).unwrap();
            aot_module.set_memory(memory);
        }

        let result = runtime.memory_grow(module_idx, 1);
        assert!(result.is_ok());
    }

    #[test]
    fn test_memory_size_reads_imported_memory() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(1),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let mut memory = crate::memory::Memory::new(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ))
        .unwrap();
        memory.grow(1).unwrap();
        aot_module
            .register_memory_import("env", "memory", memory)
            .unwrap();

        let mut runtime = Engine::new();
        runtime.modules.push(Box::new(aot_module));

        assert_eq!(runtime.memory_size(0).unwrap(), 2);
    }

    #[test]
    fn test_state_is_materialised_after_import_registration() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });
        module.memories.push(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ));
        module
            .globals
            .push(GlobalType::new(ValType::Num(NumType::I32), false));
        module.global_inits.push(vec![0x41, 0x07, 0x0B]);

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_host_import(
                "env",
                "host",
                Box::new(EmptyHostFunc),
                FunctionType::empty(),
            )
            .unwrap();

        let mut runtime = Engine::new();
        runtime.modules.push(Box::new(aot_module));

        assert_eq!(runtime.memory_size(0).unwrap(), 1);
        assert_eq!(runtime.get_global_value(0, 0).unwrap(), WasmValue::I32(7));
    }

    fn module_with_memory() -> Module {
        let mut module = Module::new();
        module.memories.push(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ));
        module
    }

    #[test]
    fn test_aot_module_drop_detaches_shared_regions() {
        use crate::memory::{PAGE_SIZE_BYTES, RegionProt};

        let (shared_memory, region_id) = {
            let mut module = LoadedModule::from_module(&module_with_memory());
            let shared_memory = module.shared_memory.clone();
            let (region_id, _page_offset) = module
                .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
                .unwrap();
            (shared_memory, region_id)
        };

        shared_memory.lock().destroy_region(region_id).unwrap();
    }

    #[test]
    fn test_aot_shared_region_access_detach_and_alignment_failures() {
        use crate::memory::{PAGE_SIZE_BYTES, RegionProt};

        let mut module = LoadedModule::from_module(&module_with_memory());
        let (region_id, _page_offset) = module
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        // Write and read via host-side API
        module
            .write_shared_region(region_id, 0, &21i32.to_le_bytes())
            .unwrap();
        module
            .write_shared_region(region_id, 4, &31i32.to_le_bytes())
            .unwrap();

        let mut buf = [0u8; 4];
        module.read_shared_region(region_id, 0, &mut buf).unwrap();
        assert_eq!(i32::from_le_bytes(buf), 21);

        let mut buf8 = [0u8; 8];
        module.read_shared_region(region_id, 0, &mut buf8).unwrap();
        let val0 = i32::from_le_bytes([buf8[0], buf8[1], buf8[2], buf8[3]]);
        let val1 = i32::from_le_bytes([buf8[4], buf8[5], buf8[6], buf8[7]]);
        assert_eq!(val0, 21);
        assert_eq!(val1, 31);

        module.detach_shared_region(region_id).unwrap();

        // After detach, host-side access to the region still works (region exists)
        let mut buf = [0u8; 4];
        module.read_shared_region(region_id, 0, &mut buf).unwrap();
        assert_eq!(i32::from_le_bytes(buf), 21);

        module.destroy_shared_region(region_id).unwrap();
    }

    #[test]
    fn test_aot_shared_region_visibility_across_modules() {
        use crate::memory::{PAGE_SIZE_BYTES, RegionProt};

        let store = Arc::new(Mutex::new(crate::runtime::Store::new()));
        let mut first =
            LoadedModule::from_module_with_store(&module_with_memory(), store.clone()).unwrap();
        let mut second =
            LoadedModule::from_module_with_store(&module_with_memory(), store).unwrap();

        let (region_id, _first_page_offset) = first
            .allocate_shared_region(PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();
        let _second_page_offset = second
            .attach_shared_region(region_id, RegionProt::ReadWrite, None)
            .unwrap();

        first
            .write_shared_region(region_id, 0, &33i32.to_le_bytes())
            .unwrap();

        let mut buf = [0u8; 4];
        second.read_shared_region(region_id, 0, &mut buf).unwrap();
        assert_eq!(i32::from_le_bytes(buf), 33);

        second
            .write_shared_region(region_id, 4, &[1, 2, 3, 4])
            .unwrap();
        let mut buf = [0u8; 4];
        first.read_shared_region(region_id, 4, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        first.detach_shared_region(region_id).unwrap();
        second.detach_shared_region(region_id).unwrap();
        first.destroy_shared_region(region_id).unwrap();
    }

    #[test]
    fn test_getters_resolve_imported_state() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(1),
            )),
        });
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: crate::runtime::ImportKind::Table(TableType::new(
                RefType::FuncRef,
                Limits::Min(1),
            )),
        });
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_memory_import(
                "env",
                "memory",
                crate::memory::Memory::new(crate::runtime::MemoryType::new(
                    crate::runtime::Limits::Min(1),
                ))
                .unwrap(),
            )
            .unwrap();
        aot_module
            .register_table_import(
                "env",
                "table",
                Table::new(TableType::new(RefType::FuncRef, Limits::Min(1))),
            )
            .unwrap();
        aot_module
            .register_global_import(
                "env",
                "global",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), false),
                    WasmValue::I32(9),
                )
                .unwrap(),
            )
            .unwrap();

        assert!(aot_module.get_memory().is_some());
        assert!(aot_module.get_table(0).is_some());
        assert_eq!(aot_module.get_global(0).unwrap().get(), WasmValue::I32(9));
    }

    #[test]
    fn test_add_table_returns_combined_index_space() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: crate::runtime::ImportKind::Table(TableType::new(
                RefType::FuncRef,
                Limits::Min(1),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_table_import(
                "env",
                "table",
                Table::new(TableType::new(RefType::FuncRef, Limits::Min(1))),
            )
            .unwrap();

        let idx =
            aot_module.add_table(Table::new(TableType::new(RefType::FuncRef, Limits::Min(2))));

        assert_eq!(idx, 1);
        assert_eq!(aot_module.get_table(idx).unwrap().size(), 2);
    }

    #[test]
    fn test_add_global_returns_combined_index_space() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_global_import(
                "env",
                "global",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), false),
                    WasmValue::I32(9),
                )
                .unwrap(),
            )
            .unwrap();

        let idx = aot_module.add_global(
            Global::new(
                GlobalType::new(ValType::Num(NumType::I32), true),
                WasmValue::I32(42),
            )
            .unwrap(),
        );

        assert_eq!(idx, 1);
        assert_eq!(
            aot_module.get_global(idx).unwrap().get(),
            WasmValue::I32(42)
        );
        assert!(aot_module.get_global_mut(0).is_none());
        assert_eq!(
            aot_module.get_global_mut(idx).unwrap().get(),
            WasmValue::I32(42)
        );
    }

    #[test]
    fn test_invoke_function_preserves_all_memories() {
        let mut module = Module::new();
        module
            .types
            .push(crate::runtime::FunctionType::new(vec![], vec![]));
        module.funcs.push(crate::runtime::Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x0B],
        });
        module.memories.push(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ));
        module.memories.push(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ));

        let mut aot_module = LoadedModule::from_module(&module);
        let first = crate::memory::Memory::new(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ))
        .unwrap();
        let mut second = crate::memory::Memory::new(crate::runtime::MemoryType::new(
            crate::runtime::Limits::Min(1),
        ))
        .unwrap();
        second.write_u8(1, 5).unwrap();
        aot_module.memories = vec![
            std::sync::Arc::new(std::sync::Mutex::new(first)),
            std::sync::Arc::new(std::sync::Mutex::new(second)),
        ];

        aot_module.invoke_function(0, &[]).unwrap();

        assert_eq!(aot_module.memories.len(), 2);
        assert_eq!(
            aot_module.memories[1].lock().unwrap().read_u8(1).unwrap(),
            5
        );
    }

    #[test]
    fn test_imported_global_alias_is_preserved() {
        let mut module = Module::new();
        module
            .types
            .push(crate::runtime::FunctionType::new(vec![], vec![]));
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "g".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                true,
            )),
        });
        module.funcs.push(crate::runtime::Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x2A, 0x24, 0x00, 0x0B],
        });

        let shared = Arc::new(Mutex::new(
            Global::new(
                GlobalType::new(ValType::Num(NumType::I32), true),
                WasmValue::I32(0),
            )
            .unwrap(),
        ));
        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_import("env", "g", Extern::Global(shared.clone()))
            .unwrap();

        aot_module.invoke_function(0, &[]).unwrap();

        assert_eq!(shared.lock().unwrap().get(), WasmValue::I32(42));
    }

    #[test]
    fn test_set_global_value_respects_imported_global_invariants() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "g".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_global_import(
                "env",
                "g",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), false),
                    WasmValue::I32(1),
                )
                .unwrap(),
            )
            .unwrap();

        let mut runtime = Engine::new();
        runtime.modules.push(Box::new(aot_module));

        assert_eq!(runtime.get_global_value(0, 0).unwrap(), WasmValue::I32(1));
        assert!(runtime.set_global_value(0, 0, WasmValue::I32(2)).is_err());
        assert!(
            runtime
                .set_global_value(0, 0, WasmValue::FuncRef(0))
                .is_err()
        );
    }

    #[test]
    fn test_register_host_import_rejects_wrong_kind() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(1),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_host_import(
            "env",
            "memory",
            Box::new(EmptyHostFunc),
            FunctionType::empty(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_register_host_import_rejects_wrong_signature() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_host_import(
            "env",
            "host",
            Box::new(WrongSigHostFunc),
            FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_register_host_import_accepts_explicit_signature_for_untyped_host() {
        let mut module = Module::new();
        module.types.push(FunctionType::empty());
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_host_import(
            "env",
            "host",
            Box::new(UntypedHostFunc),
            FunctionType::empty(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_register_memory_import_rejects_too_small_memory_immediately() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(2),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_memory_import(
            "env",
            "memory",
            crate::memory::Memory::new(crate::runtime::MemoryType::new(
                crate::runtime::Limits::Min(1),
            ))
            .unwrap(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_register_memory_import_rejects_broader_max_immediately() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::MinMax(1, 2),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_memory_import(
            "env",
            "memory",
            crate::memory::Memory::new(crate::runtime::MemoryType::new(
                crate::runtime::Limits::MinMax(1, 3),
            ))
            .unwrap(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_register_memory_and_table_imports_accept_compatible_subtypes() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "memory".to_string(),
            kind: crate::runtime::ImportKind::Memory(crate::runtime::MemoryType::new(
                crate::runtime::Limits::MinMax(1, 4),
            )),
        });
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "table".to_string(),
            kind: crate::runtime::ImportKind::Table(TableType::new(
                RefType::FuncRef,
                crate::runtime::Limits::MinMax(1, 4),
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        assert!(
            aot_module
                .register_memory_import(
                    "env",
                    "memory",
                    crate::memory::Memory::new(crate::runtime::MemoryType::new(
                        crate::runtime::Limits::MinMax(2, 3),
                    ))
                    .unwrap(),
                )
                .is_ok()
        );
        assert!(
            aot_module
                .register_table_import(
                    "env",
                    "table",
                    Table::new(TableType::new(
                        RefType::FuncRef,
                        crate::runtime::Limits::MinMax(2, 3),
                    )),
                )
                .is_ok()
        );
    }

    #[test]
    fn test_register_global_import_rejects_wrong_type_immediately() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        let result = aot_module.register_global_import(
            "env",
            "global",
            Global::new(
                GlobalType::new(ValType::Num(NumType::I64), false),
                WasmValue::I64(1),
            )
            .unwrap(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_register_import_rejects_duplicate_binding() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "global".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                true,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_global_import(
                "env",
                "global",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), true),
                    WasmValue::I32(1),
                )
                .unwrap(),
            )
            .unwrap();

        let error = aot_module
            .register_global_import(
                "env",
                "global",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), true),
                    WasmValue::I32(2),
                )
                .unwrap(),
            )
            .unwrap_err();
        assert!(
            matches!(error, WasmError::Instantiate(message) if message.contains("already registered"))
        );
    }

    #[test]
    fn test_register_duplicate_named_imports_by_occurrence() {
        let mut module = Module::new();
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "shared".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "shared".to_string(),
            kind: crate::runtime::ImportKind::Global(GlobalType::new(
                ValType::Num(NumType::I32),
                false,
            )),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_global_import(
                "env",
                "shared",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), false),
                    WasmValue::I32(1),
                )
                .unwrap(),
            )
            .unwrap();
        aot_module
            .register_global_import(
                "env",
                "shared",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), false),
                    WasmValue::I32(2),
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(aot_module.get_global(0).unwrap().get(), WasmValue::I32(1));
        assert_eq!(aot_module.get_global(1).unwrap().get(), WasmValue::I32(2));
    }

    #[test]
    fn test_invoke_imported_function() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_host_import(
                "env",
                "host",
                Box::new(EmptyHostFunc),
                FunctionType::new(vec![], vec![]),
            )
            .unwrap_err();

        struct ImportedFunc;

        impl HostFunc for ImportedFunc {
            fn call(
                &self,
                _caller: &mut HostCaller<'_>,
                _args: &[WasmValue],
            ) -> Result<Vec<WasmValue>> {
                Ok(vec![WasmValue::I32(7)])
            }

            fn function_type(&self) -> Option<&FunctionType> {
                static FUNC_TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
                Some(
                    FUNC_TYPE.get_or_init(|| {
                        FunctionType::new(vec![], vec![ValType::Num(NumType::I32)])
                    }),
                )
            }
        }

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_host_import(
                "env",
                "host",
                Box::new(ImportedFunc),
                FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]),
            )
            .unwrap();

        let result = aot_module.invoke_function(0, &[]).unwrap();
        assert_eq!(result, vec![WasmValue::I32(7)]);
    }

    #[test]
    fn test_invoke_imported_function_rejects_result_type_mismatch() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.imports.push(crate::runtime::Import {
            module: "env".to_string(),
            name: "host".to_string(),
            kind: crate::runtime::ImportKind::Func(0),
        });

        let mut aot_module = LoadedModule::from_module(&module);
        aot_module
            .register_host_import(
                "env",
                "host",
                Box::new(UntypedHostFunc),
                FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]),
            )
            .unwrap();

        let error = aot_module.invoke_function(0, &[]).unwrap_err();
        assert!(
            matches!(error, WasmError::Runtime(message) if message.contains("result count mismatch"))
        );
    }

    #[test]
    fn test_table_operations() {
        let mut runtime = Engine::new();
        let module_idx = runtime
            .load_module(&[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00])
            .unwrap();

        {
            let aot_module = runtime.get_module_mut(module_idx).unwrap();
            let table = Table::new(TableType::new(RefType::FuncRef, Limits::Min(5)));
            aot_module.add_table(table);
        }

        let size = runtime.table_size(module_idx, 0).unwrap();
        assert_eq!(size, 5);

        let old_size = runtime.table_grow(module_idx, 0, 3).unwrap();
        assert_eq!(old_size, 5);

        let new_size = runtime.table_size(module_idx, 0).unwrap();
        assert_eq!(new_size, 8);
    }

    #[test]
    fn test_global_operations() {
        let mut runtime = Engine::new();
        let module_idx = runtime
            .load_module(&[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00])
            .unwrap();

        {
            let aot_module = runtime.get_module_mut(module_idx).unwrap();
            let global = Global::new(
                GlobalType::new(ValType::Num(NumType::I32), true),
                WasmValue::I32(100),
            )
            .unwrap();
            aot_module.add_global(global);
        }

        let value = runtime.get_global_value(module_idx, 0).unwrap();
        assert_eq!(value, WasmValue::I32(100));

        runtime
            .set_global_value(module_idx, 0, WasmValue::I32(200))
            .unwrap();

        let new_value = runtime.get_global_value(module_idx, 0).unwrap();
        assert_eq!(new_value, WasmValue::I32(200));
    }
}
