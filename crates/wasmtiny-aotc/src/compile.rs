//! Compilation driver: validation → translation → machine-code generation,
//! followed by finish-linking of intra-module calls into a single code image.

use std::sync::Arc;

use cranelift_codegen::{
    FinalizedRelocTarget,
    binemit::{Addend, Reloc},
    ir::{TrapCode, UserExternalName},
    isa::TargetIsa,
    settings::{self, Configurable},
};
use cranelift_entity::EntityRef;
use cranelift_wasm::{FuncIndex, TableIndex, translate_module};
use target_lexicon::Triple;
use wasmparser::{Parser, Payload, TableInit};

use crate::{
    config::CompilerConfig,
    environment::{ElemSegKind, ElemSegRecord, Translator, wasm_error_to_compile},
    error::{CompileError, CompileResult},
};

/// The raw per-function machine-code compilation result.
type CompiledClif = (
    Vec<u8>,
    Vec<(u32, TrapCode)>,
    Vec<(u32, Reloc, Addend, FinalizedRelocTarget)>,
    Vec<UserExternalName>,
);

/// A single finish-linked machine-code blob plus its trap records.
#[derive(Debug)]
pub struct FunctionCode {
    /// `FuncIndex` in the module's combined (imported + defined) index space.
    pub func_index: u32,
    /// Machine code bytes (relocations resolved against the final layout).
    pub code: Vec<u8>,
    /// Trap records: (offset within `code`, trap code).
    pub traps: Vec<(u32, TrapCode)>,
    /// Byte offset of this function's code within the linked code image.
    pub code_offset: u32,
    /// Byte offset of this function's entry trampoline within the code image.
    pub trampoline_offset: u32,
    /// Number of intra-module relocations that were resolved into `code`.
    pub linked_relocations: usize,
}

/// The fully compiled, finish-linked representation of a module, ready for
/// serialisation into an artifact.
pub struct CompiledModule {
    /// The target triple this module was compiled for.
    pub target: Triple,
    /// Translation state (types, imports/exports, memories/tables/globals,
    /// segments, function signatures, and CLIF bodies).
    pub translator: Translator,
    /// Finish-linked machine code for each function, in function order.
    pub functions: Vec<FunctionCode>,
    /// Code-image offset of each function import's host-call stub, in
    /// function-import order.
    pub func_import_stub_offsets: Vec<u32>,
    /// Trap sites (absolute code-image offsets) outside wasm functions —
    /// entry trampolines and host-call stubs.
    pub extra_traps: Vec<(u32, TrapCode)>,
    /// The linked code image containing the assembled code for all functions.
    pub code_image: Vec<u8>,
}

/// Which endianness the artifact header records for the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactEndianness {
    /// Little-endian.
    Little,
}

/// A raw, not-yet-linked compilation result for one function.
struct RawFunction {
    func_index: u32,
    buffer: Vec<u8>,
    traps: Vec<(u32, TrapCode)>,
    relocs: Vec<(u32, Reloc, Addend, FinalizedRelocTarget)>,
    /// The function's user external-name table, used to resolve call-target
    /// relocations back to defined-function ordinals.
    user_funcs: Vec<UserExternalName>,
}

/// A compiled entry trampoline, deduplicated by signature type index.
struct TrampolineUnit {
    type_idx: u32,
    buffer: Vec<u8>,
    traps: Vec<(u32, TrapCode)>,
}

/// A compiled host-call stub for one function import.
struct StubUnit {
    buffer: Vec<u8>,
    traps: Vec<(u32, TrapCode)>,
}

