//! The wasm→CLIF translation pipeline.
//!
//! The standalone `cranelift-wasm` crate is discontinued (last published at
//! 0.112, incompatible with the current `cranelift-codegen`), so this module
//! owns the translation itself:
//!
//! 1. [`translate_module`] walks the validated module payloads and collects
//!    the declarations into a [`Translator`](crate::environment::Translator).
//! 2. [`FuncTranslator`] translates each function body's operator stream into
//!    CLIF, guided by the [`FuncEnv`](crate::environment::FuncEnv) lowering.
//!
//! The control-flow state machine (value stack, control stack, reachability)
//! follows the same design as the historical `cranelift-wasm` translator.
//! Validation is a separate, prior pass (`gate_unsupported_features` in
//! `compile.rs`), so this module only parses already-validated binaries.

use std::collections::HashMap;

use cranelift_codegen::ir::{
    self, AtomicRmwOp, Block, BlockArg, Inst, InstBuilder, JumpTableData, MemFlagsData, Opcode,
    Value, ValueLabel,
    condcodes::{FloatCC, IntCC},
    immediates::{Ieee32, Ieee64, Offset32},
    types,
};
use cranelift_entity::packed_option::ReservedValue;
use cranelift_frontend::{FunctionBuilder, Variable};
use wasmparser::{
    BinaryReader, BlockType, CompositeInnerType, ConstExpr, DataKind, ElementItems, ElementKind,
    ExternalKind, FunctionBody, MemArg, Operator, Parser, Payload, TypeRef, ValType,
};

use crate::{
    environment::{
        DataSegKind, DataSegRecord, ElemSegKind, ElemSegRecord, FuncEnv, GlobalVar, ModuleInfo,
        Translator, USER_TRAP_UNREACHABLE, mem_access_flags, user_trap,
    },
    error::{CompileError, CompileResult},
    types::{FuncIndex, GlobalIndex, MemoryIndex, TableIndex, TypeIndex},
};

/// A wasm translation result.
pub type TranslateResult<T> = Result<T, TranslateError>;

/// Errors produced while translating wasm to CLIF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// The module uses an operator the compiler does not lower.
    Unsupported(String),
    /// Any other translation failure.
    Other(String),
}

/// Information about the presence of an associated `else` for an `if`, or the
/// lack thereof.
#[derive(Debug)]
pub enum ElseData {
    /// The `if` does not already have an `else` block.
    ///
    /// This doesn't mean that it will never have an `else`, just that we
    /// haven't seen it yet.
    NoElse {
        /// If we discover that we need an `else` block, this is the jump
        /// instruction that needs to be fixed up to point to the new `else`
        /// block rather than the destination block after the `if...end`.
        branch_inst: Inst,
        /// The placeholder block we're replacing.
        placeholder: Block,
    },
    /// We have already allocated an `else` block.
    WithElse {
        /// This is the `else` block.
        else_block: Block,
    },
}

/// A control stack frame: an `if`, a `block` or a `loop`.
#[derive(Debug)]
pub enum ControlStackFrame {
    /// An `if` frame.
    If {
        /// Block that will hold the code after the control block.
        destination: Block,
        /// Presence/absence of an `else` block.
        else_data: ElseData,
        /// Number of block parameter values.
        num_param_values: usize,
        /// Number of block return values.
        num_return_values: usize,
        /// Size of the value stack at the beginning of the control block.
        original_stack_size: usize,
        /// Whether a branch to the exit block was already emitted.
        exit_is_branched_to: bool,
        /// The block type.
        blocktype: BlockType,
        /// Was the head of the `if` reachable?
        head_is_reachable: bool,
        /// Reachability at the end of the consequent (`None` until `else`/`end`).
        consequent_ends_reachable: Option<bool>,
    },
    /// A plain `block` frame.
    Block {
        /// Block that will hold the code after the control block.
        destination: Block,
        /// Number of block parameter values.
        num_param_values: usize,
        /// Number of block return values.
        num_return_values: usize,
        /// Size of the value stack at the beginning of the control block.
        original_stack_size: usize,
        /// Whether a branch to the exit block was already emitted.
        exit_is_branched_to: bool,
    },
    /// A `loop` frame.
    Loop {
        /// Block that will hold the code after the loop.
        destination: Block,
        /// The loop header (branch target).
        header: Block,
        /// Number of block parameter values.
        num_param_values: usize,
        /// Number of block return values.
        num_return_values: usize,
        /// Size of the value stack at the beginning of the control block.
        original_stack_size: usize,
    },
}

/// Contains information passed along during a function's translation: the
/// current value and control stacks, and the reachability of the current
/// position.
#[derive(Debug)]
pub struct FuncTranslationState {
    /// A stack of values corresponding to the active values in the input wasm
    /// function at this point.
    stack: Vec<Value>,
    /// A stack of active control flow operations.
    control_stack: Vec<ControlStackFrame>,
    /// Is the current translation state still reachable?
    reachable: bool,
}

/// WebAssembly to Cranelift IR function translator.
///
/// A single translator instance can be reused to translate multiple functions,
/// which reduces heap allocation traffic.
pub struct FuncTranslator {
    func_ctx: cranelift_frontend::FunctionBuilderContext,
    state: FuncTranslationState,
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            TranslateError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for TranslateError {}

impl From<crate::error::CompileError> for TranslateError {
    fn from(err: crate::error::CompileError) -> Self {
        TranslateError::Other(err.to_string())
    }
}

impl From<wasmparser::BinaryReaderError> for TranslateError {
    fn from(err: wasmparser::BinaryReaderError) -> Self {
        TranslateError::Other(err.to_string())
    }
}

impl ControlStackFrame {
    fn num_return_values(&self) -> usize {
        match *self {
            Self::If {
                num_return_values, ..
            }
            | Self::Block {
                num_return_values, ..
            }
            | Self::Loop {
                num_return_values, ..
            } => num_return_values,
        }
    }

    fn num_param_values(&self) -> usize {
        match *self {
            Self::If {
                num_param_values, ..
            }
            | Self::Block {
                num_param_values, ..
            }
            | Self::Loop {
                num_param_values, ..
            } => num_param_values,
        }
    }

    fn following_code(&self) -> Block {
        match *self {
            Self::If { destination, .. }
            | Self::Block { destination, .. }
            | Self::Loop { destination, .. } => destination,
        }
    }

    fn br_destination(&self) -> Block {
        match *self {
            Self::If { destination, .. } | Self::Block { destination, .. } => destination,
            Self::Loop { header, .. } => header,
        }
    }

    fn original_stack_size(&self) -> usize {
        match *self {
            Self::If {
                original_stack_size,
                ..
            }
            | Self::Block {
                original_stack_size,
                ..
            }
            | Self::Loop {
                original_stack_size,
                ..
            } => original_stack_size,
        }
    }

    fn is_loop(&self) -> bool {
        matches!(self, Self::Loop { .. })
    }

    fn exit_is_branched_to(&self) -> bool {
        matches!(
            self,
            Self::If {
                exit_is_branched_to: true,
                ..
            } | Self::Block {
                exit_is_branched_to: true,
                ..
            }
        )
    }

    fn set_branched_to_exit(&mut self) {
        match self {
            Self::If {
                exit_is_branched_to,
                ..
            }
            | Self::Block {
                exit_is_branched_to,
                ..
            } => *exit_is_branched_to = true,
            Self::Loop { .. } => {}
        }
    }

    /// Pop values from the value stack so that it is left at the
    /// input-parameters to an else-block.
    fn truncate_value_stack_to_else_params(&self, stack: &mut Vec<Value>) {
        debug_assert!(matches!(self, &ControlStackFrame::If { .. }));
        stack.truncate(self.original_stack_size());
    }

    /// Pop values from the value stack so that it is left at the state it was
    /// before this control-flow frame.
    fn truncate_value_stack_to_original_size(&self, stack: &mut Vec<Value>) {
        // The "If" frame pushes its parameters twice, so they're available to
        // the else block. Yet, the original_stack_size member accounts for
        // them only once, so we need to subtract an extra number of parameter
        // values for if blocks.
        let num_duplicated_params = match self {
            &ControlStackFrame::If {
                num_param_values, ..
            } => {
                debug_assert!(num_param_values <= self.original_stack_size());
                num_param_values
            }
            _ => 0,
        };
        stack.truncate(self.original_stack_size() - num_duplicated_params);
    }
}

impl FuncTranslationState {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            control_stack: Vec::new(),
            reachable: true,
        }
    }

    fn clear(&mut self) {
        debug_assert!(self.stack.is_empty());
        debug_assert!(self.control_stack.is_empty());
        self.reachable = true;
    }

    /// Initialize the state for compiling a function with the given signature.
    ///
    /// This resets the state to containing only a single block representing
    /// the whole function. The exit block is the last block in the function
    /// which will contain the return instruction.
    fn initialize(&mut self, sig: &ir::Signature, exit_block: Block) {
        self.clear();
        self.push_block(
            exit_block,
            0,
            sig.returns
                .iter()
                .filter(|arg| arg.purpose == ir::ArgumentPurpose::Normal)
                .count(),
        );
    }

    fn push1(&mut self, val: Value) {
        self.stack.push(val);
    }

    fn pushn(&mut self, vals: &[Value]) {
        self.stack.extend_from_slice(vals);
    }

    fn pop1(&mut self) -> Value {
        self.stack
            .pop()
            .expect("attempted to pop a value from an empty stack")
    }

    fn pop2(&mut self) -> (Value, Value) {
        let v2 = self.stack.pop().unwrap();
        let v1 = self.stack.pop().unwrap();
        (v1, v2)
    }

    fn pop3(&mut self) -> (Value, Value, Value) {
        let v3 = self.stack.pop().unwrap();
        let v2 = self.stack.pop().unwrap();
        let v1 = self.stack.pop().unwrap();
        (v1, v2, v3)
    }

    fn peek1(&self) -> Value {
        *self
            .stack
            .last()
            .expect("attempted to peek at a value on an empty stack")
    }

    fn ensure_length_is_at_least(&self, n: usize) {
        debug_assert!(
            n <= self.stack.len(),
            "attempted to access {n} values but stack only has {} values",
            self.stack.len()
        )
    }

    fn popn(&mut self, n: usize) {
        self.ensure_length_is_at_least(n);
        let new_len = self.stack.len() - n;
        self.stack.truncate(new_len);
    }

    fn peekn(&self, n: usize) -> &[Value] {
        self.ensure_length_is_at_least(n);
        &self.stack[self.stack.len() - n..]
    }

    fn peekn_mut(&mut self, n: usize) -> &mut [Value] {
        self.ensure_length_is_at_least(n);
        let len = self.stack.len();
        &mut self.stack[len - n..]
    }

    fn push_block(
        &mut self,
        following_code: Block,
        num_param_types: usize,
        num_result_types: usize,
    ) {
        debug_assert!(num_param_types <= self.stack.len());
        self.control_stack.push(ControlStackFrame::Block {
            destination: following_code,
            original_stack_size: self.stack.len() - num_param_types,
            num_param_values: num_param_types,
            num_return_values: num_result_types,
            exit_is_branched_to: false,
        });
    }

    fn push_loop(
        &mut self,
        header: Block,
        following_code: Block,
        num_param_types: usize,
        num_result_types: usize,
    ) {
        debug_assert!(num_param_types <= self.stack.len());
        self.control_stack.push(ControlStackFrame::Loop {
            header,
            destination: following_code,
            original_stack_size: self.stack.len() - num_param_types,
            num_param_values: num_param_types,
            num_return_values: num_result_types,
        });
    }

    fn push_if(
        &mut self,
        destination: Block,
        else_data: ElseData,
        num_param_types: usize,
        num_result_types: usize,
        blocktype: BlockType,
    ) {
        debug_assert!(num_param_types <= self.stack.len());

        // Push a second copy of our `if`'s parameters on the stack. This lets
        // us avoid saving them on the side in the `ControlStackFrame` for our
        // `else` block (if it exists), which would require a second heap
        // allocation.
        self.stack.reserve(num_param_types);
        for i in (self.stack.len() - num_param_types)..self.stack.len() {
            let val = self.stack[i];
            self.stack.push(val);
        }

        self.control_stack.push(ControlStackFrame::If {
            destination,
            else_data,
            original_stack_size: self.stack.len() - num_param_types,
            num_param_values: num_param_types,
            num_return_values: num_result_types,
            exit_is_branched_to: false,
            head_is_reachable: self.reachable,
            consequent_ends_reachable: None,
            blocktype,
        });
    }
}

