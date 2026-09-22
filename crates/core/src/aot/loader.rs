//! Strict artifact loader: header / section validation and reconstruction of a
//! module representation from an `.aot` artifact — with no wasm parsing.
//!
//! # Version-skew policy
//!
//! The loader accepts exactly one format version, ABI version, endianness,
//! pointer size and target ISA (head [`format`] constants). A mismatch of any
//! of these is a hard refusal (`WasmError::Load`), never a best-effort load:
//! artifacts are finish-linked machine code, so running one under a different
//! ABI or ISA is a memory-safety hazard, not a compatibility question.
//!
//! When the compiler ABI, format, or deployment ISA changes, the owning repo
//! build **regenerates** artifacts (`.wasm` → `.aot`), and old artifacts are
//! rejected on load until then. There is no loader-side migration path by
//! design — this keeps the loader read-only after the integrity check.

use std::env;

use super::{
    format,
    reader::{Reader, load_error},
    verifier::verify_integrity,
};

use crate::runtime::{
    DataKind, DataSegment, ElemKind, ElemSegment, ExportKind, ExportType, FunctionType, GlobalType,
    Import, ImportKind, Limits, MemoryType, NumType, RefType, Result, TableType, TrapCode, ValType,
    WasmError,
};

/// Upper bound on a single memory's declared capacity in bytes (4 GiB).
const MAX_MEMORY_BYTES: u64 = 1 << 32;
/// Upper bound on a declared string field, so crafted artifacts cannot drive
/// unbounded allocation.
const MAX_STRING: usize = 1 << 20;

/// A defined function's code and trap records, extracted from the artifact.
#[derive(Debug, Clone)]
pub struct AotFunction {
    /// Function index in the module's combined (imported + defined) space.
    pub func_index: u32,
    /// Index into the type section.
    pub type_idx: u32,
    /// Byte offset of this function's code within the code image.
    pub code_offset: u32,
    /// Length of this function's code in bytes.
    pub code_len: u32,
    /// Byte offset of this function's entry trampoline within the code image.
    pub trampoline_offset: u32,
    /// Trap records: (offset within this function's code, trap code).
    pub traps: Vec<(u32, TrapCode)>,
}

/// A loaded `.aot` artifact, ready for instantiation.
#[derive(Debug, Clone)]
pub struct AotModule {
    /// Artifact format version.
    pub format_version: u32,
    /// ABI version.
    pub abi_version: u32,
    /// The target triple string recorded in the header.
    pub target: String,
    /// Feature-flag bitmask recorded in the header.
    pub feature_flags: u32,
    /// Function signatures from the type section.
    pub types: Vec<FunctionType>,
    /// Imports, in declaration order.
    pub imports: Vec<Import>,
    /// Exports.
    pub exports: Vec<ExportType>,
    /// Defined memories.
    pub memories: Vec<MemoryType>,
    /// Defined tables.
    pub tables: Vec<TableType>,
    /// Defined globals: (type, encoded initialiser expression).
    pub globals: Vec<(GlobalType, Vec<u8>)>,
    /// Data segments in unified index order.
    pub data: Vec<DataSegment>,
    /// Element segments in unified index order (runtime `Module` form, for the
    /// interpreter path).
    pub elems: Vec<ElemSegment>,
    /// Raw element-segment function indices (`u32::MAX` = null), parallel to
    /// `elems`, for the native replay path.
    pub elem_funcs: Vec<Vec<u32>>,
    /// Defined functions.
    pub functions: Vec<AotFunction>,
    /// Trap sites outside wasm functions: (absolute code-image offset, code).
    pub extra_traps: Vec<(u32, TrapCode)>,
    /// Code-image offset of each function import's host-call stub, in
    /// function-import order.
    pub func_import_stub_offsets: Vec<u32>,
    /// Optional start function.
    pub start: Option<u32>,
    /// The finish-linked machine-code image.
    pub code_image: Vec<u8>,
}