/// Creates the target ISA described by `config`.
///
/// The shared flags select deterministic, spectre-guarded, canonical-NaN
/// lowering without PIC (artifacts are finish-linked and executed from a
/// fixed mapping).
///
/// Only little-endian, 64-bit-pointer targets are accepted: the artifact
/// format declares endianness and pointer width, and the runtime's glue
/// (descriptors, trampolines, value slots) is written for exactly that
/// shape — accepting a big-endian or 32-bit triple here would emit an
/// artifact whose header lies about what its bytes actually are.
pub fn build_isa(config: &CompilerConfig) -> CompileResult<Arc<dyn TargetIsa>> {
    use target_lexicon::Endianness;

    if config.target.endianness() != Ok(Endianness::Little) {
        return Err(CompileError::Isa(format!(
            "big-endian targets are unsupported ({}); the artifact format is little-endian",
            config.target
        )));
    }
    match config.target.pointer_width() {
        Ok(target_lexicon::PointerWidth::U64) => {}
        _ => {
            return Err(CompileError::Isa(format!(
                "targets with non-64-bit pointers are unsupported ({}); the artifact format \
                 assumes 8-byte pointers",
                config.target
            )));
        }
    }

    let mut builder = cranelift_codegen::isa::lookup(config.target.clone())
        .map_err(|err| CompileError::Isa(err.to_string()))?;

    // On x86_64, float rounding (`ceil`/`floor`/`trunc`/`nearest`) lowers to
    // libcall relocations without SSE4.1, and the finish-linker below can only
    // patch intra-module user functions — a `LibCall(…)` external name is a
    // hard link failure. `roundss`/`roundsd` make those ops native; SSE4.1 is
    // the de facto x86_64-v2 baseline, so it is enabled unconditionally.
    if config.target.architecture == target_lexicon::Architecture::X86_64 {
        builder
            .enable("has_sse41")
            .map_err(|err| CompileError::Isa(err.to_string()))?;
    }

    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(|err| CompileError::Isa(err.to_string()))?;
    flag_builder
        .set("enable_nan_canonicalization", "true")
        .map_err(|err| CompileError::Isa(err.to_string()))?;
    flag_builder
        .set("enable_heap_access_spectre_mitigation", "true")
        .map_err(|err| CompileError::Isa(err.to_string()))?;
    flag_builder
        .set("is_pic", "false")
        .map_err(|err| CompileError::Isa(err.to_string()))?;
    let flags = settings::Flags::new(flag_builder);

    let isa = builder
        .finish(flags)
        .map_err(|err| CompileError::Isa(err.to_string()))?;
    Ok(isa)
}