impl FuncTranslator {
    /// Creates a new translator.
    pub fn new() -> Self {
        Self {
            func_ctx: cranelift_frontend::FunctionBuilderContext::new(),
            state: FuncTranslationState::new(),
        }
    }

    /// Translate a binary WebAssembly function from a `FunctionBody`.
    ///
    /// The Cranelift IR function `func` should be completely empty except for
    /// the `func.signature` and `func.name` fields. The signature's
    /// `VMContext` argument and any `Normal` arguments are made accessible as
    /// WebAssembly local variables.
    pub fn translate_body(
        &mut self,
        body: FunctionBody<'_>,
        func: &mut ir::Function,
        environ: &mut FuncEnv<'_>,
    ) -> TranslateResult<()> {
        debug_assert_eq!(func.dfg.num_blocks(), 0, "Function must be empty");
        debug_assert_eq!(func.dfg.num_insts(), 0, "Function must be empty");

        let mut builder = FunctionBuilder::new(func, &mut self.func_ctx);
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block); // Declare all predecessors known.
        builder.ensure_inserted_block();

        let num_params = declare_wasm_parameters(&mut builder, entry_block);

        // Set up the translation state with a single pushed control block
        // representing the whole function and its return values.
        let exit_block = builder.create_block();
        builder.append_block_params_for_function_returns(exit_block);
        self.state.initialize(&builder.func.signature, exit_block);

        // Read the locals declaration, then the operators (from a fresh reader
        // that skips the locals).
        let mut reader = body.get_binary_reader();
        parse_local_decls(&mut reader, &mut builder, num_params)?;

        environ
            .before_translate_function(&mut builder)
            .map_err(|e| TranslateError::Other(e.to_string()))?;

        let ops = body.get_operators_reader()?;
        for op in ops {
            let op = op?;
            translate_operator(&op, &mut builder, &mut self.state, environ)?;
        }

        // The final `End` operator left us in the exit block where we need to
        // manually add a return instruction. After that `End` the value stack
        // holds exactly the function's return values (the exit block's
        // parameters).
        if self.state.reachable && !builder.is_unreachable() {
            let args = self.state.stack.clone();
            builder.ins().return_(&args);
        }
        self.state.stack.clear();

        builder.finalize(environ.frontend_config());
        Ok(())
    }
}

impl Default for FuncTranslator {
    fn default() -> Self {
        Self::new()
    }
}

