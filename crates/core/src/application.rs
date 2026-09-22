//! WasmApplication - High-level API for Wasm module execution.
//!
//! This module provides the main entry point for interacting with the WebAssembly
//! runtime. [`WasmApplication`] manages module loading, instantiation, host function
//! registration, and function invocation.
//!
//! # Example
//!
//! ```ignore
//! use wasmtiny::{WasmApplication, WasmValue};
//!
//! let mut app = WasmApplication::new();
//! let idx = app.load_module_from_file("hello.wasm")?;
//! app.instantiate(idx)?;
//! let result = app.call_function(idx, "main", &[])?;
//! ```

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use crate::{
    engine::runtime::{Engine, Export},
    memory::RegionProt,
    runtime::{
        FunctionType, HostFunc, Memory, Result, SharedRegionId, Store, WasmError, WasmValue,
    },
};

/// A WebAssembly application instance.
///
/// This is the main entry point for interacting with the WebAssembly runtime.
/// It manages module loading, instantiation, host function registration, and
/// function invocation.
///
/// # Example
///
/// ```ignore
/// use wasmtiny::{WasmApplication, WasmValue};
///
/// let mut app = WasmApplication::new();
/// let idx = app.load_module_from_file("module.wasm")?;
/// app.instantiate(idx)?;
/// let result = app.call_function(idx, "add", &[WasmValue::I32(1), WasmValue::I32(2)])?;
/// ```
pub struct WasmApplication {
    pub runtime: Engine,
}

impl WasmApplication {
    /// Creates a new `WasmApplication`.
    pub fn new() -> Self {
        Self {
            runtime: Engine::new(),
        }
    }

    /// Creates a `WasmApplication` backed by the given store, sharing its
    /// `SharedMemoryRegistry` with all modules loaded through this application.
    pub fn with_store(store: Arc<Mutex<Store>>) -> Self {
        Self {
            runtime: Engine::with_store(store),
        }
    }

    /// Loads module from file.
    pub fn load_module_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<u32> {
        let data = fs::read(path)?;
        self.load_module_from_memory(&data)
    }

    /// Loads module from memory.
    pub fn load_module_from_memory(&mut self, data: &[u8]) -> Result<u32> {
        self.runtime.load_module(data)
    }