/// Compiles a `.wasm` binary into a finish-linked [`CompiledModule`].
pub fn compile_module(wasm: &[u8], config: &CompilerConfig) -> CompileResult<CompiledModule> {
    let isa = build_isa(config)?;
    let translator = translate(wasm, config, &*isa)?;

    // Generate machine code for every defined function.
    let mut raw: Vec<RawFunction> = Vec::new();
    for (defined_index, func) in translator.info.function_bodies.iter() {
        let (buffer, traps, relocs, user_funcs) = compile_clif(func, &*isa)?;
        let func_index = (translator.info.imported_func_count() + defined_index.index()) as u32;
        raw.push(RawFunction {
            func_index,
            buffer,
            traps,
            relocs,
            user_funcs,
        });
    }

    // Generate a (deduplicated) entry trampoline per distinct signature type.
    let mut trampolines: Vec<TrampolineUnit> = Vec::new();
    let mut trampoline_index_by_type: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::new();
    for (defined_index, _) in translator.info.function_bodies.iter() {
        let func_index_u32 = (translator.info.imported_func_count() + defined_index.index()) as u32;
        let type_idx = translator.info.functions[FuncIndex::from_u32(func_index_u32)];
        let key = type_idx.as_u32();
        if trampoline_index_by_type.contains_key(&key) {
            continue;
        }
        let wasm_type = &translator.info.wasm_types[type_idx];
        let callee_sig = translator.func_env().vmctx_sig(type_idx);
        let clif = crate::trampoline::build_entry_trampoline(
            translator.info.call_conv,
            &callee_sig,
            wasm_type,
            key,
        );
        let (buffer, traps, relocs, _) = compile_clif(&clif, &*isa)?;
        // Trampolines must not need relocations: the finish-linker only
        // patches function bodies. If a future backend emits one here it
        // would be silently dropped — fail loudly instead.
        assert!(
            relocs.is_empty(),
            "entry trampoline emitted relocations ({:?}); the linker cannot patch it",
            relocs
        );
        trampoline_index_by_type.insert(key, trampolines.len());
        trampolines.push(TrampolineUnit {
            type_idx: key,
            buffer,
            traps,
        });
    }

    // Compute the code layout first so relocations can be resolved against
    // final function offsets, and so trampoline offsets are known.
    let mut offsets = Vec::with_capacity(raw.len());
    let mut cursor = 0u32;
    for function in &raw {
        let padding = (16 - (cursor % 16)) % 16;
        cursor += padding;
        offsets.push(cursor);
        cursor += function.buffer.len() as u32;
    }
    let mut trampoline_offsets = Vec::with_capacity(trampolines.len());
    for trampoline in &trampolines {
        let padding = (16 - (cursor % 16)) % 16;
        cursor += padding;
        trampoline_offsets.push(cursor);
        cursor += trampoline.buffer.len() as u32;
    }
    let trampoline_offset_by_type: std::collections::HashMap<u32, u32> = trampolines
        .iter()
        .zip(trampoline_offsets.iter().copied())
        .map(|(unit, offset)| (unit.type_idx, offset))
        .collect();

    // Host-call stubs for each function import (in function-import order).
    let mut stubs: Vec<StubUnit> = Vec::new();
    for (ordinal, import) in translator.info.imported_funcs.iter().enumerate() {
        let wasm_type = &translator.info.wasm_types[import.type_index];
        let callee_sig = translator.func_env().vmctx_sig(import.type_index);
        let clif = crate::trampoline::build_host_call_stub(
            translator.info.call_conv,
            &callee_sig,
            wasm_type,
            ordinal as u32,
        );
        let (buffer, traps, relocs, _) = compile_clif(&clif, &*isa)?;
        // Same contract as the entry trampolines: the finish-linker only
        // patches function bodies, so a stub needing a relocation is a
        // linker bug, not something to drop on the floor.
        assert!(
            relocs.is_empty(),
            "host-call stub emitted relocations ({:?}); the linker cannot patch it",
            relocs
        );
        stubs.push(StubUnit { buffer, traps });
    }

    // Extend the layout past the stubs.
    let mut stub_offsets = Vec::with_capacity(stubs.len());
    for stub in &stubs {
        let padding = (16 - (cursor % 16)) % 16;
        cursor += padding;
        stub_offsets.push(cursor);
        cursor += stub.buffer.len() as u32;
    }
    // Stub order is function-import order.
    let func_import_stub_offsets = stub_offsets.clone();

    // Extra trap sites (absolute code-image offsets) from trampolines and
    // stubs. Both are laid out after the functions, so the concatenation is
    // already sorted, but sort defensively anyway.
    let mut extra_traps: Vec<(u32, TrapCode)> = Vec::new();
    for (trampoline, base) in trampolines.iter().zip(trampoline_offsets.iter().copied()) {
        for &(offset, code) in &trampoline.traps {
            extra_traps.push((base + offset, code));
        }
    }
    for (stub, base) in stubs.iter().zip(stub_offsets.iter().copied()) {
        for &(offset, code) in &stub.traps {
            extra_traps.push((base + offset, code));
        }
    }
    extra_traps.sort_unstable_by_key(|(offset, _)| *offset);

    // Assemble and link the functions.
    let mut code_image = Vec::new();
    let mut functions = Vec::with_capacity(raw.len());
    for (function, base_offset) in raw.into_iter().zip(offsets.iter().copied()) {
        while code_image.len() < base_offset as usize {
            code_image.push(0xCC); // int3 padding
        }

        let func_index = function.func_index;
        let type_idx = translator.info.functions[FuncIndex::from_u32(func_index)].as_u32();
        let trampoline_offset = trampoline_offset_by_type
            .get(&type_idx)
            .copied()
            .ok_or_else(|| CompileError::Link("missing entry trampoline".to_string()))?;

        let mut buffer = function.buffer;
        let mut linked_relocations = 0usize;
        for (reloc_offset, kind, addend, target) in &function.relocs {
            let target_addr = match target {
                FinalizedRelocTarget::ExternalName(name) => {
                    let user_ref = match name {
                        cranelift_codegen::ir::ExternalName::User(user_ref) => user_ref,
                        other => {
                            return Err(CompileError::Link(format!(
                                "unexpected external name in relocation: {other:?}"
                            )));
                        }
                    };
                    let uname = &function.user_funcs[user_ref.index()];
                    let defined_ordinal = uname.index as usize;
                    u64::from(offsets[defined_ordinal])
                }
                FinalizedRelocTarget::Func(offset) => u64::from(base_offset + offset),
            };
            apply_reloc(
                &mut buffer,
                *reloc_offset,
                base_offset,
                target_addr,
                *kind,
                *addend,
            )?;
            linked_relocations += 1;
        }

        code_image.extend_from_slice(&buffer);
        functions.push(FunctionCode {
            func_index,
            code: buffer,
            traps: function.traps,
            code_offset: base_offset,
            trampoline_offset,
            linked_relocations,
        });
    }

    // Assemble the trampolines (no external relocations).
    for (trampoline, base_offset) in trampolines
        .into_iter()
        .zip(trampoline_offsets.iter().copied())
    {
        while code_image.len() < base_offset as usize {
            code_image.push(0xCC);
        }
        let _ = trampoline.type_idx;
        code_image.extend_from_slice(&trampoline.buffer);
    }

    // Assemble the host-call stubs (in function-import order, matching
    // `func_import_stub_offsets`).
    for (stub, base_offset) in stubs.into_iter().zip(stub_offsets.iter().copied()) {
        while code_image.len() < base_offset as usize {
            code_image.push(0xCC);
        }
        code_image.extend_from_slice(&stub.buffer);
    }

    Ok(CompiledModule {
        target: config.target.clone(),
        translator,
        functions,
        func_import_stub_offsets,
        extra_traps,
        code_image,
    })
}