/// Ahead-of-time module loader.
pub struct AotLoader;

struct Header {
    format_version: u32,
    abi_version: u32,
    target: String,
    feature_flags: u32,
}

impl AotModule {
    /// Builds a runtime [`Module`](crate::runtime::Module) from this artifact,
    /// without any wasm parsing. Defined-function bodies are empty: execution
    /// is native through the code image.
    pub fn into_module(&self) -> crate::runtime::Module {
        let mut module = crate::runtime::Module::new();
        module.types = self.types.clone();
        module.imports = self.imports.clone();
        module.exports = self.exports.clone();
        module.tables = self.tables.clone();
        module.memories = self.memories.clone();
        module.globals = self.globals.iter().map(|(ty, _)| ty.clone()).collect();
        module.global_inits = self.globals.iter().map(|(_, init)| init.clone()).collect();
        module.data = self.data.clone();
        module.elems = self.elems.clone();
        module.start = self.start;
        module.data_count = Some(self.data.len() as u32);
        module.funcs = self
            .functions
            .iter()
            .map(|function| crate::runtime::Func {
                type_idx: function.type_idx,
                locals: Vec::new(),
                body: Vec::new(),
            })
            .collect();
        module
    }
}

impl AotLoader {
    /// Creates a new loader.
    pub fn new() -> Self {
        Self
    }

