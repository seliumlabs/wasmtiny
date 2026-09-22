use std::sync::{Arc, Mutex};

use super::runtime::{Export, LoadedModule};

use crate::{
    loader::{Parser, Validator},
    runtime::{Instance, Module, Result, WasmError},
};

/// Ahead-of-time module loader.
pub struct EngineLoader {
    parser: Parser,
    validator: Validator,
    store: Arc<Mutex<crate::runtime::Store>>,
}

impl EngineLoader {
    /// Creates a new `EngineLoader`.
    pub fn new() -> Self {
        Self::with_store(Arc::new(Mutex::new(crate::runtime::Store::new())))
    }

    /// Returns this value configured with store.
    pub fn with_store(store: Arc<Mutex<crate::runtime::Store>>) -> Self {
        Self {
            parser: Parser::new(),
            validator: Validator::new(),
            store,
        }
    }

    /// Loads a WebAssembly module into the target runtime representation.
    pub fn load(&self, data: &[u8]) -> Result<LoadedModule> {
        self.load_with_store(data, self.store.clone())
    }

    fn load_with_store(
        &self,
        data: &[u8],
        store: Arc<Mutex<crate::runtime::Store>>,
    ) -> Result<LoadedModule> {
        let module = self.parse_validated_module(data)?;
        self.convert_to_aot_module(&module, store)
    }

    /// Loads wasm.
    pub fn load_wasm(&self, data: &[u8]) -> Result<LoadedModule> {
        self.load(data)
    }

    /// Validates the provided WebAssembly module or binary input.
    pub fn validate(&self, data: &[u8]) -> Result<()> {
        self.parse_validated_module(data).map(|_| ())
    }

    fn parse_validated_module(&self, data: &[u8]) -> Result<Module> {
        let module = self.parser.parse(data)?;
        self.validator.validate(&module)?;
        Ok(module)
    }

    fn convert_to_aot_module(
        &self,
        module: &Module,
        store: Arc<Mutex<crate::runtime::Store>>,
    ) -> Result<LoadedModule> {
        let mut aot_module = LoadedModule::from_module_with_store(module, store.clone())?;
        if module.imports.is_empty() {
            let instance = Instance::new_with_store(Arc::new(module.clone()), store)?;
            aot_module.memories = instance.memories.to_vec();
            aot_module.tables = instance.tables.to_vec();
            aot_module.globals = instance
                .globals
                .iter()
                .map(|global| {
                    global
                        .lock()
                        .map_err(poisoned_lock)
                        .map(|global| global.clone())
                })
                .collect::<Result<Vec<_>>>()?;
        }

        for export in &module.exports {
            let export_idx = match &export.kind {
                crate::runtime::ExportKind::Func(idx) => Export::Function(*idx),
                crate::runtime::ExportKind::Table(idx) => Export::Table(*idx),
                crate::runtime::ExportKind::Memory(idx) => Export::Memory(*idx),
                crate::runtime::ExportKind::Global(idx) => Export::Global(*idx),
                crate::runtime::ExportKind::Tag(_) => continue,
            };
            aot_module.exports.insert(export.name.clone(), export_idx);
        }

        Ok(aot_module)
    }
}

impl Default for EngineLoader {
    fn default() -> Self {
        Self::new()
    }
}

fn poisoned_lock<T>(_: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>) -> WasmError {
    WasmError::Runtime("instance lock poisoned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_valid_aot() {
        let loader = EngineLoader::new();
        let mut data = vec![0x00, 0x61, 0x73, 0x6D];
        data.extend_from_slice(&[1, 0, 0, 0]);
        assert!(loader.validate(&data).is_ok());
    }

    #[test]
    fn test_load_wasm_module() {
        let loader = EngineLoader::new();
        let wasm_data = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
        let result = loader.load(&wasm_data);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_rejects_truncated_header_with_valid_magic() {
        let loader = EngineLoader::new();
        let truncated = vec![0x00, 0x61, 0x73, 0x6D];
        assert!(loader.validate(&truncated).is_err());
    }
}