    /// Instantiates the module and resolves its imports.
    pub fn instantiate(&mut self, module_idx: u32) -> Result<()> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Instantiate(format!("module {} not found", module_idx)))?;
        module.instantiate()
    }

    /// Registers host function.
    pub fn register_host_function(
        &mut self,
        module_idx: u32,
        import_module: &str,
        name: &str,
        func: Box<dyn HostFunc>,
        func_type: FunctionType,
    ) -> Result<()> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Instantiate(format!("module {} not found", module_idx)))?;
        module.register_host_import(import_module, name, func, func_type)
    }

    /// Returns an exported memory by name.
    pub fn export_memory(&self, module_idx: u32, name: &str) -> Result<Memory> {
        let module = self
            .runtime
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;

        match module.get_export(name) {
            Some(Export::Memory(0)) => module.get_memory().ok_or_else(|| {
                WasmError::Runtime(format!("memory export {} is unavailable", name))
            }),
            Some(Export::Memory(idx)) => Err(WasmError::Runtime(format!(
                "memory export {} uses unsupported memory index {}",
                name, idx
            ))),
            Some(_) => Err(WasmError::Runtime(format!(
                "export {} is not a memory",
                name
            ))),
            None => Err(WasmError::Runtime(format!(
                "memory export {} not found",
                name
            ))),
        }
    }

    /// Allocates a shared region and maps it into the module's memory.
    ///
    /// Returns `(region_id, page_offset)`.
    pub fn allocate_shared_region(
        &mut self,
        module_idx: u32,
        size: u32,
        prot: RegionProt,
    ) -> Result<(SharedRegionId, u32)> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.allocate_shared_region(size, prot)
    }

    /// Allocates a shared region without mapping it into any guest memory.
    pub fn allocate_shared_region_standalone(
        &mut self,
        module_idx: u32,
        size: u32,
    ) -> Result<SharedRegionId> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.allocate_shared_region_standalone(size)
    }

    /// Destroys shared region.
    pub fn destroy_shared_region(
        &mut self,
        module_idx: u32,
        region_id: SharedRegionId,
    ) -> Result<()> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.destroy_shared_region(region_id)
    }

    /// Returns the length of the shared region in bytes.
    pub fn shared_region_len(&self, module_idx: u32, region_id: SharedRegionId) -> Result<u32> {
        let module = self
            .runtime
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.shared_region_len(region_id)
    }

    /// Attaches an existing shared region to the module's memory.
    ///
    /// Returns the page offset where the region was mapped.
    pub fn attach_shared_region(
        &mut self,
        module_idx: u32,
        region_id: SharedRegionId,
        prot: RegionProt,
        reader_slot: Option<u32>,
    ) -> Result<u32> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.attach_shared_region(region_id, prot, reader_slot)
    }

    /// Detaches a shared region from the module's memory.
    pub fn detach_shared_region(
        &mut self,
        module_idx: u32,
        region_id: SharedRegionId,
    ) -> Result<()> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.detach_shared_region(region_id)
    }

    /// Writes data to a shared region from the host side.
    pub fn write_shared_region(
        &self,
        module_idx: u32,
        region_id: SharedRegionId,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        let module = self
            .runtime
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.write_shared_region(region_id, offset, data)
    }

    /// Reads data from a shared region from the host side.
    pub fn read_shared_region(
        &self,
        module_idx: u32,
        region_id: SharedRegionId,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        let module = self
            .runtime
            .get_module(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        module.read_shared_region(region_id, offset, buf)
    }

    /// Calls function.
    pub fn call_function(
        &mut self,
        module_idx: u32,
        func_name: &str,
        args: &[WasmValue],
    ) -> Result<Vec<WasmValue>> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;

        if let Some(Export::Function(func_idx)) = module.get_export(func_name).cloned() {
            module.invoke_function(func_idx, args)
        } else {
            Err(WasmError::Runtime(format!(
                "function {} not found",
                func_name
            )))
        }
    }

    /// Executes start.
    pub fn execute_start(&mut self, module_idx: u32) -> Result<()> {
        let module = self
            .runtime
            .get_module_mut(module_idx)
            .ok_or_else(|| WasmError::Runtime(format!("module {} not found", module_idx)))?;
        if let Some(start_idx) = module.start_function() {
            let _ = module.invoke_function(start_idx, &[])?;
        }

        Ok(())
    }
}

impl Default for WasmApplication {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{FunctionType, Global, GlobalType, HostCaller, NumType, ValType};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountingHostFunc;