    /// Loads and verifies an `.aot` artifact, returning its module form.
    pub fn load(&self, data: &[u8]) -> Result<AotModule> {
        let mut reader = Reader::new(data);

        let header = parse_header(&mut reader)?;
        let mut integrity: Option<(usize, Vec<u8>)> = None;
        let mut module = AotModule {
            format_version: header.format_version,
            abi_version: header.abi_version,
            target: header.target,
            feature_flags: header.feature_flags,
            types: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            memories: Vec::new(),
            tables: Vec::new(),
            globals: Vec::new(),
            data: Vec::new(),
            elems: Vec::new(),
            elem_funcs: Vec::new(),
            functions: Vec::new(),
            extra_traps: Vec::new(),
            func_import_stub_offsets: Vec::new(),
            start: None,
            code_image: Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();

        while !reader.at_end() {
            let id = reader.read_u32()?;
            let len = reader.read_u32()? as usize;
            if len > reader.remaining() {
                return Err(load_error(format!(
                    "section {id} declares {len} bytes but only {} remain",
                    reader.remaining()
                )));
            }
            let payload = reader.read_bytes(len)?;

            if id == format::SECTION_INTEGRITY {
                if integrity.is_some() {
                    return Err(load_error("multiple integrity sections".to_string()));
                }
                if !reader.at_end() {
                    return Err(load_error(
                        "sections must not follow the integrity section".to_string(),
                    ));
                }
                integrity = Some((reader.position() - len, payload.to_vec()));
                continue;
            }

            if !seen.insert(id) {
                return Err(load_error(format!("duplicate section {id}")));
            }

            parse_section(id, payload, &mut module)?;
        }

        // The integrity section is mandatory: fail closed without it.
        let (preceding_end, integrity_payload) = integrity.ok_or_else(|| {
            WasmError::Load("artifact lacks a mandatory integrity section".to_string())
        })?;
        verify_integrity(&data[..preceding_end], &integrity_payload)?;

        // The code image must account for every defined function.
        validate_code_layout(&module)?;
        // Every index the artifact quotes must land inside the module it
        // describes. Integrity verification detects tampering, but a
        // *crafted* artifact (its author can recompute the digest — SHA512
        // is corruption detection, not authentication) must not be able to
        // reach instantiation with out-of-range indices that later index
        // unchecked panics instead of structured errors.
        validate_indices(&module)?;

        Ok(module)
    }
}

impl Default for AotLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns whether the artifact's target triple names the host ISA.
/// The comparison is prefix-based (ISA component only) — the code executes if
/// the ISA matches, which is what matters for native dispatch.
pub(crate) fn target_matches_host(target: &str) -> bool {
    let isa = target.split('-').next().unwrap_or("");
    isa == env::consts::ARCH
}

fn decode_triple(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Encodes a stored element (function index, or the null sentinel) as the
/// wasm constant-expression bytes the runtime's expression evaluator replays.
fn elem_init_expr(func_index: u32) -> Vec<u8> {
    if func_index == u32::MAX {
        // ref.null func
        vec![0xD0, 0x70, 0x0B]
    } else {
        // ref.func $func_index
        let mut bytes = vec![0xD2];
        push_uleb128(&mut bytes, u64::from(func_index));
        bytes.push(0x0B);
        bytes
    }
}

fn parse_header(reader: &mut Reader<'_>) -> Result<Header> {
    if reader.remaining() < format::HEADER_SIZE {
        return Err(load_error("artifact too short for header".to_string()));
    }

    let magic = reader.read_bytes(4)?;
    if magic != format::MAGIC {
        return Err(load_error(
            "bad artifact magic (expected \"WTA0\")".to_string(),
        ));
    }

    let format_version = reader.read_u32()?;
    if format_version != format::FORMAT_VERSION {
        return Err(load_error(format!(
            "unsupported format version {format_version} (loader supports {})",
            format::FORMAT_VERSION
        )));
    }

    let abi_version = reader.read_u32()?;
    if abi_version != format::ABI_VERSION {
        return Err(load_error(format!(
            "unsupported ABI version {abi_version} (loader supports {}); regenerate the artifact",
            format::ABI_VERSION
        )));
    }

    let endianness = reader.read_u32()?;
    if endianness != format::ENDIANNESS_LITTLE {
        return Err(load_error(
            "artifact endianness does not match this loader".to_string(),
        ));
    }

    let pointer_size = reader.read_u32()?;
    if pointer_size != format::POINTER_SIZE {
        return Err(load_error(format!(
            "unsupported pointer size {pointer_size}"
        )));
    }

    let triple = reader.read_bytes(format::TRIPLE_FIELD_SIZE)?;
    let target = decode_triple(triple);
    if !target_matches_host(&target) {
        return Err(load_error(format!(
            "artifact target ISA ({}) does not match the host ({})",
            target.split('-').next().unwrap_or(target.as_str()),
            env::consts::ARCH
        )));
    }

    let feature_flags = reader.read_u32()?;

    Ok(Header {
        format_version,
        abi_version,
        target,
        feature_flags,
    })
}

/// Parses one section payload into the module.
fn parse_section(id: u32, payload: &[u8], module: &mut AotModule) -> Result<()> {
    let mut reader = Reader::new(payload);
    match id {
        format::SECTION_TYPES => {
            let count = reader.read_count(8)?;
            for _ in 0..count {
                let param_count = reader.read_u32()? as usize;
                let result_count = reader.read_u32()? as usize;
                if param_count > reader.remaining() || result_count > reader.remaining() {
                    return Err(load_error("signature arity exceeds section".to_string()));
                }
                let mut params = Vec::with_capacity(param_count);
                for _ in 0..param_count {
                    params.push(valtype_from_byte(reader.read_u8()?)?);
                }
                let mut results = Vec::with_capacity(result_count);
                for _ in 0..result_count {
                    results.push(valtype_from_byte(reader.read_u8()?)?);
                }
                module.types.push(FunctionType { params, results });
            }
        }
        format::SECTION_IMPORTS => {
            let count = reader.read_count(1)?;
            for _ in 0..count {
                let kind = reader.read_u32()?;
                let import_module = reader.read_string(MAX_STRING)?.to_string();
                let name = reader.read_string(MAX_STRING)?.to_string();
                let kind = match kind {
                    format::IMPORT_FUNC => {
                        let type_idx = reader.read_u32()?;
                        let stub_offset = reader.read_u32()?;
                        module.func_import_stub_offsets.push(stub_offset);
                        ImportKind::Func(type_idx)
                    }
                    format::IMPORT_TABLE => ImportKind::Table(read_table_type(&mut reader)?),
                    format::IMPORT_MEMORY => ImportKind::Memory(read_memory_type(&mut reader)?),
                    format::IMPORT_GLOBAL => {
                        let content_type = valtype_from_byte(reader.read_u8()?)?;
                        let mutable = reader.read_u8()? != 0;
                        ImportKind::Global(GlobalType {
                            content_type,
                            mutable,
                        })
                    }
                    other => {
                        return Err(load_error(format!("unknown import kind {other}")));
                    }
                };
                module.imports.push(Import {
                    module: import_module,
                    name,
                    kind,
                });
            }
        }
        format::SECTION_EXPORTS => {
            let count = reader.read_count(1)?;
            for _ in 0..count {
                let name = reader.read_string(MAX_STRING)?.to_string();
                let kind = reader.read_u32()?;
                let index = reader.read_u32()?;
                let kind = match kind {
                    format::EXPORT_FUNC => ExportKind::Func(index),
                    format::EXPORT_TABLE => ExportKind::Table(index),
                    format::EXPORT_MEMORY => ExportKind::Memory(index),
                    format::EXPORT_GLOBAL => ExportKind::Global(index),
                    other => return Err(load_error(format!("unknown export kind {other}"))),
                };
                module.exports.push(ExportType { name, kind });
            }
        }
        format::SECTION_MEMORIES => {
            let count = reader.read_count(9)?;
            for _ in 0..count {
                module.memories.push(read_memory_type(&mut reader)?);
            }
        }
        format::SECTION_TABLES => {
            let count = reader.read_count(10)?;
            for _ in 0..count {
                module.tables.push(read_table_type(&mut reader)?);
            }
        }
        format::SECTION_GLOBALS => {
            let count = reader.read_count(6)?;
            for _ in 0..count {
                let content_type = valtype_from_byte(reader.read_u8()?)?;
                let mutable = reader.read_u8()? != 0;
                let init_len = reader.read_u32()? as usize;
                if init_len > reader.remaining() {
                    return Err(load_error("global init exceeds section".to_string()));
                }
                let init = reader.read_bytes(init_len)?.to_vec();
                module.globals.push((
                    GlobalType {
                        content_type,
                        mutable,
                    },
                    init,
                ));
            }
        }
        format::SECTION_DATA => {
            let count = reader.read_count(1)?;
            for _ in 0..count {
                let kind = reader.read_u8()?;
                let kind = match kind {
                    0 => {
                        let memory_idx = reader.read_u32()?;
                        let offset_len = reader.read_u32()? as usize;
                        let offset = reader.read_bytes(offset_len)?.to_vec();
                        DataKind::Active { memory_idx, offset }
                    }
                    1 => DataKind::Passive,
                    other => {
                        return Err(load_error(format!("unknown data segment kind {other}")));
                    }
                };
                let data_len = reader.read_u32()? as usize;
                if data_len > reader.remaining() {
                    return Err(load_error("data segment exceeds section".to_string()));
                }
                let init = reader.read_bytes(data_len)?.to_vec();
                module.data.push(DataSegment { kind, init });
            }
        }
        format::SECTION_ELEMS => {
            let count = reader.read_count(1)?;
            for _ in 0..count {
                let kind = reader.read_u8()?;
                let kind = match kind {
                    0 => {
                        let table_idx = reader.read_u32()?;
                        let offset_len = reader.read_u32()? as usize;
                        let offset = reader.read_bytes(offset_len)?.to_vec();
                        ElemKind::Active { table_idx, offset }
                    }
                    1 => ElemKind::Passive,
                    2 => ElemKind::Declarative,
                    other => {
                        return Err(load_error(format!("unknown element segment kind {other}")));
                    }
                };
                let elem_count = reader.read_count(4)?;
                let mut init = Vec::with_capacity(elem_count as usize);
                let mut func_indices = Vec::with_capacity(elem_count as usize);
                for _ in 0..elem_count {
                    let func_index = reader.read_u32()?;
                    func_indices.push(func_index);
                    init.push(elem_init_expr(func_index));
                }
                module.elems.push(ElemSegment {
                    kind,
                    type_: RefType::FuncRef,
                    nullable: true,
                    init,
                    generated_by_table_init: false,
                });
                module.elem_funcs.push(func_indices);
            }
        }
        format::SECTION_CODE => {
            module.code_image = payload.to_vec();
        }
        format::SECTION_EXTRA_TRAPS => {
            let count = reader.read_count(5)?;
            for _ in 0..count {
                let offset = reader.read_u32()?;
                let code = reader.read_u8()?;
                let code = format::trap_code_from_byte(code)
                    .ok_or_else(|| load_error(format!("unknown trap code {code}")))?;
                module.extra_traps.push((offset, code));
            }
        }
        format::SECTION_START => {
            if reader.read_u8()? != 0 {
                module.start = Some(reader.read_u32()?);
            }
        }
        format::SECTION_FUNCTION_MAP => {
            let count = reader.read_count(24)?;
            for _ in 0..count {
                let func_index = reader.read_u32()?;
                let type_idx = reader.read_u32()?;
                let trampoline_offset = reader.read_u32()?;
                let code_offset = reader.read_u32()?;
                let code_len = reader.read_u32()?;
                let trap_count = reader.read_u32()? as usize;
                if trap_count > reader.remaining() / 5 {
                    return Err(load_error("trap count exceeds section".to_string()));
                }
                let mut traps = Vec::with_capacity(trap_count);
                for _ in 0..trap_count {
                    let offset = reader.read_u32()?;
                    let code = reader.read_u8()?;
                    let code = format::trap_code_from_byte(code)
                        .ok_or_else(|| load_error(format!("unknown trap code {code}")))?;
                    traps.push((offset, code));
                }
                module.functions.push(AotFunction {
                    func_index,
                    type_idx,
                    code_offset,
                    code_len,
                    trampoline_offset,
                    traps,
                });
            }
        }
        format::SECTION_INTEGRITY => {
            // Handled in the top-level walk.
        }
        other => {
            return Err(load_error(format!("unknown section id {other}")));
        }
    }
    Ok(())
}

fn push_uleb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value as u8) & 0x7F;
        value >>= 7;
        if value != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            break;
        }
    }
}