/// Walks the module payloads, collecting declarations into `translator` and
/// translating every defined function body.
///
/// The input must already have been validated with [`crate::environment::wasm_features`]
/// (the caller runs `gate_unsupported_features` first).
pub fn translate_module(wasm: &[u8], translator: &mut Translator) -> CompileResult<()> {
    let mut defined_func_types: Vec<TypeIndex> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|err| CompileError::Validation(err.to_string()))?;
        match payload {
            Payload::Version { .. }
            | Payload::End(_)
            | Payload::CustomSection(_)
            | Payload::DataCountSection { .. } => {}

            Payload::TypeSection(section) => {
                for entry in section {
                    let group = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    for sub in group.types() {
                        match &sub.composite_type.inner {
                            CompositeInnerType::Func(ty) => {
                                let mut sig = ir::Signature::new(translator.info.call_conv);
                                sig.params.extend(ty.params().iter().map(|ty| {
                                    ir::AbiParam::new(crate::types::valtype_to_clif(*ty))
                                }));
                                sig.returns.extend(ty.results().iter().map(|ty| {
                                    ir::AbiParam::new(crate::types::valtype_to_clif(*ty))
                                }));
                                translator.info.wasm_types.push(ty.clone());
                                translator.info.signatures.push(sig);
                            }
                            _ => {
                                return Err(CompileError::Unsupported(
                                    "GC types (struct/array) are outside the supported feature set"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }

            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.map_err(|err| CompileError::Validation(err.to_string()))?;
                    match import.ty {
                        TypeRef::Func(type_index) => {
                            translator
                                .info
                                .functions
                                .push(TypeIndex::from_u32(type_index));
                            translator
                                .info
                                .imported_funcs
                                .push(crate::environment::FuncImport {
                                    module: import.module.to_string(),
                                    field: import.name.to_string(),
                                    type_index: TypeIndex::from_u32(type_index),
                                });
                        }
                        TypeRef::Table(ty) => {
                            translator.info.tables.push(crate::types::Table::from(ty));
                            translator
                                .info
                                .imported_tables
                                .push(crate::environment::TableImport {
                                    module: import.module.to_string(),
                                    field: import.name.to_string(),
                                    table: crate::types::Table::from(ty),
                                });
                        }
                        TypeRef::Memory(ty) => {
                            translator
                                .info
                                .memories
                                .push(crate::types::Memory::from(ty));
                            translator.info.imported_memories.push(
                                crate::environment::MemoryImport {
                                    module: import.module.to_string(),
                                    field: import.name.to_string(),
                                    memory: crate::types::Memory::from(ty),
                                },
                            );
                        }
                        TypeRef::Global(ty) => {
                            translator
                                .info
                                .globals
                                .push((crate::types::Global::from(ty), None));
                            translator.info.imported_globals.push(
                                crate::environment::GlobalImport {
                                    module: import.module.to_string(),
                                    field: import.name.to_string(),
                                    global: crate::types::Global::from(ty),
                                },
                            );
                        }
                        TypeRef::Tag(_) | TypeRef::FuncExact(_) => {
                            return Err(CompileError::Unsupported(
                                "exception handling / exact function types are outside the \
                                 supported feature set"
                                    .to_string(),
                            ));
                        }
                    }
                }
            }

            Payload::FunctionSection(section) => {
                for entry in section {
                    let type_index =
                        entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    let type_index = TypeIndex::from_u32(type_index);
                    translator.info.functions.push(type_index);
                    defined_func_types.push(type_index);
                }
            }

            Payload::TableSection(section) => {
                for entry in section {
                    let table = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    translator
                        .info
                        .tables
                        .push(crate::types::Table::from(table.ty));
                }
            }

            Payload::MemorySection(section) => {
                for entry in section {
                    let memory = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    translator
                        .info
                        .memories
                        .push(crate::types::Memory::from(memory));
                }
            }

            Payload::GlobalSection(section) => {
                for entry in section {
                    let global = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    let init = crate::types::ConstExpr::parse(&global.init_expr)
                        .map_err(CompileError::Unsupported)?;
                    translator
                        .info
                        .globals
                        .push((crate::types::Global::from(global.ty), Some(init)));
                }
            }

            Payload::ExportSection(section) => {
                for entry in section {
                    let export = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    match export.kind {
                        ExternalKind::Func => translator
                            .info
                            .func_exports
                            .push((FuncIndex::from_u32(export.index), export.name.to_string())),
                        ExternalKind::Table => translator
                            .info
                            .table_exports
                            .push((TableIndex::from_u32(export.index), export.name.to_string())),
                        ExternalKind::Memory => translator
                            .info
                            .memory_exports
                            .push((MemoryIndex::from_u32(export.index), export.name.to_string())),
                        ExternalKind::Global => translator
                            .info
                            .global_exports
                            .push((GlobalIndex::from_u32(export.index), export.name.to_string())),
                        ExternalKind::Tag => {
                            return Err(CompileError::Unsupported(
                                "exception handling is outside the supported feature set"
                                    .to_string(),
                            ));
                        }
                        _ => {
                            return Err(CompileError::Unsupported(format!(
                                "unsupported export kind {:?}",
                                export.kind
                            )));
                        }
                    }
                }
            }

            Payload::StartSection { func, .. } => {
                translator.info.start_func = Some(FuncIndex::from_u32(func));
            }

            Payload::ElementSection(section) => {
                for entry in section {
                    let element = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    let elements = read_elems(&element.items)?;
                    let record = match element.kind {
                        ElementKind::Active {
                            table_index,
                            offset_expr,
                        } => {
                            let (base, offset) = const_offset(&offset_expr)?;
                            ElemSegRecord {
                                kind: ElemSegKind::Active {
                                    table_index: TableIndex::from_u32(table_index.unwrap_or(0)),
                                    base,
                                    offset: offset as u32,
                                },
                                elements,
                            }
                        }
                        ElementKind::Passive => ElemSegRecord {
                            kind: ElemSegKind::Passive,
                            elements,
                        },
                        ElementKind::Declared => ElemSegRecord {
                            kind: ElemSegKind::Declarative,
                            elements,
                        },
                    };
                    translator.info.elem_segments.push(record);
                }
            }

            Payload::DataSection(section) => {
                for entry in section {
                    let data = entry.map_err(|err| CompileError::Validation(err.to_string()))?;
                    let record = match data.kind {
                        DataKind::Active {
                            memory_index,
                            offset_expr,
                        } => {
                            let (base, offset) = const_offset(&offset_expr)?;
                            DataSegRecord {
                                kind: DataSegKind::Active {
                                    memory_index: MemoryIndex::from_u32(memory_index),
                                    base,
                                    offset,
                                },
                                data: data.data.to_vec(),
                            }
                        }
                        DataKind::Passive => DataSegRecord {
                            kind: DataSegKind::Passive,
                            data: data.data.to_vec(),
                        },
                    };
                    translator.info.data_segments.push(record);
                }
            }

            Payload::CodeSectionStart { .. } => {}

            Payload::CodeSectionEntry(body) => {
                let type_index = defined_func_types
                    .get(translator.info.function_bodies.len())
                    .copied()
                    .ok_or_else(|| {
                        CompileError::Internal(
                            "code section has more entries than the function section".to_string(),
                        )
                    })?;
                translate_function_body(translator, body, type_index)?;
            }

            // Never produced for a validated core module with the v1 feature
            // set (tags are handled above; components and the rest are gated
            // off).
            other => {
                return Err(CompileError::Internal(format!(
                    "unexpected payload during translation: {other:?}"
                )));
            }
        }
    }

    Ok(())
}

/// Checks that an atomic address is aligned, trapping `HEAP_OUT_OF_BOUNDS`
/// (the artifact maps heap misalignment to `MemoryOutOfBounds`, matching the
/// historical `HeapMisaligned` trap) if not.
fn align_atomic_addr(
    memarg: &MemArg,
    loaded_bytes: u32,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    // Atomic addresses must all be aligned correctly; the check runs before
    // the out-of-bounds check (matching the threads proposal's current
    // semantics).
    if loaded_bytes > 1 {
        let addr = state.peek1();
        let effective_addr = if memarg.offset == 0 {
            addr
        } else {
            builder
                .ins()
                .iadd_imm_s(addr, i64::from(memarg.offset as i32))
        };
        debug_assert!(loaded_bytes.is_power_of_two());
        let misalignment = builder
            .ins()
            .band_imm_u(effective_addr, i64::from(loaded_bytes - 1));
        let f = builder.ins().icmp_imm_u(IntCC::NotEqual, misalignment, 0);
        builder.ins().trapnz(f, ir::TrapCode::HEAP_OUT_OF_BOUNDS);
    }
}

/// Create a `Block` with the given Wasm parameters.
fn block_with_params(builder: &mut FunctionBuilder, params: &[ValType]) -> Block {
    let block = builder.create_block();
    for ty in params {
        builder.append_block_param(block, crate::types::valtype_to_clif(*ty));
    }
    block
}

/// Get the parameter and result types for the given Wasm blocktype.
fn blocktype_params_results(
    info: &ModuleInfo,
    ty: BlockType,
) -> TranslateResult<(Vec<ValType>, Vec<ValType>)> {
    Ok(match ty {
        BlockType::Empty => (Vec::new(), Vec::new()),
        BlockType::Type(ty) => (Vec::new(), vec![ty]),
        BlockType::FuncType(ty_index) => {
            let ty = &info.wasm_types[TypeIndex::from_u32(ty_index)];
            (ty.params().to_vec(), ty.results().to_vec())
        }
    })
}

/// The same but for a `brif` instruction.
fn canonicalise_brif(
    builder: &mut FunctionBuilder,
    cond: ir::Value,
    block_then: ir::Block,
    params_then: &[ir::Value],
    block_else: ir::Block,
    params_else: &[ir::Value],
) -> ir::Inst {
    let args_then: Vec<BlockArg> = params_then.iter().copied().map(BlockArg::from).collect();
    let args_else: Vec<BlockArg> = params_else.iter().copied().map(BlockArg::from).collect();
    builder
        .ins()
        .brif(cond, block_then, &args_then, block_else, &args_else)
}

/// Generate a `jump` instruction to `destination` with `params`.
fn canonicalise_then_jump(
    builder: &mut FunctionBuilder,
    destination: ir::Block,
    params: &[ir::Value],
) -> ir::Inst {
    let args: Vec<BlockArg> = params.iter().copied().map(BlockArg::from).collect();
    builder.ins().jump(destination, &args)
}

/// Extracts `(base_global, constant_offset)` from an active segment's offset
/// constant expression.
fn const_offset(expr: &ConstExpr<'_>) -> CompileResult<(Option<GlobalIndex>, u64)> {
    let mut ops = expr.get_operators_reader();
    let op = ops
        .read()
        .map_err(|err| CompileError::Validation(err.to_string()))?;
    let out = match op {
        Operator::I32Const { value } => (None, value as u64),
        Operator::GlobalGet { global_index } => (Some(GlobalIndex::from_u32(global_index)), 0),
        Operator::I64Const { value } => (None, value as u64),
        other => {
            return Err(CompileError::Unsupported(format!(
                "unsupported segment offset expression {other:?}"
            )));
        }
    };
    Ok(out)
}

/// Declare `count` local variables of the same type, starting from `next_local`.
fn declare_locals(
    builder: &mut FunctionBuilder,
    count: u32,
    wasm_type: ValType,
    next_local: &mut usize,
) -> TranslateResult<()> {
    // All locals are initialized to 0.
    let (ty, init) = match wasm_type {
        ValType::I32 => (types::I32, Some(builder.ins().iconst(types::I32, 0))),
        ValType::I64 => (types::I64, Some(builder.ins().iconst(types::I64, 0))),
        ValType::F32 => (
            types::F32,
            Some(builder.ins().f32const(Ieee32::with_bits(0))),
        ),
        ValType::F64 => (
            types::F64,
            Some(builder.ins().f64const(Ieee64::with_bits(0))),
        ),
        ValType::V128 => {
            let constant_handle = builder.func.dfg.constants.insert([0; 16].to_vec().into());
            (
                types::I8X16,
                Some(builder.ins().vconst(types::I8X16, constant_handle)),
            )
        }
        // References are raw `u32` handles; the only reference type reachable
        // in the v1 feature set is nullable (`ref.null` = 0).
        ValType::Ref(_) => (types::I32, Some(builder.ins().iconst(types::I32, 0))),
    };

    for _ in 0..count {
        let local = builder.declare_var(ty);
        let init = init.expect("local initialiser");
        builder.def_var(local, init);
        builder.set_val_label(init, ValueLabel::from_u32(*next_local as u32));
        *next_local += 1;
    }
    Ok(())
}

/// Declare local variables for the signature parameters that correspond to
/// WebAssembly locals.
///
/// Returns the number of local variables declared.
fn declare_wasm_parameters(builder: &mut FunctionBuilder, entry_block: Block) -> usize {
    let sig_len = builder.func.signature.params.len();
    let mut next_local: usize = 0;
    for i in 0..sig_len {
        let param = builder.func.signature.params[i];
        // Skip the hidden `VMContext` parameter; every other parameter is a
        // normal wasm parameter.
        if param.purpose == ir::ArgumentPurpose::Normal {
            let local = builder.declare_var(param.value_type);
            next_local += 1;
            let param_value = builder.block_params(entry_block)[i];
            builder.def_var(local, param_value);
            builder.set_val_label(param_value, ValueLabel::from_u32((next_local - 1) as u32));
        }
    }
    next_local
}

/// Maps a wasm load operator to its CLIF opcode and result type.
fn load_opcode_and_type(op: &Operator<'_>) -> (Opcode, ir::Type) {
    use Operator::*;
    match op {
        I32Load { .. } => (Opcode::Load, types::I32),
        I64Load { .. } => (Opcode::Load, types::I64),
        F32Load { .. } => (Opcode::Load, types::F32),
        F64Load { .. } => (Opcode::Load, types::F64),
        I32Load8S { .. } => (Opcode::Sload8, types::I32),
        I32Load8U { .. } => (Opcode::Uload8, types::I32),
        I32Load16S { .. } => (Opcode::Sload16, types::I32),
        I32Load16U { .. } => (Opcode::Uload16, types::I32),
        I64Load8S { .. } => (Opcode::Sload8, types::I64),
        I64Load8U { .. } => (Opcode::Uload8, types::I64),
        I64Load16S { .. } => (Opcode::Sload16, types::I64),
        I64Load16U { .. } => (Opcode::Uload16, types::I64),
        I64Load32S { .. } => (Opcode::Sload32, types::I64),
        I64Load32U { .. } => (Opcode::Uload32, types::I64),
        _ => unreachable!("not a load operator"),
    }
}

/// The size in bytes of the memory access for a load/store opcode.
fn mem_op_size(opcode: Opcode, ty: ir::Type) -> u32 {
    match opcode {
        Opcode::Istore8 | Opcode::Sload8 | Opcode::Uload8 => 1,
        Opcode::Istore16 | Opcode::Sload16 | Opcode::Uload16 => 2,
        Opcode::Istore32 | Opcode::Sload32 | Opcode::Uload32 => 4,
        Opcode::Store | Opcode::Load => ty.bytes(),
        _ => panic!("unknown size of mem op for {opcode:?}"),
    }
}

/// Parse the local variable declarations that precede the function body.
///
/// Declare local variables, starting from `num_params`.
fn parse_local_decls(
    reader: &mut BinaryReader<'_>,
    builder: &mut FunctionBuilder,
    num_params: usize,
) -> TranslateResult<()> {
    let mut next_local = num_params;
    let local_count = reader.read_var_u32()?;

    for _ in 0..local_count {
        let count = reader.read_var_u32()?;
        let ty = reader.read::<ValType>()?;
        declare_locals(builder, count, ty, &mut next_local)?;
    }

    Ok(())
}

/// Extracts the function indices from an element-segment's items, using
/// `FuncIndex::reserved_value()` (`u32::MAX`) for `ref.null` entries (the
/// artifact's encoding for a null element).
fn read_elems(items: &ElementItems<'_>) -> CompileResult<Vec<FuncIndex>> {
    let mut elems = Vec::new();
    match items {
        ElementItems::Functions(funcs) => {
            for func in funcs.clone() {
                let idx = func.map_err(|err| CompileError::Validation(err.to_string()))?;
                elems.push(FuncIndex::from_u32(idx));
            }
        }
        ElementItems::Expressions(_ty, exprs) => {
            for expr in exprs.clone() {
                let expr = expr.map_err(|err| CompileError::Validation(err.to_string()))?;
                let idx = match expr
                    .get_operators_reader()
                    .read()
                    .map_err(|err| CompileError::Validation(err.to_string()))?
                {
                    Operator::RefNull { .. } => FuncIndex::reserved_value(),
                    Operator::RefFunc { function_index } => FuncIndex::from_u32(function_index),
                    other => {
                        return Err(CompileError::Unsupported(format!(
                            "unsupported element-segment initialiser {other:?}"
                        )));
                    }
                };
                elems.push(idx);
            }
        }
    }
    Ok(elems)
}

/// Maps a wasm store operator to its CLIF opcode and stored type.
fn store_opcode_and_type(op: &Operator<'_>, val_ty: ir::Type) -> (Opcode, ir::Type) {
    use Operator::*;
    match op {
        I32Store { .. } => (Opcode::Store, types::I32),
        I64Store { .. } => (Opcode::Store, types::I64),
        F32Store { .. } => (Opcode::Store, types::F32),
        F64Store { .. } => (Opcode::Store, types::F64),
        I32Store8 { .. } => (Opcode::Istore8, val_ty),
        I32Store16 { .. } => (Opcode::Istore16, val_ty),
        I64Store8 { .. } => (Opcode::Istore8, val_ty),
        I64Store16 { .. } => (Opcode::Istore16, val_ty),
        I64Store32 { .. } => (Opcode::Istore32, val_ty),
        _ => unreachable!("not a store operator"),
    }
}

/// Translates an atomic compare-and-swap.
fn translate_atomic_cas(
    widened_ty: ir::Type,
    access_ty: ir::Type,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    let (mut expected, mut replacement) = state.pop2();
    let expected_ty = builder.func.dfg.value_type(expected);
    let replacement_ty = builder.func.dfg.value_type(replacement);

    // The compare-and-swap is performed at type `access_ty`, and the old value
    // is zero-extended to type `widened_ty`.
    debug_assert!(widened_ty.bytes() >= access_ty.bytes());
    debug_assert!(expected_ty.bytes() >= access_ty.bytes());
    debug_assert!(replacement_ty.bytes() >= access_ty.bytes());
    if expected_ty.bytes() > access_ty.bytes() {
        expected = builder.ins().ireduce(access_ty, expected);
    }
    if replacement_ty.bytes() > access_ty.bytes() {
        replacement = builder.ins().ireduce(access_ty, replacement);
    }

    align_atomic_addr(memarg, access_ty.bytes(), builder, state);
    let index = state.pop1();
    let (addr, _) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        access_ty.bytes(),
    )?;
    let atomic_flags = mem_access_flags();

    let mut res = builder
        .ins()
        .atomic_cas(atomic_flags, addr, expected, replacement);
    if access_ty != widened_ty {
        res = builder.ins().uextend(widened_ty, res);
    }
    state.push1(res);
    Ok(())
}

/// Translates an atomic load.
fn translate_atomic_load(
    widened_ty: ir::Type,
    access_ty: ir::Type,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    // The load is performed at type `access_ty`, and the loaded value is zero
    // extended to `widened_ty`.
    debug_assert!(widened_ty.bytes() >= access_ty.bytes());

    align_atomic_addr(memarg, access_ty.bytes(), builder, state);
    let index = state.pop1();
    let (addr, _) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        access_ty.bytes(),
    )?;
    let atomic_flags = mem_access_flags();

    let mut res = builder.ins().atomic_load(access_ty, atomic_flags, addr);
    if access_ty != widened_ty {
        res = builder.ins().uextend(widened_ty, res);
    }
    state.push1(res);
    Ok(())
}