/// Applies a single relocation to `buffer` at `reloc_offset`.
///
/// `target_addr` is the code-image offset of the target (the loader maps the
/// finished code image at a chosen base, so image-relative offsets are
/// sufficient for PC-relative and absolute relocations alike within the
/// image).
fn apply_reloc(
    buffer: &mut [u8],
    reloc_offset: u32,
    func_base: u32,
    target_addr: u64,
    kind: Reloc,
    addend: Addend,
) -> CompileResult<()> {
    let here = u64::from(func_base) + u64::from(reloc_offset);
    let value = target_addr.wrapping_add(addend as u64);

    match kind {
        // Absolute relocations cannot be satisfied at compile time because the
        // loader maps the code image at an arbitrary base address (the image is
        // position-independent: all intra-image references are PC-relative).
        // Reject them so an artifact is never silently mislinked.
        Reloc::Abs4 | Reloc::Abs8 => {
            return Err(CompileError::Link(
                "absolute relocation requires a fixed load address (unsupported for AOT images)"
                    .to_string(),
            ));
        }
        Reloc::X86PCRel4 | Reloc::X86CallPCRel4 | Reloc::X86CallPLTRel4 | Reloc::X86GOTPCRel4 => {
            // S + A - P, where P is the address of the relocation field start
            // (`here`). Cranelift already encodes the "measure from the end of
            // the instruction" adjustment as a -4 addend, so the field start,
            // not `here + 4`, is the subtraction base.
            let disp = value.wrapping_sub(here) as i64;
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&disp) {
                return Err(CompileError::Link(
                    "x86 PC-relative relocation out of range".to_string(),
                ));
            }
            patch(buffer, reloc_offset, &(disp as i32).to_le_bytes());
        }
        Reloc::Arm64Call => {
            // Encode the immediate of a `bl` instruction: imm26 = (target - PC) / 4.
            let disp = value.wrapping_sub(here) as i64;
            if disp % 4 != 0 || !(-(1 << 27)..(1 << 27)).contains(&disp) {
                return Err(CompileError::Link(
                    "arm64 branch relocation out of range".to_string(),
                ));
            }
            let imm26 = ((disp >> 2) as u32) & 0x03FF_FFFF;
            let insn = read_u32(buffer, reloc_offset);
            let patched = (insn & 0xFC00_0000) | imm26;
            patch(buffer, reloc_offset, &patched.to_le_bytes());
        }
        other => {
            return Err(CompileError::Link(format!(
                "unsupported relocation kind: {other:?}"
            )));
        }
    }

    Ok(())
}

/// Recovers defined-table initialiser expressions from the raw wasm.
///
/// Returns `(table_index, elements)` pairs for tables whose initialiser is
/// `ref.func` (repeated across the table's minimum size). `ref.null`
/// initialisers are elided: a freshly allocated table already starts null.
fn collect_table_inits(
    wasm: &[u8],
    imported_tables: u32,
) -> CompileResult<Vec<(u32, Vec<FuncIndex>)>> {
    let mut inits = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        let Payload::TableSection(tables) =
            payload.map_err(|err| CompileError::Validation(err.to_string()))?
        else {
            continue;
        };
        for (offset, entry) in tables.into_iter().enumerate() {
            let table = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
            let TableInit::Expr(expr) = &table.init else {
                continue;
            };
            let mut reader = expr.get_binary_reader();
            let opcode = reader
                .read_u8()
                .map_err(|err| CompileError::Validation(err.to_string()))?;
            if opcode == 0xD0 {
                // `ref.null`: keep the table's natural null initialisation.
                continue;
            }
            if opcode != 0xD2 {
                return Err(CompileError::Unsupported(format!(
                    "unsupported table initialiser expression (opcode {opcode:#04x})"
                )));
            }
            let func = reader
                .read_var_u32()
                .map_err(|err| CompileError::Validation(err.to_string()))?;
            let min = table.ty.initial as u32;
            inits.push((
                imported_tables + offset as u32,
                vec![FuncIndex::from_u32(func); min as usize],
            ));
        }
    }
    Ok(inits)
}