fn read_memory_type(reader: &mut Reader<'_>) -> Result<MemoryType> {
    let min = reader.read_u32()?;
    let has_max = reader.read_u32()? != 0;
    let max = if has_max {
        Some(reader.read_u32()?)
    } else {
        None
    };
    let shared = reader.read_u8()? != 0;
    // Reject memories that could not have been produced by the compiler
    // (and which would overflow the reservation bound checks).
    let max_bytes = u64::from(min) * 65536;
    if max_bytes > MAX_MEMORY_BYTES {
        return Err(load_error(format!(
            "memory minimum {min} pages exceeds 4 GiB"
        )));
    }
    let limits = match max {
        Some(max) => Limits::MinMax(min, max),
        None => Limits::Min(min),
    };
    Ok(MemoryType { limits, shared })
}

fn read_table_type(reader: &mut Reader<'_>) -> Result<TableType> {
    let elem_byte = reader.read_u8()?;
    let elem_type = match elem_byte {
        0x70 => RefType::FuncRef,
        0x6F => RefType::ExternRef,
        other => {
            return Err(load_error(format!(
                "unknown table element type {other:#04x}"
            )));
        }
    };
    let nullable = reader.read_u8()? != 0;
    let min = reader.read_u32()?;
    let has_max = reader.read_u32()? != 0;
    let limits = match if has_max {
        Some(reader.read_u32()?)
    } else {
        None
    } {
        Some(max) => Limits::MinMax(min, max),
        None => Limits::Min(min),
    };
    Ok(TableType {
        elem_type,
        nullable,
        limits,
    })
}