/// Translates an atomic read-modify-write.
fn translate_atomic_rmw(
    widened_ty: ir::Type,
    access_ty: ir::Type,
    op: AtomicRmwOp,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    let mut arg2 = state.pop1();
    let arg2_ty = builder.func.dfg.value_type(arg2);

    // The operation is performed at type `access_ty`, and the old value is
    // zero-extended to type `widened_ty`.
    debug_assert!(widened_ty.bytes() >= access_ty.bytes());
    debug_assert!(arg2_ty.bytes() >= access_ty.bytes());
    if arg2_ty.bytes() > access_ty.bytes() {
        arg2 = builder.ins().ireduce(access_ty, arg2);
    }

    align_atomic_addr(memarg, access_ty.bytes(), builder, state);
    let index = state.pop1();
    let (addr, _) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        access_ty.bytes(),
    )?;
    let atomic_flags = mem_access_flags();

    let mut res = builder
        .ins()
        .atomic_rmw(access_ty, atomic_flags, op, addr, arg2);
    if access_ty != widened_ty {
        res = builder.ins().uextend(widened_ty, res);
    }
    state.push1(res);
    Ok(())
}

/// Translates an atomic store.
fn translate_atomic_store(
    access_ty: ir::Type,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    let mut data = state.pop1();
    let data_ty = builder.func.dfg.value_type(data);

    // The operation is performed at type `access_ty`, and the data to be
    // stored may first need to be narrowed accordingly.
    debug_assert!(data_ty.bytes() >= access_ty.bytes());
    if data_ty.bytes() > access_ty.bytes() {
        data = builder.ins().ireduce(access_ty, data);
    }

    align_atomic_addr(memarg, access_ty.bytes(), builder, state);
    let index = state.pop1();
    let (addr, _) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        access_ty.bytes(),
    )?;
    let atomic_flags = mem_access_flags();

    builder.ins().atomic_store(atomic_flags, data, addr);
    Ok(())
}

/// Translates a binary integer or float operator.
fn translate_binop(
    opcode: Opcode,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let (arg0, arg1) = state.pop2();
    let res = match opcode {
        Opcode::Iadd => builder.ins().iadd(arg0, arg1),
        Opcode::Isub => builder.ins().isub(arg0, arg1),
        Opcode::Imul => builder.ins().imul(arg0, arg1),
        Opcode::Sdiv => builder.ins().sdiv(arg0, arg1),
        Opcode::Udiv => builder.ins().udiv(arg0, arg1),
        Opcode::Srem => builder.ins().srem(arg0, arg1),
        Opcode::Urem => builder.ins().urem(arg0, arg1),
        Opcode::Band => builder.ins().band(arg0, arg1),
        Opcode::Bor => builder.ins().bor(arg0, arg1),
        Opcode::Bxor => builder.ins().bxor(arg0, arg1),
        Opcode::Ishl => builder.ins().ishl(arg0, arg1),
        Opcode::Sshr => builder.ins().sshr(arg0, arg1),
        Opcode::Ushr => builder.ins().ushr(arg0, arg1),
        Opcode::Rotl => builder.ins().rotl(arg0, arg1),
        Opcode::Rotr => builder.ins().rotr(arg0, arg1),
        Opcode::Fadd => builder.ins().fadd(arg0, arg1),
        Opcode::Fsub => builder.ins().fsub(arg0, arg1),
        Opcode::Fmul => builder.ins().fmul(arg0, arg1),
        Opcode::Fdiv => builder.ins().fdiv(arg0, arg1),
        Opcode::Fmin => builder.ins().fmin(arg0, arg1),
        Opcode::Fmax => builder.ins().fmax(arg0, arg1),
        Opcode::Fcopysign => builder.ins().fcopysign(arg0, arg1),
        _ => unreachable!("not a binary operator"),
    };
    state.push1(res);
}

fn translate_br_if(
    relative_depth: u32,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    let (br_destination, inputs) = translate_br_if_args(relative_depth, state);
    let next_block = builder.create_block();
    canonicalise_brif(builder, val, br_destination, inputs, next_block, &[]);

    builder.seal_block(next_block); // The only predecessor is the current block.
    builder.switch_to_block(next_block);
}

fn translate_br_if_args(
    relative_depth: u32,
    state: &mut FuncTranslationState,
) -> (ir::Block, &mut [ir::Value]) {
    let i = state.control_stack.len() - 1 - (relative_depth as usize);
    let (return_count, br_destination) = {
        let frame = &mut state.control_stack[i];
        // The values returned by the branch are still available for the
        // reachable code that comes after it.
        frame.set_branched_to_exit();
        let return_count = if frame.is_loop() {
            frame.num_param_values()
        } else {
            frame.num_return_values()
        };
        (return_count, frame.br_destination())
    };
    let inputs = state.peekn_mut(return_count);
    (br_destination, inputs)
}

/// Translates an `fcmp` operator, pushing the zero-extended `i32` result.
fn translate_fcmp(cc: FloatCC, builder: &mut FunctionBuilder, state: &mut FuncTranslationState) {
    let (arg0, arg1) = state.pop2();
    let val = builder.ins().fcmp(cc, arg0, arg1);
    state.push1(builder.ins().uextend(types::I32, val));
}

/// Translates an int-to-float conversion.
fn translate_fcvt_from_sint(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_from_sint(result_ty, val));
}

fn translate_fcvt_from_uint(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_from_uint(result_ty, val));
}

/// Translates a float-to-int conversion that traps on overflow / invalid.
fn translate_fcvt_to_sint(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_to_sint(result_ty, val));
}

/// Translates a saturating float-to-int conversion.
fn translate_fcvt_to_sint_sat(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_to_sint_sat(result_ty, val));
}

fn translate_fcvt_to_uint(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_to_uint(result_ty, val));
}

fn translate_fcvt_to_uint_sat(
    result_ty: ir::Type,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
) {
    let val = state.pop1();
    state.push1(builder.ins().fcvt_to_uint_sat(result_ty, val));
}

/// Translates one defined function body into CLIF and stores it.
fn translate_function_body(
    translator: &mut Translator,
    body: FunctionBody<'_>,
    type_index: TypeIndex,
) -> CompileResult<()> {
    let sig = translator.func_env().vmctx_sig(type_index);
    let defined_index = translator.info.function_bodies.len();
    let mut func =
        ir::Function::with_name_signature(ir::UserFuncName::user(0, defined_index as u32), sig);

    let mut func_env = FuncEnv::new(&translator.info, translator.stack_pointer_override);
    translator
        .trans
        .translate_body(body, &mut func, &mut func_env)
        .map_err(crate::environment::translate_error)?;
    translator.info.function_bodies.push(func);
    Ok(())
}

/// Translates an `icmp` operator, pushing the zero-extended `i32` result.
fn translate_icmp(cc: IntCC, builder: &mut FunctionBuilder, state: &mut FuncTranslationState) {
    let (arg0, arg1) = state.pop2();
    let val = builder.ins().icmp(cc, arg0, arg1);
    state.push1(builder.ins().uextend(types::I32, val));
}

/// Translates a wasm load instruction into a bounds-checked CLIF load.
fn translate_load(
    op: &Operator<'_>,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    let (opcode, result_ty) = load_opcode_and_type(op);
    let index = state.pop1();
    let mem_op_size = mem_op_size(opcode, result_ty);
    let (addr, flags) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        mem_op_size,
    )?;
    let offset = Offset32::new(0);
    let val = match opcode {
        Opcode::Load => builder.ins().load(result_ty, flags, addr, offset),
        Opcode::Sload8 => builder.ins().sload8(result_ty, flags, addr, offset),
        Opcode::Uload8 => builder.ins().uload8(result_ty, flags, addr, offset),
        Opcode::Sload16 => builder.ins().sload16(result_ty, flags, addr, offset),
        Opcode::Uload16 => builder.ins().uload16(result_ty, flags, addr, offset),
        // `sload32`/`uload32` infer their result type from the address
        // operand (always i64 for wasm32), so no explicit type is passed.
        Opcode::Sload32 => builder.ins().sload32(flags, addr, offset),
        Opcode::Uload32 => builder.ins().uload32(flags, addr, offset),
        _ => unreachable!("not a load opcode"),
    };
    state.push1(val);
    Ok(())
}

