use std::io::Cursor;

use super::BinaryReader;

use crate::runtime::{
    DataKind, DataSegment, ElemKind, ElemSegment, ExportKind, ExportType, Func, FunctionType,
    GlobalType, Import, ImportKind, Limits, Local, MemoryType, Module, NumType, RefType, Result,
    TableType, ValType, WasmError,
};

const CURRENT_VERSION: u32 = 1;
const FUNC_TYPE_FORM: u8 = 0x60;
const MAGIC: u32 = 0x6D736100;

/// WebAssembly module parser.
///
/// Parses a binary WebAssembly module (`.wasm` file) into a [`Module`].
///
/// # Example
///
/// ```
/// use wasmtiny::loader::Parser;
/// use wasmtiny::runtime::Module;
///
/// let wasm_bytes = [0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
/// let parser = Parser::new();
/// let module: Module = parser.parse(&wasm_bytes).unwrap();
/// assert_eq!(module.funcs.len(), 0);
/// ```
#[derive(Debug)]
/// Parser.
pub struct Parser;

impl Parser {
    /// Creates a new `Parser`.
    pub fn new() -> Self {
        Self
    }

    /// Parses a binary WebAssembly module.
    pub fn parse(&self, data: &[u8]) -> Result<Module> {
        let mut reader = BinaryReader::from_slice(data);
        self.parse_module(&mut reader)
    }

    fn parse_module(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Module> {
        let magic = reader.read_u32()?;
        if magic != MAGIC {
            return Err(WasmError::Load(format!(
                "invalid magic number: {:x}",
                magic
            )));
        }

        let version = reader.read_u32()?;
        if version != CURRENT_VERSION {
            return Err(WasmError::Load(format!("unsupported version: {}", version)));
        }

        let mut module = Module::new();
        let mut last_non_custom_order = 0u8;
        let mut seen_sections = [false; 14];

        while reader.remaining() > 0 {
            let section_id = reader.read_u8()?;
            let section_size = reader.read_uleb128()? as usize;
            let section_bytes = reader.read_bytes(section_size)?;
            let mut section_reader = BinaryReader::from_slice(&section_bytes);

            if section_id != 0 {
                if section_id as usize >= seen_sections.len() {
                    return Err(WasmError::Load(format!(
                        "unknown section id: {}",
                        section_id
                    )));
                }
                if seen_sections[section_id as usize] {
                    return Err(WasmError::Load(format!(
                        "duplicate section id: {}",
                        section_id
                    )));
                }
                let section_order = Self::section_order(section_id).ok_or_else(|| {
                    WasmError::Load(format!("unknown section id: {}", section_id))
                })?;
                if section_order < last_non_custom_order {
                    return Err(WasmError::Load(format!(
                        "section {} out of order",
                        section_id
                    )));
                }
                seen_sections[section_id as usize] = true;
                last_non_custom_order = section_order;
            }

            match section_id {
                0 => self.parse_custom(&mut section_reader)?,
                1 => self.parse_type(&mut section_reader, &mut module)?,
                2 => self.parse_import(&mut section_reader, &mut module)?,
                3 => self.parse_function(&mut section_reader, &mut module)?,
                4 => self.parse_table(&mut section_reader, &mut module)?,
                5 => self.parse_memory(&mut section_reader, &mut module)?,
                6 => self.parse_global(&mut section_reader, &mut module)?,
                7 => self.parse_export(&mut section_reader, &mut module)?,
                8 => self.parse_start(&mut section_reader, &mut module)?,
                9 => self.parse_elem(&mut section_reader, &mut module)?,
                10 => self.parse_code(&mut section_reader, &mut module)?,
                11 => self.parse_data(&mut section_reader, &mut module)?,
                12 => self.parse_data_count(&mut section_reader, &mut module)?,
                13 => self.parse_tag(&mut section_reader, &mut module)?,
                _ => {
                    return Err(WasmError::Load(format!(
                        "unknown section id: {}",
                        section_id
                    )));
                }
            }

            if section_reader.remaining() != 0 {
                return Err(WasmError::Load(format!(
                    "section {} has {} trailing bytes",
                    section_id,
                    section_reader.remaining()
                )));
            }
        }

        Ok(module)
    }

    fn parse_custom(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<()> {
        if reader.remaining() == 0 {
            return Ok(());
        }

        let name_len = reader.read_uleb128()? as usize;
        let _name = reader.read_bytes(name_len)?;
        let _payload = reader.read_bytes(reader.remaining())?;
        Ok(())
    }

    fn parse_data_count(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        module.data_count = Some(reader.read_uleb128()?);
        Ok(())
    }

    fn parse_tag(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        _module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let _attribute = reader.read_u8()?;
            let _type_idx = reader.read_uleb128()?;
        }
        Ok(())
    }

    fn section_order(section_id: u8) -> Option<u8> {
        Some(match section_id {
            1 => 1,   // Type
            2 => 2,   // Import
            3 => 3,   // Function
            13 => 4,  // Tag (wast crate places between Function and Table)
            4 => 5,   // Table
            5 => 6,   // Memory
            6 => 7,   // Global
            7 => 8,   // Export
            8 => 9,   // Start
            9 => 10,  // Elem
            12 => 11, // DataCount (before Code)
            10 => 12, // Code
            11 => 13, // Data
            _ => return None,
        })
    }

    fn parse_type(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let form = reader.read_u8()?;
            if form != FUNC_TYPE_FORM {
                return Err(WasmError::Load("expected func type form".to_string()));
            }

            let param_count = reader.read_uleb128()?;
            let mut params = Vec::new();
            for _ in 0..param_count {
                params.push(self.read_val_type(reader)?);
            }

            let result_count = reader.read_uleb128()?;
            let mut results = Vec::new();
            for _ in 0..result_count {
                results.push(self.read_val_type(reader)?);
            }

            module.types.push(FunctionType::new(params, results));
        }

        Ok(())
    }