/// Number of imported tables in the artifact.
fn table_import_count(module: &AotModule) -> usize {
    module
        .imports
        .iter()
        .filter(|import| matches!(import.kind, ImportKind::Table(_)))
        .count()
}

/// Ensures every declared function's code range lies within the code image.
fn validate_code_layout(module: &AotModule) -> Result<()> {
    let image_len = module.code_image.len() as u64;
    for function in &module.functions {
        let start = u64::from(function.code_offset);
        let end = start
            .checked_add(u64::from(function.code_len))
            .ok_or_else(|| {
                load_error(format!(
                    "function {} code range overflows",
                    function.func_index
                ))
            })?;
        if end > image_len {
            return Err(load_error(format!(
                "function {} code range [{start}, {end}) exceeds code image of {image_len} bytes",
                function.func_index
            )));
        }
        if u64::from(function.trampoline_offset) >= image_len {
            return Err(load_error(format!(
                "function {} trampoline offset {} exceeds code image of {image_len} bytes",
                function.func_index, function.trampoline_offset
            )));
        }
        for (offset, _) in &function.traps {
            if u64::from(*offset) >= u64::from(function.code_len) {
                return Err(load_error(format!(
                    "function {} trap offset {offset} outside its code",
                    function.func_index
                )));
            }
        }
    }
    for stub_offset in &module.func_import_stub_offsets {
        if u64::from(*stub_offset) >= image_len {
            return Err(load_error(format!(
                "function-import stub offset {stub_offset} exceeds code image of {image_len} bytes"
            )));
        }
    }
    Ok(())
}