/// Translates a single operator.
fn translate_operator(
    op: &Operator<'_>,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    if !state.reachable {
        translate_unreachable_operator(op, builder, state, environ)?;
        return Ok(());
    }

    match op {
        /********************************** Locals ****************************************/
        Operator::LocalGet { local_index } => {
            let val = builder.use_var(Variable::from_u32(*local_index));
            state.push1(val);
        }
        Operator::LocalSet { local_index } => {
            let val = state.pop1();
            builder.def_var(Variable::from_u32(*local_index), val);
        }
        Operator::LocalTee { local_index } => {
            let val = state.peek1();
            builder.def_var(Variable::from_u32(*local_index), val);
        }

        /********************************** Globals ****************************************/
        Operator::GlobalGet { global_index } => {
            let GlobalVar { base, offset, ty } =
                environ.make_global(builder, GlobalIndex::from_u32(*global_index))?;
            let addr = builder.ins().iadd_imm_s(base, i64::from(offset));
            let val = builder
                .ins()
                .load(ty, MemFlagsData::trusted(), addr, Offset32::new(0));
            state.push1(val);
        }
        Operator::GlobalSet { global_index } => {
            let val = state.pop1();
            let GlobalVar { base, offset, .. } =
                environ.make_global(builder, GlobalIndex::from_u32(*global_index))?;
            let addr = builder.ins().iadd_imm_s(base, i64::from(offset));
            builder
                .ins()
                .store(MemFlagsData::trusted(), val, addr, Offset32::new(0));
        }

        /********************************** Parametric ****************************************/
        Operator::Drop => {
            let _ = state.pop1();
        }
        Operator::Select => {
            let (arg1, arg2, cond) = state.pop3();
            state.push1(builder.ins().select(cond, arg1, arg2));
        }
        Operator::TypedSelect { .. } => {
            let (arg1, arg2, cond) = state.pop3();
            state.push1(builder.ins().select(cond, arg1, arg2));
        }

        /********************************** Misc ****************************************/
        Operator::Nop => {
            // Nothing to do.
        }
        Operator::Unreachable => {
            builder.ins().trap(user_trap(USER_TRAP_UNREACHABLE));
            state.reachable = false;
        }

        /***************************** Control flow blocks **********************************/
        Operator::Block { blockty } => {
            let (params, results) = blocktype_params_results(environ.module_info(), *blockty)?;
            let next = block_with_params(builder, &results);
            state.push_block(next, params.len(), results.len());
        }
        Operator::Loop { blockty } => {
            let (params, results) = blocktype_params_results(environ.module_info(), *blockty)?;
            let loop_body = block_with_params(builder, &params);
            let next = block_with_params(builder, &results);
            canonicalise_then_jump(builder, loop_body, state.peekn(params.len()));
            state.push_loop(loop_body, next, params.len(), results.len());

            // Pop the initial `Block` actuals and replace them with the
            // `Block`'s params since control flow joins at the top of the loop.
            state.popn(params.len());
            state
                .stack
                .extend_from_slice(builder.block_params(loop_body));

            builder.switch_to_block(loop_body);
        }
        Operator::If { blockty } => {
            let val = state.pop1();

            let next_block = builder.create_block();
            let (params, results) = blocktype_params_results(environ.module_info(), *blockty)?;
            let (destination, else_data) = if params == results {
                // It is possible there is no `else` block, so we will only
                // allocate a block for it if/when we find the `else`. For now,
                // if the condition isn't true, we jump directly to the
                // destination block following the whole `if...end`.
                let destination = block_with_params(builder, &results);
                let branch_inst = canonicalise_brif(
                    builder,
                    val,
                    next_block,
                    &[],
                    destination,
                    state.peekn(params.len()),
                );
                (
                    destination,
                    ElseData::NoElse {
                        branch_inst,
                        placeholder: destination,
                    },
                )
            } else {
                // The `if` type signature is not valid without an `else`
                // block, so we eagerly allocate the `else` block here.
                let destination = block_with_params(builder, &results);
                let else_block = block_with_params(builder, &params);
                canonicalise_brif(
                    builder,
                    val,
                    next_block,
                    &[],
                    else_block,
                    state.peekn(params.len()),
                );
                builder.seal_block(else_block);
                (destination, ElseData::WithElse { else_block })
            };

            builder.seal_block(next_block); // Only predecessor is the current block.
            builder.switch_to_block(next_block);

            state.push_if(
                destination,
                else_data,
                params.len(),
                results.len(),
                *blockty,
            );
        }
        Operator::Else => {
            let i = state.control_stack.len() - 1;
            match state.control_stack[i] {
                ControlStackFrame::If {
                    ref else_data,
                    head_is_reachable,
                    ref mut consequent_ends_reachable,
                    num_return_values,
                    blocktype,
                    destination,
                    ..
                } => {
                    // We finished the consequent, so record its final
                    // reachability state.
                    debug_assert!(consequent_ends_reachable.is_none());
                    *consequent_ends_reachable = Some(state.reachable);

                    if head_is_reachable {
                        // We have a branch from the head of the `if` to the `else`.
                        state.reachable = true;

                        // Ensure we have a block for the `else` block (it may
                        // have already been pre-allocated).
                        let else_block = match *else_data {
                            ElseData::NoElse {
                                branch_inst,
                                placeholder,
                            } => {
                                let (params, _results) =
                                    blocktype_params_results(environ.module_info(), blocktype)?;
                                let else_block = block_with_params(builder, &params);
                                canonicalise_then_jump(
                                    builder,
                                    destination,
                                    state.peekn(params.len()),
                                );
                                state.popn(params.len());

                                builder.change_jump_destination(
                                    branch_inst,
                                    placeholder,
                                    else_block,
                                );
                                builder.seal_block(else_block);
                                else_block
                            }
                            ElseData::WithElse { else_block } => {
                                canonicalise_then_jump(
                                    builder,
                                    destination,
                                    state.peekn(num_return_values),
                                );
                                state.popn(num_return_values);
                                else_block
                            }
                        };

                        // The parameters for this `else` block are already on
                        // the top of the stack: we pushed the parameters twice
                        // when we saw the initial `if`.
                        builder.switch_to_block(else_block);
                    }
                }
                _ => unreachable!(),
            }
        }
        Operator::End => {
            let frame = state.control_stack.pop().unwrap();
            let next_block = frame.following_code();
            let return_count = frame.num_return_values();
            let return_args = state.peekn_mut(return_count);

            canonicalise_then_jump(builder, next_block, return_args);
            // No need to clean up a duplicate set of parameters for a
            // no-else `if`: the stack is truncated to its original size below.

            builder.switch_to_block(next_block);
            builder.seal_block(next_block);

            // If it is a loop we also have to seal the body loop block.
            if let ControlStackFrame::Loop { header, .. } = frame {
                builder.seal_block(header)
            }

            frame.truncate_value_stack_to_original_size(&mut state.stack);
            state
                .stack
                .extend_from_slice(builder.block_params(next_block));
        }

        /**************************** Branch instructions *********************************/
        Operator::Br { relative_depth } => {
            let i = state.control_stack.len() - 1 - (*relative_depth as usize);
            let (return_count, br_destination) = {
                let frame = &mut state.control_stack[i];
                // We signal that all the code that follows until the next End
                // is unreachable.
                frame.set_branched_to_exit();
                let return_count = if frame.is_loop() {
                    frame.num_param_values()
                } else {
                    frame.num_return_values()
                };
                (return_count, frame.br_destination())
            };
            let destination_args = state.peekn_mut(return_count);
            canonicalise_then_jump(builder, br_destination, destination_args);
            state.popn(return_count);
            state.reachable = false;
        }
        Operator::BrIf { relative_depth } => {
            translate_br_if(*relative_depth, builder, state);
        }
        Operator::BrTable { targets } => {
            let default = targets.default();
            let mut min_depth = default;
            for depth in targets.targets() {
                let depth = depth?;
                if depth < min_depth {
                    min_depth = depth;
                }
            }
            let jump_args_count = {
                let i = state.control_stack.len() - 1 - (min_depth as usize);
                let min_depth_frame = &state.control_stack[i];
                if min_depth_frame.is_loop() {
                    min_depth_frame.num_param_values()
                } else {
                    min_depth_frame.num_return_values()
                }
            };
            let val = state.pop1();
            let mut data = Vec::with_capacity(targets.len() as usize);
            if jump_args_count == 0 {
                // No jump arguments.
                for depth in targets.targets() {
                    let depth = depth?;
                    let block = {
                        let i = state.control_stack.len() - 1 - (depth as usize);
                        let frame = &mut state.control_stack[i];
                        frame.set_branched_to_exit();
                        frame.br_destination()
                    };
                    data.push(builder.func.dfg.block_call(block, &[] as &[BlockArg]));
                }
                let block = {
                    let i = state.control_stack.len() - 1 - (default as usize);
                    let frame = &mut state.control_stack[i];
                    frame.set_branched_to_exit();
                    frame.br_destination()
                };
                let block = builder.func.dfg.block_call(block, &[] as &[BlockArg]);
                let jt = builder.create_jump_table(JumpTableData::new(block, &data));
                builder.ins().br_table(val, jt);
            } else {
                // Here we have jump arguments, but Cranelift's br_table
                // doesn't support them: split the edges going out of the
                // br_table.
                let return_count = jump_args_count;
                let mut dest_block_sequence = vec![];
                let mut dest_block_map = HashMap::new();
                for depth in targets.targets() {
                    let depth = depth?;
                    let branch_block = match dest_block_map.entry(depth as usize) {
                        std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            let block = builder.create_block();
                            dest_block_sequence.push((depth as usize, block));
                            *entry.insert(block)
                        }
                    };
                    data.push(
                        builder
                            .func
                            .dfg
                            .block_call(branch_block, &[] as &[BlockArg]),
                    );
                }
                let default_branch_block = match dest_block_map.entry(default as usize) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let block = builder.create_block();
                        dest_block_sequence.push((default as usize, block));
                        *entry.insert(block)
                    }
                };
                let default_branch_block = builder
                    .func
                    .dfg
                    .block_call(default_branch_block, &[] as &[BlockArg]);
                let jt = builder.create_jump_table(JumpTableData::new(default_branch_block, &data));
                builder.ins().br_table(val, jt);
                for (depth, dest_block) in dest_block_sequence {
                    builder.switch_to_block(dest_block);
                    builder.seal_block(dest_block);
                    let real_dest_block = {
                        let i = state.control_stack.len() - 1 - depth;
                        let frame = &mut state.control_stack[i];
                        frame.set_branched_to_exit();
                        frame.br_destination()
                    };
                    let destination_args = state.peekn_mut(return_count);
                    canonicalise_then_jump(builder, real_dest_block, destination_args);
                }
                state.popn(return_count);
            }
            state.reachable = false;
        }
        Operator::Return => {
            let return_count = {
                let frame = &mut state.control_stack[0];
                frame.num_return_values()
            };
            let return_args = state.peekn_mut(return_count);
            let args = return_args.to_vec();
            builder.ins().return_(&args);
            state.popn(return_count);
            state.reachable = false;
        }

        /************************************ Calls ****************************************/
        Operator::Call { function_index } => {
            let (fref, num_args) =
                environ.make_direct_func(builder.func, FuncIndex::from_u32(*function_index))?;
            let args = state.peekn_mut(num_args);
            let call = environ.translate_call(
                builder,
                FuncIndex::from_u32(*function_index),
                fref,
                args,
            )?;
            let inst_results = builder.inst_results(call);
            state.popn(num_args);
            state.pushn(inst_results);
        }
        Operator::CallIndirect {
            type_index,
            table_index,
        } => {
            let (sig_ref, num_args) =
                environ.make_indirect_sig(builder.func, TypeIndex::from_u32(*type_index))?;
            let callee = state.pop1();
            let args = state.peekn_mut(num_args);
            let call = environ.translate_call_indirect(
                builder,
                TableIndex::from_u32(*table_index),
                TypeIndex::from_u32(*type_index),
                sig_ref,
                callee,
                args,
            )?;
            let call = call.ok_or_else(|| {
                TranslateError::Other("call_indirect produced no instruction".to_string())
            })?;
            let inst_results = builder.inst_results(call);
            state.popn(num_args);
            state.pushn(inst_results);
        }
        Operator::ReturnCall { .. }
        | Operator::ReturnCallIndirect { .. }
        | Operator::ReturnCallRef { .. }
        | Operator::CallRef { .. } => {
            return Err(TranslateError::Unsupported(format!(
                "operator {:?} is not supported (tail calls / function references are \
                 outside the v1 feature set)",
                op
            )));
        }

        /************************************ Memory ****************************************/
        Operator::I32Load { memarg }
        | Operator::I64Load { memarg }
        | Operator::F32Load { memarg }
        | Operator::F64Load { memarg }
        | Operator::I32Load8S { memarg }
        | Operator::I32Load8U { memarg }
        | Operator::I32Load16S { memarg }
        | Operator::I32Load16U { memarg }
        | Operator::I64Load8S { memarg }
        | Operator::I64Load8U { memarg }
        | Operator::I64Load16S { memarg }
        | Operator::I64Load16U { memarg }
        | Operator::I64Load32S { memarg }
        | Operator::I64Load32U { memarg } => {
            translate_load(op, memarg, builder, state, environ)?;
        }
        Operator::I32Store { memarg }
        | Operator::I64Store { memarg }
        | Operator::F32Store { memarg }
        | Operator::F64Store { memarg }
        | Operator::I32Store8 { memarg }
        | Operator::I32Store16 { memarg }
        | Operator::I64Store8 { memarg }
        | Operator::I64Store16 { memarg }
        | Operator::I64Store32 { memarg } => {
            translate_store(op, memarg, builder, state, environ)?;
        }
        Operator::MemorySize { mem } => {
            let val =
                environ.translate_memory_size(builder.cursor(), MemoryIndex::from_u32(*mem))?;
            state.push1(val);
        }
        Operator::MemoryGrow { mem } => {
            let val = state.pop1();
            let result = environ.translate_memory_grow(
                builder.cursor(),
                MemoryIndex::from_u32(*mem),
                val,
            )?;
            state.push1(result);
        }
        Operator::MemoryCopy { dst_mem, src_mem } => {
            let (dst, src, len) = state.pop3();
            environ.translate_memory_copy(
                builder.cursor(),
                MemoryIndex::from_u32(*src_mem),
                MemoryIndex::from_u32(*dst_mem),
                dst,
                src,
                len,
            )?;
        }
        Operator::MemoryFill { mem } => {
            let (dst, val, len) = state.pop3();
            environ.translate_memory_fill(
                builder.cursor(),
                MemoryIndex::from_u32(*mem),
                dst,
                val,
                len,
            )?;
        }
        Operator::MemoryInit { data_index, mem } => {
            let (dst, src, len) = state.pop3();
            environ.translate_memory_init(
                builder.cursor(),
                MemoryIndex::from_u32(*mem),
                *data_index,
                dst,
                src,
                len,
            )?;
        }
        Operator::DataDrop { data_index } => {
            environ.translate_data_drop(builder.cursor(), *data_index)?;
        }

        /************************************ Tables ****************************************/
        Operator::TableGet { table } => {
            let index = state.pop1();
            let val = environ.translate_table_get(builder, TableIndex::from_u32(*table), index)?;
            state.push1(val);
        }
        Operator::TableSet { table } => {
            // Stack is `[index value]`; pop the value first.
            let value = state.pop1();
            let index = state.pop1();
            environ.translate_table_set(builder, TableIndex::from_u32(*table), value, index)?;
        }
        Operator::TableGrow { table } => {
            let (init_value, delta) = state.pop2();
            let val = environ.translate_table_grow(
                builder.cursor(),
                TableIndex::from_u32(*table),
                delta,
                init_value,
            )?;
            state.push1(val);
        }
        Operator::TableSize { table } => {
            let val =
                environ.translate_table_size(builder.cursor(), TableIndex::from_u32(*table))?;
            state.push1(val);
        }
        Operator::TableFill { table } => {
            let (dst, val, len) = state.pop3();
            environ.translate_table_fill(
                builder.cursor(),
                TableIndex::from_u32(*table),
                dst,
                val,
                len,
            )?;
        }
        Operator::TableCopy {
            dst_table,
            src_table,
        } => {
            let (dst, src, len) = state.pop3();
            environ.translate_table_copy(
                builder.cursor(),
                TableIndex::from_u32(*dst_table),
                TableIndex::from_u32(*src_table),
                dst,
                src,
                len,
            )?;
        }
        Operator::TableInit { elem_index, table } => {
            let (dst, src, len) = state.pop3();
            environ.translate_table_init(
                builder.cursor(),
                *elem_index,
                TableIndex::from_u32(*table),
                dst,
                src,
                len,
            )?;
        }
        Operator::ElemDrop { elem_index } => {
            environ.translate_elem_drop(builder.cursor(), *elem_index)?;
        }

        /************************************ References ****************************************/
        Operator::RefNull { hty } => {
            let val = environ.translate_ref_null(builder.cursor(), *hty);
            state.push1(val);
        }
        Operator::RefIsNull => {
            let value = state.pop1();
            let val = environ.translate_ref_is_null(builder.cursor(), value);
            state.push1(val);
        }
        Operator::RefFunc { function_index } => {
            let val = environ
                .translate_ref_func(builder.cursor(), FuncIndex::from_u32(*function_index))?;
            state.push1(val);
        }
        Operator::RefI31 { .. } | Operator::I31GetS { .. } | Operator::I31GetU { .. } => {
            return Err(TranslateError::Unsupported(format!(
                "operator {:?} is not supported (GC is outside the v1 feature set)",
                op
            )));
        }

        /************************************ Constants ****************************************/
        Operator::I32Const { value } => {
            state.push1(builder.ins().iconst(types::I32, i64::from(*value)));
        }
        Operator::I64Const { value } => {
            state.push1(builder.ins().iconst(types::I64, *value));
        }
        Operator::F32Const { value } => {
            state.push1(builder.ins().f32const(Ieee32::with_bits(value.bits())));
        }
        Operator::F64Const { value } => {
            state.push1(builder.ins().f64const(Ieee64::with_bits(value.bits())));
        }

        /************************************ i32 ****************************************/
        Operator::I32Eqz => {
            let val = state.pop1();
            let cmp = builder.ins().icmp_imm_u(IntCC::Equal, val, 0);
            state.push1(builder.ins().uextend(types::I32, cmp));
        }
        Operator::I32Eq => translate_icmp(IntCC::Equal, builder, state),
        Operator::I32Ne => translate_icmp(IntCC::NotEqual, builder, state),
        Operator::I32LtS => translate_icmp(IntCC::SignedLessThan, builder, state),
        Operator::I32LtU => translate_icmp(IntCC::UnsignedLessThan, builder, state),
        Operator::I32GtS => translate_icmp(IntCC::SignedGreaterThan, builder, state),
        Operator::I32GtU => translate_icmp(IntCC::UnsignedGreaterThan, builder, state),
        Operator::I32LeS => translate_icmp(IntCC::SignedLessThanOrEqual, builder, state),
        Operator::I32LeU => translate_icmp(IntCC::UnsignedLessThanOrEqual, builder, state),
        Operator::I32GeS => translate_icmp(IntCC::SignedGreaterThanOrEqual, builder, state),
        Operator::I32GeU => translate_icmp(IntCC::UnsignedGreaterThanOrEqual, builder, state),
        Operator::I32Clz => translate_unop(Opcode::Clz, builder, state),
        Operator::I32Ctz => translate_unop(Opcode::Ctz, builder, state),
        Operator::I32Popcnt => translate_unop(Opcode::Popcnt, builder, state),
        Operator::I32Add => translate_binop(Opcode::Iadd, builder, state),
        Operator::I32Sub => translate_binop(Opcode::Isub, builder, state),
        Operator::I32Mul => translate_binop(Opcode::Imul, builder, state),
        Operator::I32DivS => translate_binop(Opcode::Sdiv, builder, state),
        Operator::I32DivU => translate_binop(Opcode::Udiv, builder, state),
        Operator::I32RemS => translate_binop(Opcode::Srem, builder, state),
        Operator::I32RemU => translate_binop(Opcode::Urem, builder, state),
        Operator::I32And => translate_binop(Opcode::Band, builder, state),
        Operator::I32Or => translate_binop(Opcode::Bor, builder, state),
        Operator::I32Xor => translate_binop(Opcode::Bxor, builder, state),
        Operator::I32Shl => translate_binop(Opcode::Ishl, builder, state),
        Operator::I32ShrS => translate_binop(Opcode::Sshr, builder, state),
        Operator::I32ShrU => translate_binop(Opcode::Ushr, builder, state),
        Operator::I32Rotl => translate_binop(Opcode::Rotl, builder, state),
        Operator::I32Rotr => translate_binop(Opcode::Rotr, builder, state),

        /************************************ i64 ****************************************/
        Operator::I64Eqz => {
            let val = state.pop1();
            let cmp = builder.ins().icmp_imm_u(IntCC::Equal, val, 0);
            state.push1(builder.ins().uextend(types::I32, cmp));
        }
        Operator::I64Eq => translate_icmp(IntCC::Equal, builder, state),
        Operator::I64Ne => translate_icmp(IntCC::NotEqual, builder, state),
        Operator::I64LtS => translate_icmp(IntCC::SignedLessThan, builder, state),
        Operator::I64LtU => translate_icmp(IntCC::UnsignedLessThan, builder, state),
        Operator::I64GtS => translate_icmp(IntCC::SignedGreaterThan, builder, state),
        Operator::I64GtU => translate_icmp(IntCC::UnsignedGreaterThan, builder, state),
        Operator::I64LeS => translate_icmp(IntCC::SignedLessThanOrEqual, builder, state),
        Operator::I64LeU => translate_icmp(IntCC::UnsignedLessThanOrEqual, builder, state),
        Operator::I64GeS => translate_icmp(IntCC::SignedGreaterThanOrEqual, builder, state),
        Operator::I64GeU => translate_icmp(IntCC::UnsignedGreaterThanOrEqual, builder, state),
        Operator::I64Clz => translate_unop(Opcode::Clz, builder, state),
        Operator::I64Ctz => translate_unop(Opcode::Ctz, builder, state),
        Operator::I64Popcnt => translate_unop(Opcode::Popcnt, builder, state),
        Operator::I64Add => translate_binop(Opcode::Iadd, builder, state),
        Operator::I64Sub => translate_binop(Opcode::Isub, builder, state),
        Operator::I64Mul => translate_binop(Opcode::Imul, builder, state),
        Operator::I64DivS => translate_binop(Opcode::Sdiv, builder, state),
        Operator::I64DivU => translate_binop(Opcode::Udiv, builder, state),
        Operator::I64RemS => translate_binop(Opcode::Srem, builder, state),
        Operator::I64RemU => translate_binop(Opcode::Urem, builder, state),
        Operator::I64And => translate_binop(Opcode::Band, builder, state),
        Operator::I64Or => translate_binop(Opcode::Bor, builder, state),
        Operator::I64Xor => translate_binop(Opcode::Bxor, builder, state),
        Operator::I64Shl => translate_binop(Opcode::Ishl, builder, state),
        Operator::I64ShrS => translate_binop(Opcode::Sshr, builder, state),
        Operator::I64ShrU => translate_binop(Opcode::Ushr, builder, state),
        Operator::I64Rotl => translate_binop(Opcode::Rotl, builder, state),
        Operator::I64Rotr => translate_binop(Opcode::Rotr, builder, state),

        /************************************ f32 ****************************************/
        Operator::F32Abs => translate_unop(Opcode::Fabs, builder, state),
        Operator::F32Neg => translate_unop(Opcode::Fneg, builder, state),
        Operator::F32Ceil => translate_unop(Opcode::Ceil, builder, state),
        Operator::F32Floor => translate_unop(Opcode::Floor, builder, state),
        Operator::F32Trunc => translate_unop(Opcode::Trunc, builder, state),
        Operator::F32Nearest => translate_unop(Opcode::Nearest, builder, state),
        Operator::F32Sqrt => translate_unop(Opcode::Sqrt, builder, state),
        Operator::F32Add => translate_binop(Opcode::Fadd, builder, state),
        Operator::F32Sub => translate_binop(Opcode::Fsub, builder, state),
        Operator::F32Mul => translate_binop(Opcode::Fmul, builder, state),
        Operator::F32Div => translate_binop(Opcode::Fdiv, builder, state),
        Operator::F32Min => translate_binop(Opcode::Fmin, builder, state),
        Operator::F32Max => translate_binop(Opcode::Fmax, builder, state),
        Operator::F32Copysign => translate_binop(Opcode::Fcopysign, builder, state),
        Operator::F32Eq => translate_fcmp(FloatCC::Equal, builder, state),
        Operator::F32Ne => translate_fcmp(FloatCC::NotEqual, builder, state),
        Operator::F32Lt => translate_fcmp(FloatCC::LessThan, builder, state),
        Operator::F32Gt => translate_fcmp(FloatCC::GreaterThan, builder, state),
        Operator::F32Le => translate_fcmp(FloatCC::LessThanOrEqual, builder, state),
        Operator::F32Ge => translate_fcmp(FloatCC::GreaterThanOrEqual, builder, state),

        /************************************ f64 ****************************************/
        Operator::F64Abs => translate_unop(Opcode::Fabs, builder, state),
        Operator::F64Neg => translate_unop(Opcode::Fneg, builder, state),
        Operator::F64Ceil => translate_unop(Opcode::Ceil, builder, state),
        Operator::F64Floor => translate_unop(Opcode::Floor, builder, state),
        Operator::F64Trunc => translate_unop(Opcode::Trunc, builder, state),
        Operator::F64Nearest => translate_unop(Opcode::Nearest, builder, state),
        Operator::F64Sqrt => translate_unop(Opcode::Sqrt, builder, state),
        Operator::F64Add => translate_binop(Opcode::Fadd, builder, state),
        Operator::F64Sub => translate_binop(Opcode::Fsub, builder, state),
        Operator::F64Mul => translate_binop(Opcode::Fmul, builder, state),
        Operator::F64Div => translate_binop(Opcode::Fdiv, builder, state),
        Operator::F64Min => translate_binop(Opcode::Fmin, builder, state),
        Operator::F64Max => translate_binop(Opcode::Fmax, builder, state),
        Operator::F64Copysign => translate_binop(Opcode::Fcopysign, builder, state),
        Operator::F64Eq => translate_fcmp(FloatCC::Equal, builder, state),
        Operator::F64Ne => translate_fcmp(FloatCC::NotEqual, builder, state),
        Operator::F64Lt => translate_fcmp(FloatCC::LessThan, builder, state),
        Operator::F64Gt => translate_fcmp(FloatCC::GreaterThan, builder, state),
        Operator::F64Le => translate_fcmp(FloatCC::LessThanOrEqual, builder, state),
        Operator::F64Ge => translate_fcmp(FloatCC::GreaterThanOrEqual, builder, state),

        /************************************ Conversions ****************************************/
        Operator::I32WrapI64 => {
            let val = state.pop1();
            state.push1(builder.ins().ireduce(types::I32, val));
        }
        Operator::I32TruncF32S => translate_fcvt_to_sint(types::I32, builder, state),
        Operator::I32TruncF32U => translate_fcvt_to_uint(types::I32, builder, state),
        Operator::I32TruncF64S => translate_fcvt_to_sint(types::I32, builder, state),
        Operator::I32TruncF64U => translate_fcvt_to_uint(types::I32, builder, state),
        Operator::I64ExtendI32S => {
            let val = state.pop1();
            state.push1(builder.ins().sextend(types::I64, val));
        }
        Operator::I64ExtendI32U => {
            let val = state.pop1();
            state.push1(builder.ins().uextend(types::I64, val));
        }
        Operator::I64TruncF32S => translate_fcvt_to_sint(types::I64, builder, state),
        Operator::I64TruncF32U => translate_fcvt_to_uint(types::I64, builder, state),
        Operator::I64TruncF64S => translate_fcvt_to_sint(types::I64, builder, state),
        Operator::I64TruncF64U => translate_fcvt_to_uint(types::I64, builder, state),
        Operator::F32ConvertI32S => translate_fcvt_from_sint(types::F32, builder, state),
        Operator::F32ConvertI32U => translate_fcvt_from_uint(types::F32, builder, state),
        Operator::F32ConvertI64S => translate_fcvt_from_sint(types::F32, builder, state),
        Operator::F32ConvertI64U => translate_fcvt_from_uint(types::F32, builder, state),
        Operator::F32DemoteF64 => {
            let val = state.pop1();
            state.push1(builder.ins().fdemote(types::F32, val));
        }
        Operator::F64ConvertI32S => translate_fcvt_from_sint(types::F64, builder, state),
        Operator::F64ConvertI32U => translate_fcvt_from_uint(types::F64, builder, state),
        Operator::F64ConvertI64S => translate_fcvt_from_sint(types::F64, builder, state),
        Operator::F64ConvertI64U => translate_fcvt_from_uint(types::F64, builder, state),
        Operator::F64PromoteF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fpromote(types::F64, val));
        }
        Operator::I32ReinterpretF32 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(types::I32, MemFlagsData::new(), val));
        }
        Operator::I64ReinterpretF64 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(types::I64, MemFlagsData::new(), val));
        }
        Operator::F32ReinterpretI32 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(types::F32, MemFlagsData::new(), val));
        }
        Operator::F64ReinterpretI64 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(types::F64, MemFlagsData::new(), val));
        }

        /************************************ Sign extension ****************************************/
        Operator::I32Extend8S => {
            let val = state.pop1();
            let val = builder.ins().ireduce(types::I8, val);
            state.push1(builder.ins().sextend(types::I32, val));
        }
        Operator::I32Extend16S => {
            let val = state.pop1();
            let val = builder.ins().ireduce(types::I16, val);
            state.push1(builder.ins().sextend(types::I32, val));
        }
        Operator::I64Extend8S => {
            let val = state.pop1();
            let val = builder.ins().ireduce(types::I8, val);
            state.push1(builder.ins().sextend(types::I64, val));
        }
        Operator::I64Extend16S => {
            let val = state.pop1();
            let val = builder.ins().ireduce(types::I16, val);
            state.push1(builder.ins().sextend(types::I64, val));
        }
        Operator::I64Extend32S => {
            let val = state.pop1();
            let val = builder.ins().ireduce(types::I32, val);
            state.push1(builder.ins().sextend(types::I64, val));
        }

        /************************************ Saturating float-to-int ****************************************/
        Operator::I32TruncSatF32S => translate_fcvt_to_sint_sat(types::I32, builder, state),
        Operator::I32TruncSatF32U => translate_fcvt_to_uint_sat(types::I32, builder, state),
        Operator::I32TruncSatF64S => translate_fcvt_to_sint_sat(types::I32, builder, state),
        Operator::I32TruncSatF64U => translate_fcvt_to_uint_sat(types::I32, builder, state),
        Operator::I64TruncSatF32S => translate_fcvt_to_sint_sat(types::I64, builder, state),
        Operator::I64TruncSatF32U => translate_fcvt_to_uint_sat(types::I64, builder, state),
        Operator::I64TruncSatF64S => translate_fcvt_to_sint_sat(types::I64, builder, state),
        Operator::I64TruncSatF64U => translate_fcvt_to_uint_sat(types::I64, builder, state),

        /************************************ Atomics ****************************************/
        Operator::MemoryAtomicNotify { memarg } => {
            let (addr, count) = state.pop2();
            let val = environ.translate_atomic_notify(
                builder.cursor(),
                MemoryIndex::from_u32(memarg.memory),
                addr,
                memarg.offset,
                count,
            )?;
            state.push1(val);
        }
        Operator::MemoryAtomicWait32 { memarg } => {
            let (addr, expected, timeout) = state.pop3();
            let val = environ.translate_atomic_wait(
                builder.cursor(),
                MemoryIndex::from_u32(memarg.memory),
                addr,
                memarg.offset,
                expected,
                timeout,
            )?;
            state.push1(val);
        }
        Operator::MemoryAtomicWait64 { memarg } => {
            let (addr, expected, timeout) = state.pop3();
            let val = environ.translate_atomic_wait(
                builder.cursor(),
                MemoryIndex::from_u32(memarg.memory),
                addr,
                memarg.offset,
                expected,
                timeout,
            )?;
            state.push1(val);
        }
        Operator::AtomicFence => {
            builder.ins().fence();
        }
        Operator::I32AtomicLoad { memarg } => {
            translate_atomic_load(types::I32, types::I32, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicLoad { memarg } => {
            translate_atomic_load(types::I64, types::I64, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicLoad8U { memarg } => {
            translate_atomic_load(types::I32, types::I8, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicLoad16U { memarg } => {
            translate_atomic_load(types::I32, types::I16, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicLoad8U { memarg } => {
            translate_atomic_load(types::I64, types::I8, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicLoad16U { memarg } => {
            translate_atomic_load(types::I64, types::I16, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicLoad32U { memarg } => {
            translate_atomic_load(types::I64, types::I32, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicStore { memarg } => {
            translate_atomic_store(types::I32, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicStore { memarg } => {
            translate_atomic_store(types::I64, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicStore8 { memarg } => {
            translate_atomic_store(types::I8, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicStore16 { memarg } => {
            translate_atomic_store(types::I16, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicStore8 { memarg } => {
            translate_atomic_store(types::I8, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicStore16 { memarg } => {
            translate_atomic_store(types::I16, memarg, builder, state, environ)?;
        }
        Operator::I64AtomicStore32 { memarg } => {
            translate_atomic_store(types::I32, memarg, builder, state, environ)?;
        }
        Operator::I32AtomicRmwAdd { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwAdd { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8AddU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16AddU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8AddU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16AddU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32AddU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::Add,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwSub { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwSub { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8SubU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16SubU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8SubU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16SubU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32SubU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::Sub,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwAnd { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwAnd { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8AndU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16AndU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8AndU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16AndU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32AndU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::And,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwOr { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwOr { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8OrU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16OrU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8OrU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16OrU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32OrU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::Or,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwXor { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwXor { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8XorU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16XorU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8XorU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16XorU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32XorU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::Xor,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwXchg { memarg } => translate_atomic_rmw(
            types::I32,
            types::I32,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmwXchg { memarg } => translate_atomic_rmw(
            types::I64,
            types::I64,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw8XchgU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I8,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmw16XchgU { memarg } => translate_atomic_rmw(
            types::I32,
            types::I16,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw8XchgU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I8,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw16XchgU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I16,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I64AtomicRmw32XchgU { memarg } => translate_atomic_rmw(
            types::I64,
            types::I32,
            AtomicRmwOp::Xchg,
            memarg,
            builder,
            state,
            environ,
        )?,
        Operator::I32AtomicRmwCmpxchg { memarg } => {
            translate_atomic_cas(types::I32, types::I32, memarg, builder, state, environ)?
        }
        Operator::I64AtomicRmwCmpxchg { memarg } => {
            translate_atomic_cas(types::I64, types::I64, memarg, builder, state, environ)?
        }
        Operator::I32AtomicRmw8CmpxchgU { memarg } => {
            translate_atomic_cas(types::I32, types::I8, memarg, builder, state, environ)?
        }
        Operator::I32AtomicRmw16CmpxchgU { memarg } => {
            translate_atomic_cas(types::I32, types::I16, memarg, builder, state, environ)?
        }
        Operator::I64AtomicRmw8CmpxchgU { memarg } => {
            translate_atomic_cas(types::I64, types::I8, memarg, builder, state, environ)?
        }
        Operator::I64AtomicRmw16CmpxchgU { memarg } => {
            translate_atomic_cas(types::I64, types::I16, memarg, builder, state, environ)?
        }
        Operator::I64AtomicRmw32CmpxchgU { memarg } => {
            translate_atomic_cas(types::I64, types::I32, memarg, builder, state, environ)?
        }

        /************************************ Everything else ****************************************/
        // SIMD, GC, exception handling, shared-everything-threads, stack
        // switching, memory control and any future operators are rejected by
        // validation before translation; reach here only if the feature gate
        // and the parser disagree.
        other => {
            return Err(TranslateError::Unsupported(format!(
                "operator {other:?} is not supported"
            )));
        }
    }

    Ok(())
}

/// Translates a wasm store instruction into a bounds-checked CLIF store.
fn translate_store(
    op: &Operator<'_>,
    memarg: &MemArg,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    let val = state.pop1();
    let val_ty = builder.func.dfg.value_type(val);
    let (opcode, _) = store_opcode_and_type(op, val_ty);
    let mem_op_size = mem_op_size(opcode, val_ty);

    let index = state.pop1();
    let (addr, flags) = environ.memory_addr(
        builder,
        MemoryIndex::from_u32(memarg.memory),
        index,
        memarg.offset,
        mem_op_size,
    )?;
    let offset = Offset32::new(0);
    match opcode {
        Opcode::Store => {
            builder.ins().store(flags, val, addr, offset);
        }
        Opcode::Istore8 => {
            builder.ins().istore8(flags, val, addr, offset);
        }
        Opcode::Istore16 => {
            builder.ins().istore16(flags, val, addr, offset);
        }
        Opcode::Istore32 => {
            builder.ins().istore32(flags, val, addr, offset);
        }
        _ => unreachable!("not a store opcode"),
    }
    Ok(())
}

/// Translates a unary integer or float operator.
fn translate_unop(opcode: Opcode, builder: &mut FunctionBuilder, state: &mut FuncTranslationState) {
    let val = state.pop1();
    let res = match opcode {
        Opcode::Clz => builder.ins().clz(val),
        Opcode::Ctz => builder.ins().ctz(val),
        Opcode::Popcnt => builder.ins().popcnt(val),
        Opcode::Fabs => builder.ins().fabs(val),
        Opcode::Fneg => builder.ins().fneg(val),
        Opcode::Ceil => builder.ins().ceil(val),
        Opcode::Floor => builder.ins().floor(val),
        Opcode::Trunc => builder.ins().trunc(val),
        Opcode::Nearest => builder.ins().nearest(val),
        Opcode::Sqrt => builder.ins().sqrt(val),
        _ => unreachable!("not a unary operator"),
    };
    state.push1(res);
}

/// Handle operators in statically unreachable code: only the control-flow
/// frames need to stay balanced.
fn translate_unreachable_operator(
    op: &Operator<'_>,
    builder: &mut FunctionBuilder,
    state: &mut FuncTranslationState,
    environ: &mut FuncEnv<'_>,
) -> TranslateResult<()> {
    debug_assert!(!state.reachable);
    match *op {
        Operator::If { blockty } => {
            // Push a placeholder control stack entry. The if isn't reachable,
            // so we don't have any branches anywhere.
            state.push_if(
                ir::Block::reserved_value(),
                ElseData::NoElse {
                    branch_inst: ir::Inst::reserved_value(),
                    placeholder: ir::Block::reserved_value(),
                },
                0,
                0,
                blockty,
            );
        }
        Operator::Loop { blockty: _ } | Operator::Block { blockty: _ } => {
            state.push_block(ir::Block::reserved_value(), 0, 0);
        }
        Operator::Else => {
            let i = state.control_stack.len() - 1;
            match state.control_stack[i] {
                ControlStackFrame::If {
                    ref else_data,
                    head_is_reachable,
                    ref mut consequent_ends_reachable,
                    blocktype,
                    ..
                } => {
                    debug_assert!(consequent_ends_reachable.is_none());
                    *consequent_ends_reachable = Some(state.reachable);

                    if head_is_reachable {
                        // We have a branch from the head of the `if` to the `else`.
                        state.reachable = true;

                        let else_block = match *else_data {
                            ElseData::NoElse {
                                branch_inst,
                                placeholder,
                            } => {
                                let (params, _results) =
                                    blocktype_params_results(environ.module_info(), blocktype)?;
                                let else_block = block_with_params(builder, &params);
                                state
                                    .control_stack
                                    .last()
                                    .unwrap()
                                    .truncate_value_stack_to_else_params(&mut state.stack);

                                // We change the target of the branch instruction.
                                builder.change_jump_destination(
                                    branch_inst,
                                    placeholder,
                                    else_block,
                                );
                                builder.seal_block(else_block);
                                else_block
                            }
                            ElseData::WithElse { else_block } => {
                                state
                                    .control_stack
                                    .last()
                                    .unwrap()
                                    .truncate_value_stack_to_else_params(&mut state.stack);
                                else_block
                            }
                        };

                        builder.switch_to_block(else_block);
                    }
                }
                _ => unreachable!(),
            }
        }
        Operator::End => {
            let stack = &mut state.stack;
            let control_stack = &mut state.control_stack;
            let frame = control_stack.pop().unwrap();

            // Pop unused parameters from stack.
            frame.truncate_value_stack_to_original_size(stack);

            let reachable_anyway = match frame {
                // If it is a loop we also have to seal the body loop block
                ControlStackFrame::Loop { header, .. } => {
                    builder.seal_block(header);
                    // Loops can't have branches to the end.
                    false
                }
                // If we never set `consequent_ends_reachable` then that means
                // we are finishing the consequent now, and there was no
                // `else`. Whether the following block is reachable depends
                // only on if the head was reachable.
                ControlStackFrame::If {
                    head_is_reachable,
                    consequent_ends_reachable: None,
                    ..
                } => head_is_reachable,
                // Since we are only in this function when in unreachable
                // code, we know that the alternative just ended unreachable.
                // Whether the following block is reachable depends on if the
                // consequent ended reachable or not.
                ControlStackFrame::If {
                    head_is_reachable,
                    consequent_ends_reachable: Some(consequent_ends_reachable),
                    ..
                } => head_is_reachable && consequent_ends_reachable,
                // All other control constructs are already handled.
                _ => false,
            };

            if frame.exit_is_branched_to() || reachable_anyway {
                builder.switch_to_block(frame.following_code());
                builder.seal_block(frame.following_code());

                // And add the return values of the block, but only if the
                // next block is reachable (which corresponds to testing if
                // the stack depth is 1).
                stack.extend_from_slice(builder.block_params(frame.following_code()));
                state.reachable = true;
            }
        }
        _ => {
            // Nothing else to do: unreachable code is not translated.
        }
    }

    Ok(())
}