/// Compiles a CLIF function to machine code, returning its buffer, traps, and
/// external relocations.
fn compile_clif(
    func: &cranelift_codegen::ir::Function,
    isa: &dyn TargetIsa,
) -> CompileResult<CompiledClif> {
    let user_funcs = func
        .params
        .user_named_funcs()
        .iter()
        .map(|(_, uname)| uname.clone())
        .collect::<Vec<_>>();
    let context = cranelift_codegen::Context::for_function(func.clone());
    let mut context = context;
    let mut control_plane = cranelift_codegen::control::ControlPlane::default();
    let code = context
        .compile(isa, &mut control_plane)
        .map_err(|err| CompileError::Codegen(format!("{err:?}")))?;

    let buffer = code.code_buffer().to_vec();
    let traps = code
        .buffer
        .traps()
        .iter()
        .map(|trap| (trap.offset, trap.code))
        .collect();
    let relocs = code
        .buffer
        .relocs()
        .iter()
        .map(|reloc| (reloc.offset, reloc.kind, reloc.addend, reloc.target.clone()))
        .collect();
    Ok((buffer, traps, relocs, user_funcs))
}

/// Rejects modules using proposals outside the v1 feature set with an explicit
/// `Unsupported` error.
///
/// The authoritative gate is the curated feature set in
/// [`wasm_features`](crate::environment::wasm_features); this pre-validation
/// runs it first so the failure surfaces as a typed unsupported-feature error
/// rather than an opaque parse error. Structural checks that do not depend on
/// wasmparser's message wording (multi-memory) run first; feature-gated
/// rejections are then distinguished from structural errors by wasmparser's
/// stable messages.
fn gate_unsupported_features(wasm: &[u8]) -> CompileResult<()> {
    // Multi-memory: counted structurally rather than matched on wasmparser's
    // message, which does not contain a stable feature-gate phrasing.
    let mut memories = 0u32;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|err| CompileError::Validation(err.to_string()))? {
            Payload::MemorySection(section) => {
                memories = memories.saturating_add(section.count());
            }
            Payload::ImportSection(section) => {
                for import in section {
                    let import = import.map_err(|err| CompileError::Validation(err.to_string()))?;
                    if matches!(import.ty, wasmparser::TypeRef::Memory(_)) {
                        memories = memories.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }
    if memories > 1 {
        return Err(CompileError::Unsupported(
            "multiple memories are outside the supported feature set".to_string(),
        ));
    }

    let mut validator =
        wasmparser::Validator::new_with_features(crate::environment::wasm_features());
    if let Err(err) = validator.validate_all(wasm) {
        let message = err.to_string();
        let lower = message.to_ascii_lowercase();
        if lower.contains("not enabled")
            || lower.contains("must be enabled")
            || lower.contains("without the gc feature")
            || lower.contains("gc feature")
            || lower.contains("requires the")
            || lower.contains("multiple memories")
            || lower.contains("function references")
        {
            return Err(CompileError::Unsupported(message));
        }
        return Err(CompileError::Validation(message));
    }
    Ok(())
}

fn patch(buffer: &mut [u8], offset: u32, bytes: &[u8]) {
    let start = offset as usize;
    buffer[start..start + bytes.len()].copy_from_slice(bytes);
}

fn read_u32(buffer: &[u8], offset: u32) -> u32 {
    let start = offset as usize;
    u32::from_le_bytes([
        buffer[start],
        buffer[start + 1],
        buffer[start + 2],
        buffer[start + 3],
    ])
}

/// Runs validation and translation, producing a [`Translator`] whose
/// `function_bodies` hold CLIF for every defined function.
fn translate(
    wasm: &[u8],
    config: &CompilerConfig,
    isa: &dyn TargetIsa,
) -> CompileResult<Translator> {
    gate_unsupported_features(wasm)?;

    let mut translator = Translator::new(isa.frontend_config(), isa.default_call_conv());
    translate_module(wasm, &mut translator).map_err(wasm_error_to_compile)?;

    // Table initialiser expressions (`(table funcref (elem $f))`) are dropped
    // by `cranelift-wasm`'s table-section parser; recover them here as
    // synthetic active element segments appended after the module's own
    // segments so they replay at instantiation without disturbing the
    // `table.init`/`elem.drop` index space.
    let inits = collect_table_inits(wasm, translator.info.imported_table_count() as u32)?;
    for (table_index, elements) in inits {
        translator.info.elem_segments.push(ElemSegRecord {
            kind: ElemSegKind::Active {
                table_index: TableIndex::from_u32(table_index),
                base: None,
                offset: 0,
            },
            elements,
        });
    }

    let _ = config;
    Ok(translator)
}