    impl HostFunc for CountingHostFunc {
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
                    .get_or_init(|| FunctionType::new(vec![], vec![ValType::Num(NumType::I32)])),
            )
        }
    }

    struct StartHostFunc {
        calls: Arc<AtomicUsize>,
    }

    impl HostFunc for StartHostFunc {
        fn call(
            &self,
            _caller: &mut HostCaller<'_>,
            _args: &[WasmValue],
        ) -> Result<Vec<WasmValue>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        }

        fn function_type(&self) -> Option<&FunctionType> {
            static FUNC_TYPE: std::sync::OnceLock<FunctionType> = std::sync::OnceLock::new();
            Some(FUNC_TYPE.get_or_init(FunctionType::empty))
        }
    }

    #[test]
    fn test_application_creation() {
        let app = WasmApplication::new();
        assert_eq!(app.runtime.modules.len(), 0);
    }

    #[test]
    fn test_load_module_from_memory() {
        let mut app = WasmApplication::new();

        let wasm_data = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];

        let result = app.load_module_from_memory(&wasm_data);
        assert!(result.is_ok());

        let idx = result.unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_instantiate() {
        let mut app = WasmApplication::new();

        let wasm_data = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        let result = app.instantiate(idx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_shared_region_application_wrappers() {
        use crate::memory::{PAGE_SIZE_BYTES, RegionProt};

        let mut app = WasmApplication::new();
        let wasm_data = wat::parse_str("(module (memory 1))").unwrap();

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        app.instantiate(idx).unwrap();
        let (region_id, _page_offset) = app
            .allocate_shared_region(idx, PAGE_SIZE_BYTES, RegionProt::ReadWrite)
            .unwrap();

        app.write_shared_region(idx, region_id, 0, &41i32.to_le_bytes())
            .unwrap();
        app.write_shared_region(idx, region_id, 4, &59i32.to_le_bytes())
            .unwrap();

        let mut buf = [0u8; 8];
        app.read_shared_region(idx, region_id, 0, &mut buf).unwrap();
        let val0 = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let val1 = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(val0, 41);
        assert_eq!(val1, 59);

        app.detach_shared_region(idx, region_id).unwrap();
        app.destroy_shared_region(idx, region_id).unwrap();
    }

    #[test]
    fn test_instantiate_rejects_missing_imports() {
        let mut app = WasmApplication::new();
        let wasm_data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x02, 0x0C, 0x01, 0x03, b'e', b'n', b'v', 0x04, b'h', b'o', b's', b't', 0x00, 0x00,
        ];

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        assert!(app.instantiate(idx).is_err());
    }

    #[test]
    fn test_call_function_with_host_import() {
        let mut app = WasmApplication::new();
        let wasm_data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01,
            0x7F, 0x02, 0x0C, 0x01, 0x03, b'e', b'n', b'v', 0x04, b'h', b'o', b's', b't', 0x00,
            0x00, 0x03, 0x02, 0x01, 0x00, 0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00,
            0x01, 0x0A, 0x06, 0x01, 0x04, 0x00, 0x10, 0x00, 0x0B,
        ];

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        app.register_host_function(
            idx,
            "env",
            "host",
            Box::new(CountingHostFunc),
            FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]),
        )
        .unwrap();
        let first = app.call_function(idx, "main", &[]).unwrap();

        assert_eq!(first, vec![WasmValue::I32(0)]);
    }

    #[test]
    fn test_call_function_with_global_import() {
        let mut app = WasmApplication::new();
        let wasm_data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01,
            0x7F, 0x02, 0x0A, 0x01, 0x03, b'e', b'n', b'v', 0x01, b'g', 0x03, 0x7F, 0x01, 0x03,
            0x02, 0x01, 0x00, 0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00, 0x00, 0x0A,
            0x0A, 0x01, 0x08, 0x00, 0x41, 0x07, 0x24, 0x00, 0x23, 0x00, 0x0B,
        ];

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        let module = app.runtime.get_module_mut(idx).unwrap();
        module
            .register_global_import(
                "env",
                "g",
                Global::new(
                    GlobalType::new(ValType::Num(NumType::I32), true),
                    WasmValue::I32(0),
                )
                .unwrap(),
            )
            .unwrap();

        let results = app.call_function(idx, "main", &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(7)]);
    }

    #[test]
    fn test_execute_start_with_imported_function() {
        let mut app = WasmApplication::new();
        let wasm_data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x02, 0x0C, 0x01, 0x03, b'e', b'n', b'v', 0x04, b'i', b'n', b'i', b't', 0x00, 0x00,
            0x08, 0x01, 0x00,
        ];
        let calls = Arc::new(AtomicUsize::new(0));

        let idx = app.load_module_from_memory(&wasm_data).unwrap();
        app.register_host_function(
            idx,
            "env",
            "init",
            Box::new(StartHostFunc {
                calls: calls.clone(),
            }),
            FunctionType::empty(),
        )
        .unwrap();

        app.execute_start(idx).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