    fn read_val_type(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<ValType> {
        let type_byte = reader.read_u8()?;
        match type_byte {
            0x7F => Ok(ValType::Num(NumType::I32)),
            0x7E => Ok(ValType::Num(NumType::I64)),
            0x7D => Ok(ValType::Num(NumType::F32)),
            0x7C => Ok(ValType::Num(NumType::F64)),
            0x70 => Ok(ValType::Ref(RefType::FuncRef)),
            0x6F => Ok(ValType::Ref(RefType::ExternRef)),
            0x63 | 0x64 => Ok(ValType::Ref(self.read_heap_type_ref(reader)?)),
            _ => Err(WasmError::Load(format!("unknown val type: {}", type_byte))),
        }
    }

    fn parse_import(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let module_len = reader.read_uleb128()? as usize;
            let module_name = reader.read_bytes(module_len)?;
            let module_str = String::from_utf8(module_name)
                .map_err(|_| WasmError::Load("invalid UTF-8 in import module name".to_string()))?;

            let name_len = reader.read_uleb128()? as usize;
            let name = reader.read_bytes(name_len)?;
            let name_str = String::from_utf8(name)
                .map_err(|_| WasmError::Load("invalid UTF-8 in export name".to_string()))?;

            let kind_byte = reader.read_u8()?;
            let kind = match kind_byte {
                0x00 => ImportKind::Func(reader.read_uleb128()?),
                0x01 => {
                    let (elem_type, nullable) = self.read_table_ref_type(reader)?;
                    let limits = self.read_limits(reader)?;
                    ImportKind::Table(TableType::new_with_nullability(elem_type, nullable, limits))
                }
                0x02 => {
                    let (limits, shared) = self.read_limits_with_flags(reader)?;
                    let mut memory_type = MemoryType::new(limits);
                    memory_type.shared = shared;
                    ImportKind::Memory(memory_type)
                }
                0x03 => {
                    let content_type = self.read_val_type(reader)?;
                    let mutable = self.read_mutability(reader)?;
                    ImportKind::Global(GlobalType::new(content_type, mutable))
                }
                0x04 => {
                    let attribute = reader.read_u8()?;
                    let type_idx = reader.read_uleb128()?;
                    ImportKind::Tag(attribute, type_idx)
                }
                _ => {
                    return Err(WasmError::Load(format!(
                        "unknown import kind: {}",
                        kind_byte
                    )));
                }
            };

            module.imports.push(Import {
                module: module_str,
                name: name_str,
                kind,
            });
        }

        Ok(())
    }