/// Ensures every index-space reference in the artifact is in range, so
/// instantiation can index without panicking regardless of what the artifact
/// bytes claim. `u32::MAX` in an element segment is the null-funcref
/// sentinel and is exempt.
fn validate_indices(module: &AotModule) -> Result<()> {
    let type_count = module.types.len() as u32;
    let imported_funcs = module
        .imports
        .iter()
        .filter(|import| matches!(import.kind, ImportKind::Func(_)))
        .count() as u32;
    let total_funcs = imported_funcs + module.functions.len() as u32;

    for function in &module.functions {
        if function.type_idx >= type_count {
            return Err(load_error(format!(
                "function {} references type {} but the artifact has {type_count} types",
                function.func_index, function.type_idx
            )));
        }
    }

    for (segment, func_indices) in module.elems.iter().zip(&module.elem_funcs) {
        if let ElemKind::Active { table_idx, .. } = &segment.kind
            && *table_idx as usize >= module.tables.len() + table_import_count(module)
        {
            return Err(load_error(format!(
                "element segment references table {table_idx} outside the module's table index space"
            )));
        }
        for &func_index in func_indices {
            if func_index != u32::MAX && func_index >= total_funcs {
                return Err(load_error(format!(
                    "element segment references function {func_index} outside the module's \
                     function index space ({total_funcs} functions)"
                )));
            }
        }
    }

    if let Some(start) = module.start
        && start >= total_funcs
    {
        return Err(load_error(format!(
            "start function index {start} outside the module's function index space \
             ({total_funcs} functions)"
        )));
    }

    Ok(())
}

fn valtype_from_byte(byte: u8) -> Result<ValType> {
    match byte {
        0x7F => Ok(ValType::Num(NumType::I32)),
        0x7E => Ok(ValType::Num(NumType::I64)),
        0x7D => Ok(ValType::Num(NumType::F32)),
        0x7C => Ok(ValType::Num(NumType::F64)),
        0x70 => Ok(ValType::Ref(RefType::FuncRef)),
        0x6F => Ok(ValType::Ref(RefType::ExternRef)),
        other => Err(load_error(format!("unknown value type {other:#04x}"))),
    }
}