    fn parse_function(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let type_idx = reader.read_uleb128()?;
            module.funcs.push(Func {
                type_idx,
                locals: Vec::new(),
                body: Vec::new(),
            });
        }

        Ok(())
    }

    fn parse_table(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let prefix = reader.read_u8()?;
            match prefix {
                0x40 => {
                    let flags = reader.read_u8()?;
                    if flags != 0 {
                        return Err(WasmError::Load(format!(
                            "unsupported table init flags: {}",
                            flags
                        )));
                    }
                    let type_prefix = reader.read_u8()?;
                    let (elem_type, nullable) =
                        self.read_ref_type_with_prefix(reader, type_prefix)?;
                    let limits = self.read_limits(reader)?;
                    let init = self.read_const_expr(reader)?;
                    if type_prefix == 0x64 && init.first().copied() == Some(0xD0) {
                        return Err(WasmError::Load(
                            "non-null table initialiser cannot be ref.null".to_string(),
                        ));
                    }
                    let table_idx = module
                        .imports
                        .iter()
                        .filter(|import| matches!(import.kind, ImportKind::Table(_)))
                        .count() as u32
                        + module.tables.len() as u32;
                    let min = limits.min() as usize;

                    // Cap table minimum to prevent untrusted-count allocations
                    const MAX_TABLE_MIN: usize = 100_000;
                    if min > MAX_TABLE_MIN {
                        return Err(WasmError::Load(format!(
                            "table minimum {} exceeds supported maximum {}",
                            min, MAX_TABLE_MIN
                        )));
                    }

                    module
                        .tables
                        .push(TableType::new_with_nullability(elem_type, nullable, limits));
                    if min > 0 {
                        module.elems.push(ElemSegment {
                            kind: ElemKind::Active {
                                table_idx,
                                offset: Self::i32_zero_expr(),
                            },
                            type_: elem_type,
                            nullable,
                            init: vec![init; min],
                            generated_by_table_init: true,
                        });
                    }
                }
                0x64 => {
                    return Err(WasmError::Load(
                        "non-null table declarations require an explicit initialiser".to_string(),
                    ));
                }
                byte => {
                    let (elem_type, nullable) = self.read_ref_type_with_prefix(reader, byte)?;
                    let limits = self.read_limits(reader)?;
                    module
                        .tables
                        .push(TableType::new_with_nullability(elem_type, nullable, limits));
                }
            }
        }

        Ok(())
    }

    fn parse_memory(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let (limits, shared) = self.read_limits_with_flags(reader)?;
            let mut memory_type = MemoryType::new(limits);
            memory_type.shared = shared;
            module.memories.push(memory_type);
        }

        Ok(())
    }

    fn read_limits_with_flags(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
    ) -> Result<(Limits, bool)> {
        match reader.read_u8()? {
            0x00 => Ok((Limits::Min(reader.read_uleb128()?), false)),
            0x01 => Ok((
                Limits::MinMax(reader.read_uleb128()?, reader.read_uleb128()?),
                false,
            )),
            0x02 => Ok((Limits::Min(reader.read_uleb128()?), true)),
            0x03 => Ok((
                Limits::MinMax(reader.read_uleb128()?, reader.read_uleb128()?),
                true,
            )),
            flags => Err(WasmError::Load(format!(
                "unsupported memory limits flags: {}",
                flags
            ))),
        }
    }

    fn parse_global(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let content_type = self.read_val_type(reader)?;
            let mutable = self.read_mutability(reader)?;
            let init_expr = self.read_const_expr(reader)?;
            module.globals.push(GlobalType::new(content_type, mutable));
            module.global_inits.push(init_expr);
        }

        Ok(())
    }

    fn parse_export(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let name_len = reader.read_uleb128()? as usize;
            let name = reader.read_bytes(name_len)?;
            let name_str = String::from_utf8(name)
                .map_err(|_| WasmError::Load("invalid UTF-8 in export name".to_string()))?;

            let kind_byte = reader.read_u8()?;
            let idx = reader.read_uleb128()?;
            let kind = match kind_byte {
                0x00 => ExportKind::Func(idx),
                0x01 => ExportKind::Table(idx),
                0x02 => ExportKind::Memory(idx),
                0x03 => ExportKind::Global(idx),
                0x04 => ExportKind::Tag(idx),
                _ => {
                    return Err(WasmError::Load(format!(
                        "unknown export kind: {}",
                        kind_byte
                    )));
                }
            };

            module.exports.push(ExportType {
                name: name_str,
                kind,
            });
        }

        Ok(())
    }

    fn parse_start(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        module.start = Some(reader.read_uleb128()?);
        Ok(())
    }

    fn parse_elem(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let flags = reader.read_uleb128()?;
            match flags {
                0 => {
                    let offset = self.read_const_expr(reader)?;
                    let funcs = self.read_func_index_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Active {
                            table_idx: 0,
                            offset,
                        },
                        type_: RefType::FuncRef,
                        nullable: false,
                        init: funcs
                            .into_iter()
                            .map(Self::func_ref_expr)
                            .collect::<Vec<_>>(),
                        generated_by_table_init: false,
                    });
                }
                1 => {
                    self.read_elem_kind(reader)?;
                    let funcs = self.read_func_index_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Passive,
                        type_: RefType::FuncRef,
                        nullable: false,
                        init: funcs
                            .into_iter()
                            .map(Self::func_ref_expr)
                            .collect::<Vec<_>>(),
                        generated_by_table_init: false,
                    });
                }
                2 => {
                    let table_idx = reader.read_uleb128()?;
                    let offset = self.read_const_expr(reader)?;
                    self.read_elem_kind(reader)?;
                    let funcs = self.read_func_index_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Active { table_idx, offset },
                        type_: RefType::FuncRef,
                        nullable: false,
                        init: funcs
                            .into_iter()
                            .map(Self::func_ref_expr)
                            .collect::<Vec<_>>(),
                        generated_by_table_init: false,
                    });
                }
                3 => {
                    self.read_elem_kind(reader)?;
                    let funcs = self.read_func_index_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Declarative,
                        type_: RefType::FuncRef,
                        nullable: false,
                        init: funcs
                            .into_iter()
                            .map(Self::func_ref_expr)
                            .collect::<Vec<_>>(),
                        generated_by_table_init: false,
                    });
                }
                4 => {
                    let offset = self.read_const_expr(reader)?;
                    let init = self.read_const_expr_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Active {
                            table_idx: 0,
                            offset,
                        },
                        type_: RefType::FuncRef,
                        nullable: true,
                        init,
                        generated_by_table_init: false,
                    });
                }
                5 => {
                    let (type_, nullable) = self.read_table_ref_type(reader)?;
                    let init = self.read_const_expr_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Passive,
                        type_,
                        nullable,
                        init,
                        generated_by_table_init: false,
                    });
                }
                6 => {
                    let table_idx = reader.read_uleb128()?;
                    let offset = self.read_const_expr(reader)?;
                    let (type_, nullable) = self.read_table_ref_type(reader)?;
                    let init = self.read_const_expr_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Active { table_idx, offset },
                        type_,
                        nullable,
                        init,
                        generated_by_table_init: false,
                    });
                }
                7 => {
                    let (type_, nullable) = self.read_table_ref_type(reader)?;
                    let init = self.read_const_expr_vector(reader)?;
                    module.elems.push(ElemSegment {
                        kind: ElemKind::Declarative,
                        type_,
                        nullable,
                        init,
                        generated_by_table_init: false,
                    });
                }
                _ => {
                    return Err(WasmError::Load(format!(
                        "unsupported element segment flags: {}",
                        flags
                    )));
                }
            }
        }

        Ok(())
    }

    fn parse_code(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        if count as usize != module.funcs.len() {
            return Err(WasmError::Load(
                "function and code section size mismatch".to_string(),
            ));
        }

        for func in &mut module.funcs {
            let body_size = reader.read_uleb128()? as usize;
            let body_bytes = reader.read_bytes(body_size)?;
            let mut body_reader = BinaryReader::from_slice(&body_bytes);
            func.locals = self.parse_locals(&mut body_reader)?;
            func.body = body_reader.read_bytes(body_reader.remaining())?;
            if func.body.last().copied() != Some(0x0B) {
                return Err(WasmError::Load(
                    "function body must end with end opcode".to_string(),
                ));
            }
        }

        Ok(())
    }

    fn parse_locals(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Vec<Local>> {
        let count = reader.read_uleb128()?;
        let mut locals = Vec::new();
        for _ in 0..count {
            let local_count = reader.read_uleb128()?;
            let type_ = self.read_val_type(reader)?;
            locals.push(Local {
                count: local_count,
                type_,
            });
        }
        Ok(locals)
    }

    fn parse_data(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        module: &mut Module,
    ) -> Result<()> {
        let count = reader.read_uleb128()?;
        for _ in 0..count {
            let flags = reader.read_uleb128()?;
            let kind = match flags {
                0 => {
                    let offset = self.read_const_expr(reader)?;
                    DataKind::Active {
                        memory_idx: 0,
                        offset,
                    }
                }
                1 => DataKind::Passive,
                2 => {
                    let memory_idx = reader.read_uleb128()?;
                    let offset = self.read_const_expr(reader)?;
                    DataKind::Active { memory_idx, offset }
                }
                _ => {
                    return Err(WasmError::Load(format!(
                        "unsupported data segment flags: {}",
                        flags
                    )));
                }
            };

            let init_size = reader.read_uleb128()? as usize;
            let init = reader.read_bytes(init_size)?;
            module.data.push(DataSegment { kind, init });
        }

        Ok(())
    }

    fn read_table_ref_type(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
    ) -> Result<(RefType, bool)> {
        let prefix = reader.read_u8()?;
        self.read_ref_type_with_prefix(reader, prefix)
    }

    fn read_ref_type_with_prefix(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
        prefix: u8,
    ) -> Result<(RefType, bool)> {
        match prefix {
            0x70 => Ok((RefType::FuncRef, true)),
            0x6F => Ok((RefType::ExternRef, true)),
            0x63 => self
                .read_heap_type_ref(reader)
                .map(|ref_type| (ref_type, true)),
            0x64 => self
                .read_heap_type_ref(reader)
                .map(|ref_type| (ref_type, false)),
            byte => Err(WasmError::Load(format!("unknown ref type: {}", byte))),
        }
    }

    fn read_heap_type_ref(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<RefType> {
        match reader.read_sleb128_i64()? {
            -0x10 | -0x14 => Ok(RefType::FuncRef),
            -0x11 | -0x13 => Ok(RefType::ExternRef),
            // Positive heap type indices reference concrete types (e.g. (ref null $t)).
            // They are sub-types of funcref; treat them as funcref for the runtime.
            heap_type if heap_type >= 0 => Ok(RefType::FuncRef),
            heap_type => Err(WasmError::Load(format!(
                "GC heap types are not supported: {}",
                heap_type
            ))),
        }
    }

    fn read_limits(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Limits> {
        match reader.read_u8()? {
            0x00 => Ok(Limits::Min(reader.read_uleb128()?)),
            0x01 => Ok(Limits::MinMax(
                reader.read_uleb128()?,
                reader.read_uleb128()?,
            )),
            flags => Err(WasmError::Load(format!(
                "unsupported limits flags: {}",
                flags
            ))),
        }
    }

    fn read_mutability(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<bool> {
        match reader.read_u8()? {
            0x00 => Ok(false),
            0x01 => Ok(true),
            byte => Err(WasmError::Load(format!(
                "invalid mutability flag: {}",
                byte
            ))),
        }
    }

    fn read_const_expr(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Vec<u8>> {
        let mut expr = Vec::new();

        loop {
            let opcode = reader.read_u8()?;
            expr.push(opcode);

            match opcode {
                0x0B => break,
                0x23 | 0xD2 => expr.extend(self.read_raw_uleb(reader)?),
                0x41 => expr.extend(self.read_raw_sleb(reader)?),
                0x42 => expr.extend(self.read_raw_sleb(reader)?),
                0x43 => expr.extend(reader.read_bytes(4)?),
                0x44 => expr.extend(reader.read_bytes(8)?),
                0xD0 => expr.extend(self.read_raw_sleb(reader)?),
                0x6A..=0x6C | 0x7C..=0x7E => {}
                _ => {
                    return Err(WasmError::Load(format!(
                        "unsupported constant expression opcode: {:02x}",
                        opcode
                    )));
                }
            }
        }

        Ok(expr)
    }

    fn read_elem_kind(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<()> {
        match reader.read_u8()? {
            0x00 => Ok(()),
            byte => Err(WasmError::Load(format!(
                "unsupported element kind: {}",
                byte
            ))),
        }
    }

    fn read_func_index_vector(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Vec<u32>> {
        let count = reader.read_uleb128()?;
        let mut funcs = Vec::new();
        for _ in 0..count {
            funcs.push(reader.read_uleb128()?);
        }
        Ok(funcs)
    }

    fn read_const_expr_vector(
        &self,
        reader: &mut BinaryReader<Cursor<&[u8]>>,
    ) -> Result<Vec<Vec<u8>>> {
        let count = reader.read_uleb128()?;
        let mut exprs = Vec::new();
        for _ in 0..count {
            exprs.push(self.read_const_expr(reader)?);
        }
        Ok(exprs)
    }

    fn func_ref_expr(func_idx: u32) -> Vec<u8> {
        let mut expr = Vec::with_capacity(8);
        expr.push(0xD2);
        Self::write_uleb128(func_idx, &mut expr);
        expr.push(0x0B);
        expr
    }

    fn i32_zero_expr() -> Vec<u8> {
        vec![0x41, 0x00, 0x0B]
    }

    fn write_uleb128(mut value: u32, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn read_raw_uleb(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut shift = 0;

        loop {
            let byte = reader.read_u8()?;
            bytes.push(byte);

            if byte & 0x80 == 0 {
                break;
            }

            shift += 7;
            if shift >= 35 {
                return Err(WasmError::Load("uleb128 overflow".to_string()));
            }
        }

        Ok(bytes)
    }

    fn read_raw_sleb(&self, reader: &mut BinaryReader<Cursor<&[u8]>>) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut shift = 0;

        loop {
            let byte = reader.read_u8()?;
            bytes.push(byte);

            if byte & 0x80 == 0 {
                break;
            }

            shift += 7;
            if shift >= 70 {
                return Err(WasmError::Load("sleb128 overflow".to_string()));
            }
        }

        Ok(bytes)
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_module() {
        let data = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
        let parser = Parser::new();
        let module = parser.parse(&data).unwrap();
        assert_eq!(module.types.len(), 0);
        assert_eq!(module.funcs.len(), 0);
    }

    #[test]
    fn test_parse_expression_based_element_segment() {
        let data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x02, 0x01, 0x00, 0x04, 0x04, 0x01, 0x70, 0x00, 0x01, 0x09, 0x09, 0x01, 0x04,
            0x41, 0x00, 0x0B, 0x01, 0xD2, 0x00, 0x0B,
        ];
        let parser = Parser::new();
        let module = parser.parse(&data).unwrap();

        assert_eq!(module.elems.len(), 1);
        assert_eq!(module.elems[0].type_, RefType::FuncRef);
        assert_eq!(module.elems[0].init, vec![vec![0xD2, 0x00, 0x0B]]);
    }

    #[test]
    fn test_parse_rejects_function_body_without_end() {
        let data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x02, 0x01, 0x00, 0x0A, 0x05, 0x01, 0x03, 0x00, 0x41, 0x00,
        ];
        let parser = Parser::new();

        assert!(parser.parse(&data).is_err());
    }

    #[test]
    fn test_parse_rejects_duplicate_sections() {
        let data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
        ];
        let parser = Parser::new();

        assert!(parser.parse(&data).is_err());
    }

    #[test]
    fn test_parse_rejects_out_of_order_sections() {
        let data = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00, 0x01, 0x04,
            0x01, 0x60, 0x00, 0x00,
        ];
        let parser = Parser::new();

        assert!(parser.parse(&data).is_err());
    }
}
