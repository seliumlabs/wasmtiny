use std::sync::{Arc, Mutex};

use crate::{
    interpreter::{ControlFrame, ControlStack, FrameKind, OperandStack},
    runtime::{
        FunctionType, Instance, Module, NumType, RefType, Result, TrapCode, ValType, WasmError,
        WasmValue,
    },
};

const MAX_CALL_DEPTH: usize = 1024;
const MAX_STACK_SIZE: usize = 16384;

struct ControlSplit {
    then_body: Vec<u8>,
    else_body: Option<Vec<u8>>,
    after_end: usize,
}

struct BlockSignature {
    param_count: usize,
    result_count: usize,
}

/// WebAssembly interpreter state and execution engine.
pub struct Interpreter {
    /// The operand stack for WebAssembly values.
    pub operand_stack: OperandStack,
    /// The control flow stack (for blocks, loops, functions).
    pub control_stack: ControlStack,
    /// The WebAssembly instance being executed.
    pub instance: Option<Arc<Mutex<Instance>>>,
    /// Local variables for the current function.
    pub locals: Vec<WasmValue>,
}

impl Interpreter {
    /// Creates a new `Interpreter`.
    pub fn new() -> Self {
        Self {
            operand_stack: OperandStack::new(MAX_STACK_SIZE),
            control_stack: ControlStack::new(),
            instance: None,
            locals: Vec::new(),
        }
    }

    /// Returns this value configured with instance.
    pub fn with_instance(instance: Arc<Mutex<Instance>>) -> Self {
        Self {
            operand_stack: OperandStack::new(MAX_STACK_SIZE),
            control_stack: ControlStack::new(),
            instance: Some(instance),
            locals: Vec::new(),
        }
    }

    /// Executes the requested function.
    pub fn execute(&mut self, module: &Module, func_idx: u32) -> Result<Vec<WasmValue>> {
        self.execute_function(module, func_idx, &[])
    }

    /// Executes function.
    pub fn execute_function(
        &mut self,
        module: &Module,
        func_idx: u32,
        args: &[WasmValue],
    ) -> Result<Vec<WasmValue>> {
        self.control_stack.clear();
        self.operand_stack.clear();
        self.locals.clear();

        let mut frame = self.build_frame(module, func_idx, args)?;
        frame.height = self.operand_stack.len();
        self.locals = frame.locals.clone();
        self.control_stack.push(frame);

        self.run(module)
    }

    fn run(&mut self, module: &Module) -> Result<Vec<WasmValue>> {
        loop {
            let should_finish = match self.control_stack.last() {
                Some(frame) => frame.position >= frame.code.len(),
                None => return Ok(Vec::new()),
            };

            if should_finish {
                if let Some(results) = self.finish_frame()? {
                    return Ok(results);
                }
                continue;
            }

            let opcode = self.read_u8_immediate()?;
            match opcode {
                0x0B => {
                    if let Some(results) = self.finish_frame()? {
                        return Ok(results);
                    }
                }
                0x0F => {
                    if let Some(results) = self.return_from_function()? {
                        return Ok(results);
                    }
                }
                _ => self.execute_opcode(module, opcode)?,
            }
        }
    }

    fn execute_opcode(&mut self, module: &Module, opcode: u8) -> Result<()> {
        match opcode {
            0x00 => Err(WasmError::Trap(TrapCode::Unreachable)),
            0x01 => Ok(()),
            0x02 => self.enter_block(module, FrameKind::Block),
            0x03 => self.enter_block(module, FrameKind::Loop),
            0x04 => self.enter_if(module),
            0x0C => {
                let depth = self.read_var_u32_immediate()?;
                self.branch(depth).map(|_| ())
            }
            0x0D => {
                let depth = self.read_var_u32_immediate()?;
                let condition = self.operand_stack.pop_i32()?;
                if condition != 0 {
                    self.branch(depth).map(|_| ())
                } else {
                    Ok(())
                }
            }
            0x0E => {
                let count = self.read_var_u32_immediate()? as usize;
                let mut labels = Vec::with_capacity(count);
                for _ in 0..count {
                    labels.push(self.read_var_u32_immediate()?);
                }
                let default = self.read_var_u32_immediate()?;
                let selector = self.operand_stack.pop_i32()? as usize;
                let depth = labels.get(selector).copied().unwrap_or(default);
                self.branch(depth).map(|_| ())
            }
            0x10 => {
                let func_idx = self.read_var_u32_immediate()?;
                self.call_function(module, func_idx)
            }
            0x11 => {
                let type_idx = self.read_var_u32_immediate()?;
                let table_idx = self.read_var_u32_immediate()?;
                self.call_indirect(module, type_idx, table_idx)
            }
            0x1A => {
                self.operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
                Ok(())
            }
            0x1B => self.select_value(),
            0x1C => {
                let count = self.read_var_u32_immediate()?;
                if count != 1 {
                    return Err(WasmError::Runtime(format!(
                        "unsupported typed select arity {}",
                        count
                    )));
                }
                let _ = self.read_value_type_immediate(module)?;
                self.select_value()
            }
            0x20 => {
                let idx = self.read_var_u32_immediate()? as usize;
                let value = self
                    .current_frame()?
                    .locals
                    .get(idx)
                    .copied()
                    .ok_or_else(|| WasmError::Runtime(format!("local {} out of bounds", idx)))?;
                self.operand_stack.push(value)
            }
            0x21 => {
                let idx = self.read_var_u32_immediate()? as usize;
                let value = self
                    .operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".into()))?;
                let frame = self.current_frame_mut()?;
                let local = frame
                    .locals
                    .get_mut(idx)
                    .ok_or_else(|| WasmError::Runtime(format!("local {} out of bounds", idx)))?;
                *local = value;
                self.locals = frame.locals.clone();
                Ok(())
            }
            0x22 => {
                let idx = self.read_var_u32_immediate()? as usize;
                let value = self
                    .operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".into()))?;
                let frame = self.current_frame_mut()?;
                let local = frame
                    .locals
                    .get_mut(idx)
                    .ok_or_else(|| WasmError::Runtime(format!("local {} out of bounds", idx)))?;
                *local = value;
                self.locals = frame.locals.clone();
                self.operand_stack.push(value)
            }
            0x23 => {
                let idx = self.read_var_u32_immediate()?;
                let instance = self.instance_ref()?;
                let instance = instance.lock().map_err(poisoned_lock)?;
                let global = instance
                    .global(idx)
                    .ok_or_else(|| WasmError::Runtime(format!("global {} out of bounds", idx)))?;
                let value = global.lock().map_err(poisoned_lock)?.get();
                drop(instance);
                self.operand_stack.push(value)
            }
            0x24 => {
                let idx = self.read_var_u32_immediate()?;
                let value = self
                    .operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".into()))?;
                let instance = self.instance_ref()?;
                let mut instance = instance.lock().map_err(poisoned_lock)?;
                let global = instance
                    .global_mut(idx)
                    .ok_or_else(|| WasmError::Runtime(format!("global {} out of bounds", idx)))?;
                global.lock().map_err(poisoned_lock)?.set(value)
            }
            0x25 => {
                let table_idx = self.read_var_u32_immediate()?;
                let elem_idx = self.operand_stack.pop_i32()? as u32;
                let instance = self.instance_ref()?;
                let instance = instance.lock().map_err(poisoned_lock)?;
                let table = instance.table(table_idx).ok_or_else(|| {
                    WasmError::Runtime(format!("table {} out of bounds", table_idx))
                })?;
                let value = table
                    .lock()
                    .map_err(poisoned_lock)?
                    .get(elem_idx)
                    .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
                drop(instance);
                self.operand_stack.push(value)
            }
            0x26 => {
                let table_idx = self.read_var_u32_immediate()?;
                let value = self
                    .operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".into()))?;
                let elem_idx = self.operand_stack.pop_i32()? as u32;

                let instance = self.instance_ref()?;
                let mut instance = instance.lock().map_err(poisoned_lock)?;
                let table = instance.table_mut(table_idx).ok_or_else(|| {
                    WasmError::Runtime(format!("table {} out of bounds", table_idx))
                })?;
                table.lock().map_err(poisoned_lock)?.set(elem_idx, value)
            }
            0x28 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I32(self.read_memory_i32(address)?))
            }
            0x29 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_i64(address)?))
            }
            0x2A => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::F32(self.read_memory_f32(address)?))
            }
            0x2B => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::F64(self.read_memory_f64(address)?))
            }
            0x2C => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I32(self.read_memory_u8(address)? as i8 as i32))
            }
            0x2D => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I32(self.read_memory_u8(address)? as i32))
            }
            0x2E => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I32(self.read_memory_u16(address)? as i16 as i32))
            }
            0x2F => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I32(self.read_memory_u16(address)? as i32))
            }
            0x30 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u8(address)? as i8 as i64))
            }
            0x31 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u8(address)? as i64))
            }
            0x32 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u16(address)? as i16 as i64))
            }
            0x33 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u16(address)? as i64))
            }
            0x34 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u32(address)? as i32 as i64))
            }
            0x35 => {
                let offset = self.read_memarg()?;
                let address = self.effective_address(offset)?;
                self.operand_stack
                    .push(WasmValue::I64(self.read_memory_u32(address)? as i64))
            }
            0x36 => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i32()?;
                let address = self.effective_address(offset)?;
                self.write_memory_i32(address, value)
            }
            0x37 => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i64()?;
                let address = self.effective_address(offset)?;
                self.write_memory_i64(address, value)
            }
            0x38 => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_f32()?;
                let address = self.effective_address(offset)?;
                self.write_memory_f32(address, value)
            }
            0x39 => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_f64()?;
                let address = self.effective_address(offset)?;
                self.write_memory_f64(address, value)
            }
            0x3A => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i32()? as u8;
                let address = self.effective_address(offset)?;
                self.write_memory_u8(address, value)
            }
            0x3B => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i32()? as u16;
                let address = self.effective_address(offset)?;
                self.write_memory_u16(address, value)
            }
            0x3C => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i64()? as u8;
                let address = self.effective_address(offset)?;
                self.write_memory_u8(address, value)
            }
            0x3D => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i64()? as u16;
                let address = self.effective_address(offset)?;
                self.write_memory_u16(address, value)
            }
            0x3E => {
                let offset = self.read_memarg()?;
                let value = self.operand_stack.pop_i64()? as u32;
                let address = self.effective_address(offset)?;
                self.write_memory_u32(address, value)
            }
            0x3F => {
                self.expect_zero_immediate("memory.size")?;
                let instance = self.instance_ref()?;
                let instance = instance.lock().map_err(poisoned_lock)?;
                let memory = instance
                    .memory(0)
                    .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?;
                let size = memory.lock().map_err(poisoned_lock)?.size() as i32;
                drop(instance);
                self.operand_stack.push(WasmValue::I32(size))
            }
            0x40 => {
                self.expect_zero_immediate("memory.grow")?;
                let pages = self.operand_stack.pop_i32()?;
                let instance = self.instance_ref()?;
                let mut instance = instance.lock().map_err(poisoned_lock)?;
                let result = WasmValue::I32(instance.memory_grow_wasm(0, pages)?);
                drop(instance);
                self.operand_stack.push(result)
            }
            0x41 => {
                let value = self.read_var_i32_immediate()?;
                self.operand_stack.push(WasmValue::I32(value))
            }
            0x42 => {
                let value = self.read_var_i64_immediate()?;
                self.operand_stack.push(WasmValue::I64(value))
            }
            0x43 => {
                let value = self.read_fixed_u32_immediate()?;
                self.operand_stack
                    .push(WasmValue::F32(f32::from_bits(value)))
            }
            0x44 => {
                let value = self.read_fixed_u64_immediate()?;
                self.operand_stack
                    .push(WasmValue::F64(f64::from_bits(value)))
            }
            0x45 => {
                let value = self.operand_stack.pop_i32()? == 0;
                self.push_bool(value)
            }
            0x46 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a == b)
            }
            0x47 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a != b)
            }
            0x48 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a < b)
            }
            0x49 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                self.push_bool(a < b)
            }
            0x4A => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a > b)
            }
            0x4B => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                self.push_bool(a > b)
            }
            0x4C => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a <= b)
            }
            0x4D => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                self.push_bool(a <= b)
            }
            0x4E => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.push_bool(a >= b)
            }
            0x4F => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                self.push_bool(a >= b)
            }
            0x50 => {
                let value = self.operand_stack.pop_i64()?;
                self.push_bool(value == 0)
            }
            0x51 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a == b)
            }
            0x52 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a != b)
            }
            0x53 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a < b)
            }
            0x54 => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                self.push_bool(a < b)
            }
            0x55 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a > b)
            }
            0x56 => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                self.push_bool(a > b)
            }
            0x57 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a <= b)
            }
            0x58 => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                self.push_bool(a <= b)
            }
            0x59 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.push_bool(a >= b)
            }
            0x5A => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                self.push_bool(a >= b)
            }
            0x5B => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a == b)
            }
            0x5C => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a != b)
            }
            0x5D => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a < b)
            }
            0x5E => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a > b)
            }
            0x5F => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a <= b)
            }
            0x60 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.push_bool(a >= b)
            }
            0x61 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a == b)
            }
            0x62 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a != b)
            }
            0x63 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a < b)
            }
            0x64 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a > b)
            }
            0x65 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a <= b)
            }
            0x66 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.push_bool(a >= b)
            }
            0x67 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack
                    .push(WasmValue::I32(value.leading_zeros() as i32))
            }
            0x68 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack
                    .push(WasmValue::I32(value.trailing_zeros() as i32))
            }
            0x69 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack
                    .push(WasmValue::I32(value.count_ones() as i32))
            }
            0x6A => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.wrapping_add(b)))
            }
            0x6B => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.wrapping_sub(b)))
            }
            0x6C => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.wrapping_mul(b)))
            }
            0x6D => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                if a == i32::MIN && b == -1 {
                    return Err(WasmError::Trap(TrapCode::IntegerOverflow));
                }
                self.operand_stack.push(WasmValue::I32(a / b))
            }
            0x6E => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                self.operand_stack.push(WasmValue::I32((a / b) as i32))
            }
            0x6F => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                if a == i32::MIN && b == -1 {
                    self.operand_stack.push(WasmValue::I32(0))
                } else {
                    self.operand_stack.push(WasmValue::I32(a % b))
                }
            }
            0x70 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                self.operand_stack.push(WasmValue::I32((a % b) as i32))
            }
            0x71 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a & b))
            }
            0x72 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a | b))
            }
            0x73 => {
                let b = self.operand_stack.pop_i32()?;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a ^ b))
            }
            0x74 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.wrapping_shl(b)))
            }
            0x75 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.wrapping_shr(b)))
            }
            0x76 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()? as u32;
                self.operand_stack
                    .push(WasmValue::I32(a.wrapping_shr(b) as i32))
            }
            0x77 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.rotate_left(b)))
            }
            0x78 => {
                let b = self.operand_stack.pop_i32()? as u32;
                let a = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(a.rotate_right(b)))
            }
            0x79 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack
                    .push(WasmValue::I64(value.leading_zeros() as i64))
            }
            0x7A => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack
                    .push(WasmValue::I64(value.trailing_zeros() as i64))
            }
            0x7B => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack
                    .push(WasmValue::I64(value.count_ones() as i64))
            }
            0x7C => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.wrapping_add(b)))
            }
            0x7D => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.wrapping_sub(b)))
            }
            0x7E => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.wrapping_mul(b)))
            }
            0x7F => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                if a == i64::MIN && b == -1 {
                    return Err(WasmError::Trap(TrapCode::IntegerOverflow));
                }
                self.operand_stack.push(WasmValue::I64(a / b))
            }
            0x80 => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                self.operand_stack.push(WasmValue::I64((a / b) as i64))
            }
            0x81 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                if a == i64::MIN && b == -1 {
                    self.operand_stack.push(WasmValue::I64(0))
                } else {
                    self.operand_stack.push(WasmValue::I64(a % b))
                }
            }
            0x82 => {
                let b = self.operand_stack.pop_i64()? as u64;
                let a = self.operand_stack.pop_i64()? as u64;
                if b == 0 {
                    return Err(WasmError::Trap(TrapCode::IntegerDivisionByZero));
                }
                self.operand_stack.push(WasmValue::I64((a % b) as i64))
            }
            0x83 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a & b))
            }
            0x84 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a | b))
            }
            0x85 => {
                let b = self.operand_stack.pop_i64()?;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a ^ b))
            }
            // i64 shifts/rotates take the shift count as an i64 operand
            // (per the WASM spec); Rust's wrapping_* ops apply the mod-64 mask.
            0x86 => {
                let b = self.operand_stack.pop_i64()? as u32;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.wrapping_shl(b)))
            }
            0x87 => {
                let b = self.operand_stack.pop_i64()? as u32;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.wrapping_shr(b)))
            }
            0x88 => {
                let b = self.operand_stack.pop_i64()? as u32;
                let a = self.operand_stack.pop_i64()? as u64;
                self.operand_stack
                    .push(WasmValue::I64(a.wrapping_shr(b) as i64))
            }
            0x89 => {
                let b = self.operand_stack.pop_i64()? as u32;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.rotate_left(b)))
            }
            0x8A => {
                let b = self.operand_stack.pop_i64()? as u32;
                let a = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(a.rotate_right(b)))
            }
            0x8B => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(value.abs()))
            }
            0x8C => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(-value))
            }
            0x8D => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(value.ceil()))
            }
            0x8E => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(value.floor()))
            }
            0x8F => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(value.trunc()))
            }
            0x90 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::F32(value.round_ties_even()))
            }
            0x91 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(value.sqrt()))
            }
            0x92 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(a + b))
            }
            0x93 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(a - b))
            }
            0x94 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(a * b))
            }
            0x95 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(a / b))
            }
            0x96 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::F32(Self::wasm_f32_min(a, b)))
            }
            0x97 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::F32(Self::wasm_f32_max(a, b)))
            }
            0x98 => {
                let b = self.operand_stack.pop_f32()?;
                let a = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F32(a.copysign(b)))
            }
            0x99 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(value.abs()))
            }
            0x9A => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(-value))
            }
            0x9B => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(value.ceil()))
            }
            0x9C => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(value.floor()))
            }
            0x9D => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(value.trunc()))
            }
            0x9E => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::F64(value.round_ties_even()))
            }
            0x9F => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(value.sqrt()))
            }
            0xA0 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(a + b))
            }
            0xA1 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(a - b))
            }
            0xA2 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(a * b))
            }
            0xA3 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(a / b))
            }
            0xA4 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::F64(Self::wasm_f64_min(a, b)))
            }
            0xA5 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::F64(Self::wasm_f64_max(a, b)))
            }
            0xA6 => {
                let b = self.operand_stack.pop_f64()?;
                let a = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F64(a.copysign(b)))
            }
            0xA7 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I32(value as i32))
            }
            0xA8 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_f32_to_i32_s(value)?))
            }
            0xA9 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_f32_to_i32_u(value)? as i32))
            }
            0xAA => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_f64_to_i32_s(value)?))
            }
            0xAB => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_f64_to_i32_u(value)? as i32))
            }
            0xAC => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I64(value as i64))
            }
            0xAD => {
                let value = self.operand_stack.pop_i32()? as u32;
                self.operand_stack.push(WasmValue::I64(value as i64))
            }
            0xAE => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_f32_to_i64_s(value)?))
            }
            0xAF => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_f32_to_i64_u(value)? as i64))
            }
            0xB0 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_f64_to_i64_s(value)?))
            }
            0xB1 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_f64_to_i64_u(value)? as i64))
            }
            0xB2 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::F32(value as f32))
            }
            0xB3 => {
                let value = self.operand_stack.pop_i32()? as u32;
                self.operand_stack.push(WasmValue::F32(value as f32))
            }
            0xB4 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::F32(value as f32))
            }
            0xB5 => {
                let value = self.operand_stack.pop_i64()? as u64;
                self.operand_stack.push(WasmValue::F32(value as f32))
            }
            0xB6 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::F32(value as f32))
            }
            0xB7 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::F64(value as f64))
            }
            0xB8 => {
                let value = self.operand_stack.pop_i32()? as u32;
                self.operand_stack.push(WasmValue::F64(value as f64))
            }
            0xB9 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::F64(value as f64))
            }
            0xBA => {
                let value = self.operand_stack.pop_i64()? as u64;
                self.operand_stack.push(WasmValue::F64(value as f64))
            }
            0xBB => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::F64(value as f64))
            }
            0xBC => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack.push(WasmValue::I32(i32::from_ne_bytes(
                    value.to_bits().to_ne_bytes(),
                )))
            }
            0xBD => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack.push(WasmValue::I64(i64::from_ne_bytes(
                    value.to_bits().to_ne_bytes(),
                )))
            }
            0xBE => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack
                    .push(WasmValue::F32(f32::from_bits(value as u32)))
            }
            0xBF => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack
                    .push(WasmValue::F64(f64::from_bits(value as u64)))
            }
            0xC0 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(value as i8 as i32))
            }
            0xC1 => {
                let value = self.operand_stack.pop_i32()?;
                self.operand_stack.push(WasmValue::I32(value as i16 as i32))
            }
            0xC2 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(value as i8 as i64))
            }
            0xC3 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(value as i16 as i64))
            }
            0xC4 => {
                let value = self.operand_stack.pop_i64()?;
                self.operand_stack.push(WasmValue::I64(value as i32 as i64))
            }
            0xFC => {
                let subopcode = self.read_var_u32_immediate()?;
                match subopcode {
                    0..=7 => self.execute_numeric_extended_opcode(subopcode),
                    8..=11 => self.execute_memory_extended_opcode(subopcode),
                    12 | 13 => self.execute_table_extended_opcode(subopcode),
                    _ => Err(WasmError::Runtime(format!(
                        "unsupported numeric extended opcode: {:02x}",
                        subopcode
                    ))),
                }
            }
            0xD0 => {
                let ref_type = self.read_u8_immediate()?;
                match ref_type {
                    0x70 => self
                        .operand_stack
                        .push(WasmValue::NullRef(RefType::FuncRef)),
                    0x6F => self
                        .operand_stack
                        .push(WasmValue::NullRef(RefType::ExternRef)),
                    byte if byte < 0x40 => self
                        .operand_stack
                        .push(WasmValue::NullRef(RefType::FuncRef)),
                    _ => Err(WasmError::Runtime(format!(
                        "invalid ref.null type: {:02x}",
                        ref_type
                    ))),
                }
            }
            0xD1 => {
                let value = self
                    .operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
                self.push_bool(matches!(value, WasmValue::NullRef(_)))
            }
            0xD2 => {
                let func_idx = self.read_var_u32_immediate()?;
                let instance = self.instance_ref()?;
                let handle = instance
                    .lock()
                    .map_err(poisoned_lock)?
                    .func_ref_handle(func_idx)?;
                self.operand_stack.push(WasmValue::native_func_ref(handle))
            }
            0xFE => {
                let subopcode = self.read_var_u32_immediate()?;
                self.execute_atomic_opcode(module, subopcode as u8)
            }
            _ => Err(WasmError::Runtime(format!(
                "unsupported opcode: {:02x}",
                opcode
            ))),
        }
    }

    fn execute_atomic_opcode(&mut self, _module: &Module, subopcode: u8) -> Result<()> {
        use crate::runtime::{AtomicKind, atomic_lookup};

        let op = atomic_lookup(subopcode).ok_or_else(|| {
            WasmError::Runtime(format!(
                "unsupported atomic subopcode: 0xFE {:02x}",
                subopcode
            ))
        })?;

        let offset = match op.kind {
            AtomicKind::Fence => {
                let reserved = self.read_u8_immediate()?;
                if reserved != 0 {
                    return Err(WasmError::Runtime(format!(
                        "atomic.fence reserved byte must be 0x00, got {:02x}",
                        reserved
                    )));
                }
                return Ok(());
            }
            _ => {
                let _align = self.read_var_u32_immediate()?;
                self.read_var_u32_immediate()?
            }
        };

        match op.kind {
            AtomicKind::Fence => Ok(()),

            AtomicKind::Notify => {
                let count = self.operand_stack.pop_i32()? as u32;
                let addr = self.effective_address_atomic(offset, op.access_width)?;

                let instance = self.instance_ref()?;
                let instance = instance.lock().map_err(poisoned_lock)?;
                let memory = instance
                    .memory(0)
                    .ok_or_else(|| WasmError::Runtime("no memory for atomic.notify".to_string()))?;
                let notified = memory.lock().map_err(poisoned_lock)?.notify(addr, count)?;
                drop(instance);
                self.operand_stack.push(WasmValue::I32(notified as i32))
            }

            AtomicKind::Wait32 => {
                let timeout = self.operand_stack.pop_i64()?;
                let expected = self.operand_stack.pop_i32()? as i64;
                let addr = self.effective_address_atomic(offset, op.access_width)?;
                let result = self.do_wait(addr, expected, timeout, false)?;
                self.operand_stack.push(WasmValue::I32(result))
            }

            AtomicKind::Wait64 => {
                let timeout = self.operand_stack.pop_i64()?;
                let expected = self.operand_stack.pop_i64()?;
                let addr = self.effective_address_atomic(offset, op.access_width)?;
                let result = self.do_wait(addr, expected, timeout, true)?;
                self.operand_stack.push(WasmValue::I32(result))
            }

            AtomicKind::Load => {
                let addr = self.effective_address_atomic(offset, op.access_width)?;
                self.atomic_load(addr, op)
            }

            AtomicKind::Store => {
                let value_popped = match op.operand_type {
                    Some(NumType::I64) => self.operand_stack.pop_i64()?,
                    _ => self.operand_stack.pop_i32()? as i64,
                };
                let addr = self.effective_address_atomic(offset, op.access_width)?;
                let width = op.access_width as usize;
                match op.operand_type {
                    Some(NumType::I64) => {
                        let bytes = (value_popped as u64).to_le_bytes();
                        self.with_memory_mut(|memory| memory.write(addr, &bytes[..width]))
                    }
                    _ => {
                        let bytes = (value_popped as u32).to_le_bytes();
                        self.with_memory_mut(|memory| memory.write(addr, &bytes[..width]))
                    }
                }
            }

            AtomicKind::Rmw => {
                if op.param_count() == 3 {
                    let replacement = self.pop_atomic_operand(op);
                    let expected = self.pop_atomic_operand(op);
                    let addr = self.effective_address_atomic(offset, op.access_width)?;
                    self.atomic_cmpxchg(addr, op, expected, replacement)
                } else {
                    let operand = self.pop_atomic_operand(op);
                    let addr = self.effective_address_atomic(offset, op.access_width)?;
                    self.atomic_rmw(addr, op, operand)
                }
            }
        }
    }

    fn effective_address_atomic(&mut self, offset: u32, access_width: u32) -> Result<u32> {
        let addr = (self.operand_stack.pop_i32()? as u32)
            .checked_add(offset)
            .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;
        if access_width > 0 && addr % access_width != 0 {
            return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
        }
        Ok(addr)
    }

    fn pop_atomic_operand(&mut self, op: &crate::runtime::atomic_op::AtomicOpMeta) -> i64 {
        match op.operand_type {
            Some(NumType::I64) => self.operand_stack.pop_i64().unwrap_or(0),
            _ => self.operand_stack.pop_i32().unwrap_or(0) as i64,
        }
    }

    fn atomic_load(
        &mut self,
        addr: u32,
        op: &crate::runtime::atomic_op::AtomicOpMeta,
    ) -> Result<()> {
        let width = op.access_width as usize;
        let mut buf = vec![0u8; width];
        self.with_memory(|memory| memory.read(addr, &mut buf))?;
        let result = match op.result_type {
            Some(NumType::I64) => {
                let val = match width {
                    1 => buf[0] as u64,
                    2 => u16::from_le_bytes([buf[0], buf[1]]) as u64,
                    4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
                    8 => u64::from_le_bytes(buf.try_into().unwrap_or([0; 8])),
                    _ => return Err(WasmError::Runtime("invalid atomic load width".to_string())),
                };
                WasmValue::I64(val as i64)
            }
            Some(NumType::I32) => {
                let val = match width {
                    1 => buf[0] as u32,
                    2 => u16::from_le_bytes([buf[0], buf[1]]) as u32,
                    4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
                    _ => return Err(WasmError::Runtime("invalid atomic load width".to_string())),
                };
                WasmValue::I32(val as i32)
            }
            _ => {
                return Err(WasmError::Runtime(
                    "atomic load has no result type".to_string(),
                ));
            }
        };
        self.operand_stack.push(result)
    }

    fn atomic_rmw(
        &mut self,
        addr: u32,
        op: &crate::runtime::atomic_op::AtomicOpMeta,
        operand: i64,
    ) -> Result<()> {
        let width = op.access_width as usize;
        let mut buf = vec![0u8; width];
        self.with_memory(|memory| memory.read(addr, &mut buf))?;
        let old = read_narrow_as_i64(&buf, width);
        let new = match op.subopcode {
            0x1E..=0x24 => old.wrapping_add(operand),
            0x25..=0x2B => old.wrapping_sub(operand),
            0x2C..=0x32 => old & operand,
            0x33..=0x39 => old | operand,
            0x3A..=0x40 => old ^ operand,
            0x41..=0x47 => operand,
            _ => {
                return Err(WasmError::Runtime(format!(
                    "unsupported RMW subopcode: 0xFE {:02x}",
                    op.subopcode
                )));
            }
        };
        let new_bytes = truncate_to_width(new as u64, width);
        self.with_memory_mut(|memory| memory.write(addr, &new_bytes))?;
        match op.result_type {
            Some(NumType::I64) => self.operand_stack.push(WasmValue::I64(old)),
            _ => self.operand_stack.push(WasmValue::I32(old as i32)),
        }
    }

    fn atomic_cmpxchg(
        &mut self,
        addr: u32,
        op: &crate::runtime::atomic_op::AtomicOpMeta,
        expected: i64,
        replacement: i64,
    ) -> Result<()> {
        let width = op.access_width as usize;
        let mut buf = vec![0u8; width];
        self.with_memory(|memory| memory.read(addr, &mut buf))?;
        let old = read_narrow_as_i64(&buf, width);
        if old == expected {
            let new_bytes = truncate_to_width(replacement as u64, width);
            self.with_memory_mut(|memory| memory.write(addr, &new_bytes))?;
        }
        match op.result_type {
            Some(NumType::I64) => self.operand_stack.push(WasmValue::I64(old)),
            _ => self.operand_stack.push(WasmValue::I32(old as i32)),
        }
    }

    fn do_wait(&mut self, addr: u32, expected: i64, timeout: i64, _is64: bool) -> Result<i32> {
        let instance = self.instance_ref()?;
        let instance = instance.lock().map_err(poisoned_lock)?;
        let memory = instance
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory for atomic.wait".to_string()))?;
        {
            let memory = memory.lock().map_err(poisoned_lock)?;
            let access_width = if _is64 { 8 } else { 4 };
            if addr as usize + access_width > memory.len_bytes() {
                return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
            }
            let actual = if _is64 {
                memory.read_i64(addr)?
            } else {
                memory.read_i32(addr)? as i64
            };
            if actual != expected {
                return Ok(1);
            }
            memory.get_waiter(addr);
        }
        drop(instance);
        let timeout_ns = if timeout < 0 {
            u64::MAX
        } else {
            (timeout as u64).saturating_mul(1)
        };
        let woken = self
            .instance_ref()?
            .lock()
            .map_err(poisoned_lock)?
            .memory(0)
            .ok_or_else(|| WasmError::Runtime("no memory for wait".to_string()))?
            .lock()
            .map_err(poisoned_lock)?
            .wait_on(addr, timeout_ns);
        if woken { Ok(0) } else { Ok(2) }
    }

    fn call_function(&mut self, module: &Module, func_idx: u32) -> Result<()> {
        if self
            .control_stack
            .frames()
            .iter()
            .filter(|frame| matches!(frame.kind, FrameKind::Function))
            .count()
            >= MAX_CALL_DEPTH
        {
            return Err(WasmError::Trap(TrapCode::StackOverflow));
        }

        let func_type = module.func_type(func_idx).ok_or_else(|| {
            WasmError::Validation(format!("function type not found for func {}", func_idx))
        })?;

        let args = self.pop_args(func_type)?;
        let import_func_count = module
            .imports
            .iter()
            .filter(|import| matches!(import.kind, crate::runtime::ImportKind::Func(_)))
            .count() as u32;

        if func_idx < import_func_count {
            let instance = self.instance_ref()?;
            let mut instance = instance.lock().map_err(poisoned_lock)?;
            let results = instance.call(func_idx, &args)?;
            drop(instance);

            for value in results {
                self.operand_stack.push(value)?;
            }
            return Ok(());
        }

        let mut frame = self.build_frame(module, func_idx, &args)?;
        frame.height = self.operand_stack.len();
        self.locals = frame.locals.clone();
        self.control_stack.push(frame);
        Ok(())
    }

    fn call_indirect(&mut self, module: &Module, type_idx: u32, table_idx: u32) -> Result<()> {
        enum IndirectCallTarget {
            Local(u32),
            Native(u32),
        }

        let expected_type = module
            .type_at(type_idx)
            .ok_or_else(|| WasmError::Validation(format!("type {} not found", type_idx)))?;
        let elem_idx = self.operand_stack.pop_i32()? as u32;

        let target = {
            let instance = self.instance_ref()?;
            let instance = instance.lock().map_err(poisoned_lock)?;
            let table = instance
                .table(table_idx)
                .ok_or_else(|| WasmError::Runtime(format!("table {} out of bounds", table_idx)))?;
            let value = table
                .lock()
                .map_err(poisoned_lock)?
                .get(elem_idx)
                .ok_or(WasmError::Trap(TrapCode::TableOutOfBounds))?;
            match value {
                WasmValue::FuncRef(func_idx) => value
                    .native_func_handle()
                    .map(IndirectCallTarget::Native)
                    .unwrap_or(IndirectCallTarget::Local(func_idx)),
                WasmValue::NullRef(RefType::FuncRef) => {
                    return Err(WasmError::Trap(TrapCode::CallIndirectNull));
                }
                _ => {
                    return Err(WasmError::Runtime(
                        "table element is not a funcref".to_string(),
                    ));
                }
            }
        };

        match target {
            IndirectCallTarget::Local(target_func_idx) => {
                let target_type = module.func_type(target_func_idx).ok_or_else(|| {
                    WasmError::Validation(format!(
                        "function type not found for func {}",
                        target_func_idx
                    ))
                })?;
                if target_type != expected_type {
                    return Err(WasmError::Trap(TrapCode::IndirectCallTypeMismatch));
                }
                self.call_function(module, target_func_idx)
            }
            IndirectCallTarget::Native(native_idx) => {
                let args = self.pop_args(expected_type)?;
                let (func, func_type, guest_target) = {
                    let instance = self.instance_ref()?;
                    let instance = instance.lock().map_err(poisoned_lock)?;
                    instance.native_func_ref_parts(native_idx)?
                };
                if let Some(target) = guest_target {
                    if target.module.id == module.id {
                        let target_type =
                            target.module.func_type(target.func_idx).ok_or_else(|| {
                                WasmError::Validation(format!(
                                    "function type not found for func {}",
                                    target.func_idx
                                ))
                            })?;
                        if target_type != expected_type {
                            return Err(WasmError::Trap(TrapCode::IndirectCallTypeMismatch));
                        }
                        for value in &args {
                            self.operand_stack.push(*value)?;
                        }
                        return self.call_function(module, target.func_idx);
                    }
                } else if &func_type != expected_type {
                    return Err(WasmError::Trap(TrapCode::IndirectCallTypeMismatch));
                }
                let results = {
                    let instance = self.instance_ref()?;
                    let instance = instance.lock().map_err(poisoned_lock)?;
                    instance.call_cloned_host_func(func, &args)?
                };
                for value in results {
                    self.operand_stack.push(value)?;
                }
                Ok(())
            }
        }
    }

    fn build_frame(
        &self,
        module: &Module,
        func_idx: u32,
        args: &[WasmValue],
    ) -> Result<ControlFrame> {
        let import_func_count = module
            .imports
            .iter()
            .filter(|import| matches!(import.kind, crate::runtime::ImportKind::Func(_)))
            .count() as u32;
        if func_idx < import_func_count {
            return Err(WasmError::Runtime(
                "imported functions must be invoked through an instance".to_string(),
            ));
        }

        let local_idx = func_idx - import_func_count;
        let func = module
            .defined_func_at(local_idx)
            .ok_or_else(|| WasmError::Runtime(format!("function {} not found", func_idx)))?;
        let func_type = module
            .type_at(func.type_idx)
            .ok_or_else(|| WasmError::Validation(format!("type {} not found", func.type_idx)))?;

        if args.len() != func_type.params.len() {
            return Err(WasmError::Runtime(format!(
                "function {} expects {} args, got {}",
                func_idx,
                func_type.params.len(),
                args.len()
            )));
        }
        for (index, (arg, expected_type)) in args.iter().zip(func_type.params.iter()).enumerate() {
            if arg.val_type() != *expected_type {
                return Err(WasmError::Runtime(format!(
                    "function {} argument {} type mismatch: expected {:?}, got {:?}",
                    func_idx,
                    index,
                    expected_type,
                    arg.val_type()
                )));
            }
        }

        let mut locals = args.to_vec();
        for local in &func.locals {
            for _ in 0..local.count {
                locals.push(default_value(local.type_));
            }
        }

        Ok(ControlFrame::new(
            FrameKind::Function,
            func_type.params.len() as u32,
            func_type.results.len() as u32,
            func_type.results.len() as u32,
            func.body.clone(),
            locals,
        ))
    }

    fn enter_block(&mut self, module: &Module, kind: FrameKind) -> Result<()> {
        let signature = self.read_block_signature(module)?;
        let split = {
            let frame = self.current_frame()?;
            self.scan_control_frame(&frame.code, frame.position, false)?
        };
        let locals = self.locals.clone();
        {
            let frame = self.current_frame_mut()?;
            frame.position = split.after_end;
        }

        let label_arity = match kind {
            FrameKind::Loop => signature.param_count,
            FrameKind::Block => signature.result_count,
            FrameKind::Function => signature.result_count,
        };
        let mut block = ControlFrame::new(
            kind,
            signature.param_count as u32,
            signature.result_count as u32,
            label_arity as u32,
            split.then_body,
            locals,
        );
        block.height = self
            .operand_stack
            .len()
            .saturating_sub(signature.param_count);
        self.control_stack.push(block);
        Ok(())
    }

    fn enter_if(&mut self, module: &Module) -> Result<()> {
        let signature = self.read_block_signature(module)?;
        let condition = self.operand_stack.pop_i32()?;
        let split = {
            let frame = self.current_frame()?;
            self.scan_control_frame(&frame.code, frame.position, true)?
        };
        let locals = self.locals.clone();
        {
            let frame = self.current_frame_mut()?;
            frame.position = split.after_end;
        }

        let selected = if condition == 0 {
            split.else_body.unwrap_or_default()
        } else {
            split.then_body
        };
        let mut block = ControlFrame::new(
            FrameKind::Block,
            signature.param_count as u32,
            signature.result_count as u32,
            signature.result_count as u32,
            selected,
            locals,
        );
        block.height = self
            .operand_stack
            .len()
            .saturating_sub(signature.param_count);
        self.control_stack.push(block);
        Ok(())
    }

    fn finish_frame(&mut self) -> Result<Option<Vec<WasmValue>>> {
        let frame = self
            .control_stack
            .pop_frame()
            .ok_or_else(|| WasmError::Runtime("no frame to finish".to_string()))?;

        let mut results = Vec::with_capacity(frame.arity);
        for _ in 0..frame.arity {
            let value = self
                .operand_stack
                .pop()
                .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
            results.push(value);
        }
        results.reverse();

        self.operand_stack.truncate(frame.height);

        if let Some(parent) = self.control_stack.last_mut() {
            if !matches!(frame.kind, FrameKind::Function) {
                parent.locals = frame.locals.clone();
            }
            for value in &results {
                self.operand_stack.push(*value)?;
            }
            self.locals = parent.locals.clone();
            Ok(None)
        } else {
            self.locals.clear();
            Ok(Some(results))
        }
    }

    fn return_from_function(&mut self) -> Result<Option<Vec<WasmValue>>> {
        // `return` exits the innermost function: collect its result values
        // from the top of the operand stack, unwind all frames up to and
        // including the function frame, and resume the caller. When no caller
        // remains, execution is complete.
        let function_frame = self
            .control_stack
            .frames()
            .iter()
            .rev()
            .find(|frame| matches!(frame.kind, FrameKind::Function))
            .cloned()
            .ok_or_else(|| WasmError::Runtime("no function frame to return from".to_string()))?;

        let mut results = Vec::with_capacity(function_frame.arity);
        for _ in 0..function_frame.arity {
            let value = self
                .operand_stack
                .pop()
                .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
            results.push(value);
        }
        results.reverse();
        self.operand_stack.truncate(function_frame.height);

        while let Some(frame) = self.control_stack.pop_frame() {
            if matches!(frame.kind, FrameKind::Function) {
                break;
            }
        }

        if let Some(parent) = self.control_stack.last_mut() {
            for value in &results {
                self.operand_stack.push(*value)?;
            }
            self.locals = parent.locals.clone();
            Ok(None)
        } else {
            self.locals.clear();
            Ok(Some(results))
        }
    }

    fn branch(&mut self, depth: u32) -> Result<Option<Vec<WasmValue>>> {
        let len = self.control_stack.len();
        let target_index = len
            .checked_sub(depth as usize + 1)
            .ok_or_else(|| WasmError::Runtime(format!("invalid branch depth {}", depth)))?;
        let target = self
            .control_stack
            .get(target_index)
            .cloned()
            .ok_or_else(|| WasmError::Runtime(format!("invalid branch depth {}", depth)))?;

        let label_arity = target.label_arity;
        let mut values = Vec::with_capacity(label_arity);
        for _ in 0..label_arity {
            values.push(
                self.operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?,
            );
        }
        values.reverse();
        self.operand_stack.truncate(target.height);
        self.control_stack.truncate(target_index + 1);

        match target.kind {
            FrameKind::Loop => {
                let loop_frame = self
                    .control_stack
                    .get_mut(target_index)
                    .ok_or_else(|| WasmError::Runtime("loop frame missing".to_string()))?;
                loop_frame.position = 0;
                loop_frame.locals = self.locals.clone();
                for value in &values {
                    self.operand_stack.push(*value)?;
                }
                self.locals = loop_frame.locals.clone();
                Ok(None)
            }
            FrameKind::Block | FrameKind::Function => {
                let target_frame = self
                    .control_stack
                    .get_mut(target_index)
                    .ok_or_else(|| WasmError::Runtime("branch target missing".to_string()))?;
                target_frame.position = target_frame.code.len();
                target_frame.locals = self.locals.clone();
                for value in &values {
                    self.operand_stack.push(*value)?;
                }
                self.locals = target_frame.locals.clone();
                Ok(None)
            }
        }
    }

    fn pop_args(&mut self, func_type: &FunctionType) -> Result<Vec<WasmValue>> {
        let mut args = Vec::with_capacity(func_type.params.len());
        for _ in 0..func_type.params.len() {
            args.push(
                self.operand_stack
                    .pop()
                    .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?,
            );
        }
        args.reverse();
        Ok(args)
    }

    fn select_value(&mut self) -> Result<()> {
        let condition = self.operand_stack.pop_i32()?;
        let rhs = self
            .operand_stack
            .pop()
            .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
        let lhs = self
            .operand_stack
            .pop()
            .ok_or_else(|| WasmError::Runtime("stack underflow".to_string()))?;
        self.operand_stack
            .push(if condition == 0 { rhs } else { lhs })
    }

    fn push_bool(&mut self, value: bool) -> Result<()> {
        self.operand_stack
            .push(WasmValue::I32(if value { 1 } else { 0 }))
    }

    fn execute_numeric_extended_opcode(&mut self, subopcode: u32) -> Result<()> {
        match subopcode {
            0 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_sat_f32_to_i32_s(value)))
            }
            1 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_sat_f32_to_i32_u(value) as i32))
            }
            2 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_sat_f64_to_i32_s(value)))
            }
            3 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I32(Self::trunc_sat_f64_to_i32_u(value) as i32))
            }
            4 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_sat_f32_to_i64_s(value)))
            }
            5 => {
                let value = self.operand_stack.pop_f32()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_sat_f32_to_i64_u(value) as i64))
            }
            6 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_sat_f64_to_i64_s(value)))
            }
            7 => {
                let value = self.operand_stack.pop_f64()?;
                self.operand_stack
                    .push(WasmValue::I64(Self::trunc_sat_f64_to_i64_u(value) as i64))
            }
            _ => Err(WasmError::Runtime(format!(
                "unsupported numeric extended opcode: {:02x}",
                subopcode
            ))),
        }
    }

    fn execute_table_extended_opcode(&mut self, subopcode: u32) -> Result<()> {
        match subopcode {
            12 => {
                let elem_idx = self.read_var_u32_immediate()?;
                let table_idx = self.read_var_u32_immediate()?;
                let len = self.operand_stack.pop_i32()? as u32;
                let src = self.operand_stack.pop_i32()? as u32;
                let dst = self.operand_stack.pop_i32()? as u32;
                let instance = self.instance_ref()?;
                instance
                    .lock()
                    .map_err(poisoned_lock)?
                    .table_init(table_idx, elem_idx, dst, src, len)
            }
            13 => {
                let elem_idx = self.read_var_u32_immediate()?;
                let instance = self.instance_ref()?;
                instance.lock().map_err(poisoned_lock)?.elem_drop(elem_idx)
            }
            _ => Err(WasmError::Runtime(format!(
                "unsupported table extended opcode: {:02x}",
                subopcode
            ))),
        }
    }

    /// Executes bulk-memory instructions (0xFC 8..=11): `memory.init`,
    /// `data.drop`, `memory.copy`, and `memory.fill`.
    fn execute_memory_extended_opcode(&mut self, subopcode: u32) -> Result<()> {
        match subopcode {
            8 => {
                let data_idx = self.read_var_u32_immediate()?;
                let memory_idx = self.read_var_u32_immediate()?;
                let len = self.operand_stack.pop_i32()? as u32;
                let src = self.operand_stack.pop_i32()? as u32;
                let dst = self.operand_stack.pop_i32()? as u32;
                self.instance_ref()?
                    .lock()
                    .map_err(poisoned_lock)?
                    .memory_init(data_idx, memory_idx, dst, src, len)
            }
            9 => {
                let data_idx = self.read_var_u32_immediate()?;
                self.instance_ref()?
                    .lock()
                    .map_err(poisoned_lock)?
                    .data_drop(data_idx)
            }
            10 => {
                let _dst_memory = self.read_var_u32_immediate()?;
                let _src_memory = self.read_var_u32_immediate()?;
                let len = self.operand_stack.pop_i32()? as u32;
                let src = self.operand_stack.pop_i32()? as u32;
                let dst = self.operand_stack.pop_i32()? as u32;

                // Bounds-check using u64 arithmetic before any copy
                let _src_end = (src as u64)
                    .checked_add(len as u64)
                    .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;
                let _dst_end = (dst as u64)
                    .checked_add(len as u64)
                    .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;

                // Check both source and destination ranges are valid
                self.with_memory(|memory| {
                    if !memory.is_valid_access(src, len as usize)? {
                        return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
                    }
                    Ok(())
                })?;
                self.with_memory(|memory| {
                    if !memory.is_valid_access(dst, len as usize)? {
                        return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
                    }
                    Ok(())
                })?;

                // Copy without staging buffer: use byte-by-byte for overlap
                // or ptr-based copy with direction-aware ordering
                if len == 0 {
                    return Ok(());
                }

                if src == dst {
                    return Ok(());
                }

                // For non-overlapping or forward-overlapping ranges, copy
                // directly without a staging buffer
                self.with_memory_mut(|memory| {
                    // Read into a small stack buffer (max 4 KiB) for overlap safety
                    // This avoids allocating guest-controlled len-sized buffer
                    let mut offset = 0u32;
                    const CHUNK: usize = 4096;
                    let mut buf = vec![0u8; CHUNK.min(len as usize)];
                    while offset < len {
                        let chunk = CHUNK.min((len - offset) as usize);
                        memory.read(src + offset, &mut buf[..chunk])?;
                        memory.write(dst + offset, &buf[..chunk])?;
                        offset += chunk as u32;
                    }
                    Ok(())
                })
            }
            11 => {
                let _memory_idx = self.read_var_u32_immediate()?;
                let len = self.operand_stack.pop_i32()? as u32;
                let value = self.operand_stack.pop_i32()? as u8;
                let dst = self.operand_stack.pop_i32()? as u32;

                // Bounds-check before filling
                let dst_end = (dst as u64)
                    .checked_add(len as u64)
                    .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))?;
                let _ = dst_end; // bounds check via is_valid_access below

                self.with_memory(|memory| {
                    if !memory.is_valid_access(dst, len as usize)? {
                        return Err(WasmError::Trap(TrapCode::MemoryOutOfBounds));
                    }
                    Ok(())
                })?;

                if len == 0 {
                    return Ok(());
                }

                // Fill without staging buffer: chunked fill
                self.with_memory_mut(|memory| {
                    let mut offset = 0u32;
                    const CHUNK: usize = 4096;
                    let chunk_buf = vec![value; CHUNK.min(len as usize)];
                    while offset < len {
                        let chunk = CHUNK.min((len - offset) as usize);
                        memory.write(dst + offset, &chunk_buf[..chunk])?;
                        offset += chunk as u32;
                    }
                    Ok(())
                })
            }
            _ => Err(WasmError::Runtime(format!(
                "unsupported memory extended opcode: {:02x}",
                subopcode
            ))),
        }
    }

    fn trunc_f32_to_i32_s(value: f32) -> Result<i32> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc() as f64;
        if !value.is_finite() || !(-2147483648.0..2147483648.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as i32)
    }

    fn trunc_f32_to_i32_u(value: f32) -> Result<u32> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc() as f64;
        if !value.is_finite() || !(0.0..4294967296.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as u32)
    }

    fn trunc_f64_to_i32_s(value: f64) -> Result<i32> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc();
        if !value.is_finite() || !(-2147483648.0..2147483648.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as i32)
    }

    fn trunc_f64_to_i32_u(value: f64) -> Result<u32> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc();
        if !value.is_finite() || !(0.0..4294967296.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as u32)
    }

    fn trunc_f32_to_i64_s(value: f32) -> Result<i64> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc() as f64;
        if !value.is_finite()
            || !(-9223372036854775808.0..9223372036854775808.0).contains(&truncated)
        {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as i64)
    }

    fn trunc_f32_to_i64_u(value: f32) -> Result<u64> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc() as f64;
        if !value.is_finite() || !(0.0..18446744073709551616.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as u64)
    }

    fn trunc_f64_to_i64_s(value: f64) -> Result<i64> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc();
        if !value.is_finite()
            || !(-9223372036854775808.0..9223372036854775808.0).contains(&truncated)
        {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as i64)
    }

    fn trunc_f64_to_i64_u(value: f64) -> Result<u64> {
        if value.is_nan() {
            return Err(WasmError::Trap(TrapCode::InvalidConversionToInt));
        }
        let truncated = value.trunc();
        if !value.is_finite() || !(0.0..18446744073709551616.0).contains(&truncated) {
            return Err(WasmError::Trap(TrapCode::IntegerOverflow));
        }
        Ok(truncated as u64)
    }

    fn trunc_sat_f32_to_i32_s(value: f32) -> i32 {
        if value.is_nan() {
            0
        } else if (value.trunc() as f64) <= -2147483648.0 {
            i32::MIN
        } else if (value.trunc() as f64) >= 2147483648.0 {
            i32::MAX
        } else {
            value.trunc() as i32
        }
    }

    fn trunc_sat_f32_to_i32_u(value: f32) -> u32 {
        if value.is_nan() || value <= 0.0 {
            0
        } else if (value.trunc() as f64) >= 4294967296.0 {
            u32::MAX
        } else {
            value.trunc() as u32
        }
    }

    fn trunc_sat_f64_to_i32_s(value: f64) -> i32 {
        if value.is_nan() {
            0
        } else if value.trunc() <= -2147483648.0 {
            i32::MIN
        } else if value.trunc() >= 2147483648.0 {
            i32::MAX
        } else {
            value.trunc() as i32
        }
    }

    fn trunc_sat_f64_to_i32_u(value: f64) -> u32 {
        if value.is_nan() || value <= 0.0 {
            0
        } else if value.trunc() >= 4294967296.0 {
            u32::MAX
        } else {
            value.trunc() as u32
        }
    }

    fn trunc_sat_f32_to_i64_s(value: f32) -> i64 {
        if value.is_nan() {
            0
        } else if (value.trunc() as f64) <= -9223372036854775808.0 {
            i64::MIN
        } else if (value.trunc() as f64) >= 9223372036854775808.0 {
            i64::MAX
        } else {
            value.trunc() as i64
        }
    }

    fn trunc_sat_f32_to_i64_u(value: f32) -> u64 {
        if value.is_nan() || value <= 0.0 {
            0
        } else if (value.trunc() as f64) >= 18446744073709551616.0 {
            u64::MAX
        } else {
            value.trunc() as u64
        }
    }

    fn trunc_sat_f64_to_i64_s(value: f64) -> i64 {
        if value.is_nan() {
            0
        } else if value.trunc() <= -9223372036854775808.0 {
            i64::MIN
        } else if value.trunc() >= 9223372036854775808.0 {
            i64::MAX
        } else {
            value.trunc() as i64
        }
    }

    fn trunc_sat_f64_to_i64_u(value: f64) -> u64 {
        if value.is_nan() || value <= 0.0 {
            0
        } else if value.trunc() >= 18446744073709551616.0 {
            u64::MAX
        } else {
            value.trunc() as u64
        }
    }

    fn wasm_f32_min(a: f32, b: f32) -> f32 {
        if a.is_nan() || b.is_nan() {
            f32::from_bits(0x7fc0_0000)
        } else if a == b {
            if a == 0.0 && (a.is_sign_negative() || b.is_sign_negative()) {
                -0.0
            } else {
                a
            }
        } else if a < b {
            a
        } else {
            b
        }
    }

    fn wasm_f32_max(a: f32, b: f32) -> f32 {
        if a.is_nan() || b.is_nan() {
            f32::from_bits(0x7fc0_0000)
        } else if a == b {
            if a == 0.0 {
                if a.is_sign_positive() || b.is_sign_positive() {
                    0.0
                } else {
                    -0.0
                }
            } else {
                a
            }
        } else if a > b {
            a
        } else {
            b
        }
    }

    fn wasm_f64_min(a: f64, b: f64) -> f64 {
        if a.is_nan() || b.is_nan() {
            f64::from_bits(0x7ff8_0000_0000_0000)
        } else if a == b {
            if a == 0.0 && (a.is_sign_negative() || b.is_sign_negative()) {
                -0.0
            } else {
                a
            }
        } else if a < b {
            a
        } else {
            b
        }
    }

    fn wasm_f64_max(a: f64, b: f64) -> f64 {
        if a.is_nan() || b.is_nan() {
            f64::from_bits(0x7ff8_0000_0000_0000)
        } else if a == b {
            if a == 0.0 {
                if a.is_sign_positive() || b.is_sign_positive() {
                    0.0
                } else {
                    -0.0
                }
            } else {
                a
            }
        } else if a > b {
            a
        } else {
            b
        }
    }

    fn read_memarg(&mut self) -> Result<u32> {
        let _align = self.read_var_u32_immediate()?;
        self.read_var_u32_immediate()
    }

    fn effective_address(&mut self, offset: u32) -> Result<u32> {
        (self.operand_stack.pop_i32()? as u32)
            .checked_add(offset)
            .ok_or(WasmError::Trap(TrapCode::MemoryOutOfBounds))
    }

    fn with_memory<T>(&self, f: impl FnOnce(&crate::memory::Memory) -> Result<T>) -> Result<T> {
        let memory = {
            let instance = self.instance_ref()?;
            let instance = instance.lock().map_err(poisoned_lock)?;
            instance
                .memory(0)
                .cloned()
                .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?
        };
        let memory = memory.lock().map_err(poisoned_lock)?;
        f(&memory)
    }

    fn with_memory_mut<T>(
        &self,
        f: impl FnOnce(&mut crate::memory::Memory) -> Result<T>,
    ) -> Result<T> {
        let memory = {
            let instance = self.instance_ref()?;
            let instance = instance.lock().map_err(poisoned_lock)?;
            instance
                .memory(0)
                .cloned()
                .ok_or_else(|| WasmError::Runtime("no memory".to_string()))?
        };
        let mut memory = memory.lock().map_err(poisoned_lock)?;
        f(&mut memory)
    }

    fn read_memory_u8(&self, address: u32) -> Result<u8> {
        self.with_memory(|memory| memory.read_u8(address))
    }

    fn read_memory_u16(&self, address: u32) -> Result<u16> {
        self.with_memory(|memory| {
            let mut bytes = [0u8; 2];
            memory.read(address, &mut bytes)?;
            Ok(u16::from_le_bytes(bytes))
        })
    }

    fn read_memory_u32(&self, address: u32) -> Result<u32> {
        self.with_memory(|memory| memory.read_u32(address))
    }

    fn read_memory_i32(&self, address: u32) -> Result<i32> {
        self.with_memory(|memory| memory.read_i32(address))
    }

    fn read_memory_i64(&self, address: u32) -> Result<i64> {
        self.with_memory(|memory| memory.read_i64(address))
    }

    fn read_memory_f32(&self, address: u32) -> Result<f32> {
        self.with_memory(|memory| memory.read_f32(address))
    }

    fn read_memory_f64(&self, address: u32) -> Result<f64> {
        self.with_memory(|memory| memory.read_f64(address))
    }

    fn write_memory_u8(&self, address: u32, value: u8) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_u8(address, value))
    }

    fn write_memory_u16(&self, address: u32, value: u16) -> Result<()> {
        self.with_memory_mut(|memory| memory.write(address, &value.to_le_bytes()))
    }

    fn write_memory_u32(&self, address: u32, value: u32) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_u32(address, value))
    }

    fn write_memory_i32(&self, address: u32, value: i32) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_i32(address, value))
    }

    fn write_memory_i64(&self, address: u32, value: i64) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_i64(address, value))
    }

    fn write_memory_f32(&self, address: u32, value: f32) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_f32(address, value))
    }

    fn write_memory_f64(&self, address: u32, value: f64) -> Result<()> {
        self.with_memory_mut(|memory| memory.write_f64(address, value))
    }

    fn expect_zero_immediate(&mut self, instruction: &str) -> Result<()> {
        let reserved = self.read_u8_immediate()?;
        if reserved != 0 {
            return Err(WasmError::Runtime(format!(
                "{} expects a zero reserved byte",
                instruction
            )));
        }
        Ok(())
    }

    fn read_block_signature(&mut self, module: &Module) -> Result<BlockSignature> {
        let marker = self.read_u8_immediate()?;
        match marker {
            0x40 => Ok(BlockSignature {
                param_count: 0,
                result_count: 0,
            }),
            0x7F | 0x7E | 0x7D | 0x7C | 0x70 | 0x6F => Ok(BlockSignature {
                param_count: 0,
                result_count: 1,
            }),
            0x63 | 0x64 => {
                let first = self.read_u8_immediate()?;
                let heap_type = self.read_signed_leb_continuation(first)?;
                if heap_type < 0 && !matches!(heap_type, -0x10 | -0x11 | -0x14 | -0x13) {
                    return Err(WasmError::Validation(format!(
                        "invalid block heap type {}",
                        heap_type
                    )));
                }
                Ok(BlockSignature {
                    param_count: 0,
                    result_count: 1,
                })
            }
            byte => {
                let type_idx = self.read_signed_leb_continuation(byte)?;
                if type_idx < 0 {
                    return Err(WasmError::Validation(format!(
                        "invalid block type index {}",
                        type_idx
                    )));
                }
                let type_ = module
                    .type_at(type_idx as u32)
                    .ok_or_else(|| WasmError::Validation(format!("type {} not found", type_idx)))?;
                Ok(BlockSignature {
                    param_count: type_.params.len(),
                    result_count: type_.results.len(),
                })
            }
        }
    }

    fn read_value_type_immediate(&mut self, module: &Module) -> Result<ValType> {
        let marker = self.read_u8_immediate()?;
        match marker {
            0x7F => Ok(ValType::Num(NumType::I32)),
            0x7E => Ok(ValType::Num(NumType::I64)),
            0x7D => Ok(ValType::Num(NumType::F32)),
            0x7C => Ok(ValType::Num(NumType::F64)),
            0x70 => Ok(ValType::Ref(RefType::FuncRef)),
            0x6F => Ok(ValType::Ref(RefType::ExternRef)),
            0x63 | 0x64 => {
                let first = self.read_u8_immediate()?;
                let heap_type = self.read_signed_leb_continuation(first)?;
                match heap_type {
                    -0x10 | -0x14 => Ok(ValType::Ref(RefType::FuncRef)),
                    -0x11 | -0x13 => Ok(ValType::Ref(RefType::ExternRef)),
                    idx if idx >= 0 && module.type_at(idx as u32).is_some() => {
                        Ok(ValType::Ref(RefType::FuncRef))
                    }
                    _ => Err(WasmError::Validation(format!(
                        "invalid value type heap type {}",
                        heap_type
                    ))),
                }
            }
            byte => Err(WasmError::Validation(format!(
                "invalid value type immediate {:02x}",
                byte
            ))),
        }
    }

    fn scan_control_frame(
        &self,
        code: &[u8],
        start: usize,
        allow_else: bool,
    ) -> Result<ControlSplit> {
        let mut cursor = start;
        let mut depth = 1usize;
        let mut else_at = None;

        while cursor < code.len() {
            let opcode = code[cursor];
            cursor += 1;
            match opcode {
                0x02..=0x04 => {
                    Self::skip_block_type(code, &mut cursor)?;
                    depth += 1;
                }
                0x05 if allow_else && depth == 1 => {
                    else_at = Some(cursor - 1);
                }
                0x0B => {
                    depth -= 1;
                    if depth == 0 {
                        let then_end = else_at.unwrap_or(cursor - 1);
                        let then_body = code[start..then_end].to_vec();
                        let else_body =
                            else_at.map(|else_pos| code[else_pos + 1..cursor - 1].to_vec());
                        return Ok(ControlSplit {
                            then_body,
                            else_body,
                            after_end: cursor,
                        });
                    }
                }
                _ => Self::skip_immediates(code, &mut cursor, opcode)?,
            }
        }

        Err(WasmError::Load(
            "unterminated structured control instruction".to_string(),
        ))
    }

    fn skip_block_type(code: &[u8], cursor: &mut usize) -> Result<()> {
        let marker = *code
            .get(*cursor)
            .ok_or_else(|| WasmError::Load("unexpected end of block type".to_string()))?;
        *cursor += 1;
        if matches!(marker, 0x63 | 0x64) {
            let first = *code
                .get(*cursor)
                .ok_or_else(|| WasmError::Load("unexpected end of heap type".to_string()))?;
            *cursor += 1;
            Self::skip_sleb_tail(code, cursor, first)?;
        } else if !matches!(marker, 0x40 | 0x7F | 0x7E | 0x7D | 0x7C | 0x70 | 0x6F) {
            Self::skip_sleb_tail(code, cursor, marker)?;
        }
        Ok(())
    }

    fn skip_immediates(code: &[u8], cursor: &mut usize, opcode: u8) -> Result<()> {
        match opcode {
            0x0C | 0x0D | 0x10 | 0x20..=0x26 | 0xD2 => Self::skip_uleb(code, cursor),
            0x0E => {
                let count = Self::read_uleb(code, cursor)?;
                for _ in 0..count {
                    Self::skip_uleb(code, cursor)?;
                }
                Self::skip_uleb(code, cursor)
            }
            0x11 => {
                Self::skip_uleb(code, cursor)?;
                Self::skip_uleb(code, cursor)
            }
            0x1C => {
                let count = Self::read_uleb(code, cursor)?;
                for _ in 0..count {
                    Self::skip_val_type(code, cursor)?;
                }
                Ok(())
            }
            0x28..=0x3E => {
                Self::skip_uleb(code, cursor)?;
                Self::skip_uleb(code, cursor)
            }
            0x3F | 0x40 => Self::skip_bytes(code, cursor, 1),
            0xD0 => Self::skip_sleb(code, cursor),
            0x41 | 0x42 => Self::skip_sleb(code, cursor),
            0x43 => Self::skip_bytes(code, cursor, 4),
            0x44 => Self::skip_bytes(code, cursor, 8),
            0xFC => {
                let subopcode = Self::read_uleb(code, cursor)?;
                match subopcode {
                    0..=7 => Ok(()),
                    8 | 10 => {
                        Self::skip_uleb(code, cursor)?;
                        Self::skip_uleb(code, cursor)
                    }
                    9 | 11 => Self::skip_uleb(code, cursor),
                    12 => {
                        Self::skip_uleb(code, cursor)?;
                        Self::skip_uleb(code, cursor)
                    }
                    13 => Self::skip_uleb(code, cursor),
                    _ => Err(WasmError::Runtime(
                        "unsupported 0xfc prefixed opcode in structured control".to_string(),
                    )),
                }
            }
            0xFE => {
                let subopcode = Self::read_uleb(code, cursor)? as u8;
                match crate::runtime::atomic_op::immediate_count(subopcode) {
                    Some(count) => {
                        for _ in 0..count {
                            Self::skip_uleb(code, cursor)?;
                        }
                        Ok(())
                    }
                    None => Err(WasmError::Runtime(format!(
                        "unsupported 0xFE prefixed opcode 0xFE{:02x} in structured control",
                        subopcode
                    ))),
                }
            }
            _ => Ok(()),
        }
    }

    fn skip_uleb(code: &[u8], cursor: &mut usize) -> Result<()> {
        let _ = Self::read_uleb(code, cursor)?;
        Ok(())
    }

    fn skip_val_type(code: &[u8], cursor: &mut usize) -> Result<()> {
        let marker = *code
            .get(*cursor)
            .ok_or_else(|| WasmError::Load("unexpected end of value type".to_string()))?;
        *cursor += 1;
        if matches!(marker, 0x63 | 0x64) {
            let first = *code
                .get(*cursor)
                .ok_or_else(|| WasmError::Load("unexpected end of heap type".to_string()))?;
            *cursor += 1;
            Self::skip_sleb_tail(code, cursor, first)?;
        }
        Ok(())
    }

    fn read_uleb(code: &[u8], cursor: &mut usize) -> Result<u32> {
        let mut result = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = *code
                .get(*cursor)
                .ok_or_else(|| WasmError::Load("unexpected end of uleb immediate".to_string()))?;
            *cursor += 1;
            result |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 35 {
                return Err(WasmError::Load("uleb128 overflow".to_string()));
            }
        }
    }

    fn skip_sleb(code: &[u8], cursor: &mut usize) -> Result<()> {
        let first = *code
            .get(*cursor)
            .ok_or_else(|| WasmError::Load("unexpected end of sleb immediate".to_string()))?;
        *cursor += 1;
        Self::skip_sleb_tail(code, cursor, first)
    }

    fn skip_sleb_tail(code: &[u8], cursor: &mut usize, mut byte: u8) -> Result<()> {
        while byte & 0x80 != 0 {
            byte = *code
                .get(*cursor)
                .ok_or_else(|| WasmError::Load("unexpected end of sleb immediate".to_string()))?;
            *cursor += 1;
        }
        Ok(())
    }

    fn skip_bytes(code: &[u8], cursor: &mut usize, len: usize) -> Result<()> {
        if code.len().saturating_sub(*cursor) < len {
            return Err(WasmError::Load("unexpected end of immediate".to_string()));
        }
        *cursor += len;
        Ok(())
    }

    fn read_signed_leb_continuation(&mut self, first: u8) -> Result<i32> {
        let mut result = (first & 0x7F) as i32;
        let mut shift = 7u32;
        let mut byte = first;

        while byte & 0x80 != 0 {
            byte = self.read_u8_immediate()?;
            result |= ((byte & 0x7F) as i32) << shift;
            shift += 7;
            if shift >= 35 {
                return Err(WasmError::Load("sleb128 overflow".to_string()));
            }
        }

        if shift < 32 && (byte & 0x40) != 0 {
            result |= !0 << shift;
        }

        Ok(result)
    }

    fn current_frame(&self) -> Result<&ControlFrame> {
        self.control_stack
            .last()
            .ok_or_else(|| WasmError::Runtime("no active frame".to_string()))
    }

    fn current_frame_mut(&mut self) -> Result<&mut ControlFrame> {
        self.control_stack
            .last_mut()
            .ok_or_else(|| WasmError::Runtime("no active frame".to_string()))
    }

    fn read_u8_immediate(&mut self) -> Result<u8> {
        let frame = self.current_frame_mut()?;
        if frame.position >= frame.code.len() {
            return Err(WasmError::Load(
                "unexpected end of function body".to_string(),
            ));
        }
        let byte = frame.code[frame.position];
        frame.position += 1;
        Ok(byte)
    }

    fn read_var_u32_immediate(&mut self) -> Result<u32> {
        let mut result = 0u32;
        let mut shift = 0u32;

        loop {
            let byte = self.read_u8_immediate()?;
            result |= ((byte & 0x7F) as u32) << shift;

            if byte & 0x80 == 0 {
                return Ok(result);
            }

            shift += 7;
            if shift >= 35 {
                return Err(WasmError::Load("uleb128 overflow".to_string()));
            }
        }
    }

    fn read_var_i32_immediate(&mut self) -> Result<i32> {
        let mut result = 0i32;
        let mut shift = 0u32;
        let mut byte;

        loop {
            byte = self.read_u8_immediate()?;
            result |= ((byte & 0x7F) as i32) << shift;
            shift += 7;

            if byte & 0x80 == 0 {
                break;
            }

            if shift >= 35 {
                return Err(WasmError::Load("sleb128 overflow".to_string()));
            }
        }

        if shift < 32 && (byte & 0x40) != 0 {
            result |= !0 << shift;
        }

        Ok(result)
    }

    fn read_var_i64_immediate(&mut self) -> Result<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        let mut byte;

        loop {
            byte = self.read_u8_immediate()?;
            result |= ((byte & 0x7F) as i64) << shift;
            shift += 7;

            if byte & 0x80 == 0 {
                break;
            }

            if shift >= 70 {
                return Err(WasmError::Load("sleb128 overflow".to_string()));
            }
        }

        if shift < 64 && (byte & 0x40) != 0 {
            result |= !0 << shift;
        }

        Ok(result)
    }

    fn read_fixed_u32_immediate(&mut self) -> Result<u32> {
        let mut bytes = [0u8; 4];
        for byte in &mut bytes {
            *byte = self.read_u8_immediate()?;
        }
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_fixed_u64_immediate(&mut self) -> Result<u64> {
        let mut bytes = [0u8; 8];
        for byte in &mut bytes {
            *byte = self.read_u8_immediate()?;
        }
        Ok(u64::from_le_bytes(bytes))
    }

    fn instance_ref(&self) -> Result<&Arc<Mutex<Instance>>> {
        self.instance
            .as_ref()
            .ok_or_else(|| WasmError::Runtime("no instance available".to_string()))
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn default_value(value_type: ValType) -> WasmValue {
    match value_type {
        ValType::Num(NumType::I32) => WasmValue::I32(0),
        ValType::Num(NumType::I64) => WasmValue::I64(0),
        ValType::Num(NumType::F32) => WasmValue::F32(0.0),
        ValType::Num(NumType::F64) => WasmValue::F64(0.0),
        ValType::Ref(RefType::FuncRef) => WasmValue::NullRef(RefType::FuncRef),
        ValType::Ref(RefType::ExternRef) => WasmValue::NullRef(RefType::ExternRef),
    }
}

fn poisoned_lock<T>(_: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>) -> WasmError {
    WasmError::Runtime("instance lock poisoned".to_string())
}

fn read_narrow_as_i64(buf: &[u8], width: usize) -> i64 {
    (match width {
        1 => buf[0] as u64,
        2 => u16::from_le_bytes([buf[0], buf[1]]) as u64,
        4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
        8 => u64::from_le_bytes(buf.try_into().unwrap_or([0; 8])),
        _ => 0,
    }) as i64
}

fn truncate_to_width(value: u64, width: usize) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    bytes[..width].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        DataKind, DataSegment, Func, FunctionType, Instance, Limits, Local, MemoryType, Module,
        TableType,
    };

    #[test]
    fn test_interpreter_creation() {
        let interp = Interpreter::new();
        assert!(interp.operand_stack.is_empty());
        assert!(interp.control_stack.is_empty());
    }

    #[test]
    fn test_i32_operations() {
        let mut interp = Interpreter::new();
        interp.operand_stack.push_unchecked(WasmValue::I32(5));
        interp.operand_stack.push_unchecked(WasmValue::I32(3));
        assert_eq!(interp.operand_stack.pop_i32().unwrap(), 3);
        assert_eq!(interp.operand_stack.pop_i32().unwrap(), 5);
    }

    #[test]
    fn test_bit_operations() {
        let mut interp = Interpreter::new();
        interp.operand_stack.push_unchecked(WasmValue::I32(0b1100));
        interp.operand_stack.push_unchecked(WasmValue::I32(0b1010));
        let b = interp.operand_stack.pop_i32().unwrap();
        let a = interp.operand_stack.pop_i32().unwrap();
        let result = a & b;
        assert_eq!(result, 0b1000);
    }

    #[test]
    fn test_if_else_executes_selected_branch() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x41, 0x00, 0x04, 0x7F, 0x41, 0x01, 0x05, 0x41, 0x02, 0x0B, 0x0B,
            ],
        });

        let mut interp = Interpreter::new();
        let results = interp.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(2)]);
    }

    #[test]
    fn test_return_unwinds_nested_blocks() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x02, 0x40, 0x41, 0x07, 0x0F, 0x0B, 0x41, 0x00, 0x0B],
        });

        let mut interp = Interpreter::new();
        let results = interp.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(7)]);
    }

    #[test]
    fn test_loop_with_br_and_br_if() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![Local {
                count: 1,
                type_: ValType::Num(NumType::I32),
            }],
            body: vec![
                0x41, 0x03, 0x21, 0x00, 0x02, 0x40, 0x03, 0x40, 0x20, 0x00, 0x45, 0x0D, 0x01, 0x20,
                0x00, 0x41, 0x01, 0x6B, 0x21, 0x00, 0x0C, 0x00, 0x0B, 0x0B, 0x20, 0x00, 0x0B,
            ],
        });

        let mut interp = Interpreter::new();
        let results = interp.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(0)]);
    }

    #[test]
    fn test_br_table_selects_target() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x02, 0x7F, 0x02, 0x7F, 0x41, 0x14, 0x41, 0x01, 0x0E, 0x02, 0x00, 0x01, 0x01, 0x41,
                0x0A, 0x0B, 0x41, 0x1E, 0x0B, 0x0B,
            ],
        });

        let mut interp = Interpreter::new();
        let results = interp.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(20)]);
    }

    #[test]
    fn test_typed_loop_branch_uses_parameter_arity() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![ValType::Num(NumType::I32)], vec![]));
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.funcs.push(Func {
            type_idx: 1,
            locals: vec![Local {
                count: 1,
                type_: ValType::Num(NumType::I32),
            }],
            body: vec![
                0x41, 0x03, 0x03, 0x00, 0x21, 0x00, 0x20, 0x00, 0x20, 0x00, 0x45, 0x0D, 0x01, 0x41,
                0x01, 0x6B, 0x0C, 0x00, 0x0B, 0x20, 0x00, 0x0B,
            ],
        });

        let mut interp = Interpreter::new();
        let results = interp.execute_function(&module, 0, &[]).unwrap();
        assert_eq!(results, vec![WasmValue::I32(0)]);
    }

    #[test]
    fn test_memory_load_and_store_opcodes() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x41, 0x00, 0x41, 0x2A, 0x36, 0x02, 0x00, 0x41, 0x00, 0x28, 0x02, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));
        let mut interp = Interpreter::with_instance(instance);
        let results = interp.execute_function(&module, 0, &[]).unwrap();

        assert_eq!(results, vec![WasmValue::I32(42)]);
    }

    #[test]
    fn test_table_set_accepts_externref_tables() {
        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module
            .tables
            .push(TableType::new(RefType::ExternRef, Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x00, 0xD0, 0x6F, 0x26, 0x00, 0x0B],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));
        let mut interp = Interpreter::with_instance(instance.clone());
        interp.execute_function(&module, 0, &[]).unwrap();

        let table = instance.lock().unwrap().table(0).cloned().unwrap();
        assert_eq!(
            table.lock().unwrap().get(0),
            Some(WasmValue::NullRef(RefType::ExternRef))
        );
    }

    #[test]
    fn test_execute_function_rejects_argument_type_mismatch() {
        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![ValType::Num(NumType::I32)], vec![]));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x0B],
        });

        let mut interp = Interpreter::new();
        let error = interp
            .execute_function(&module, 0, &[WasmValue::F64(1.0)])
            .unwrap_err();

        assert!(
            matches!(error, WasmError::Runtime(message) if message.contains("argument 0 type mismatch"))
        );
    }
    #[test]
    fn test_atomic_load_store() {
        use crate::memory::Memory;
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I32)],
        ));
        module.memories.push(MemoryType::new(Limits::Min(1)));

        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x41, 0x00, 0x41, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x36, 0x02, 0x00, 0x41, 0x00, 0x28,
                0x02, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let mut instance = Instance::new(module.clone()).unwrap();
        let memory = Arc::new(Mutex::new(
            Memory::new(MemoryType::new(Limits::Min(1))).unwrap(),
        ));
        instance.memories.push(memory.clone());
        let instance = Arc::new(Mutex::new(instance));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[WasmValue::I32(42)]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_atomic_rmw_add() {
        use std::sync::Arc;

        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.memories.push(MemoryType::new(Limits::Min(1)));

        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x00, 0x41, 0x05, 0xFE, 0x1E, 0x02, 0x00, 0x0B],
        });

        let module = Arc::new(module);
        let instance = Instance::new(module.clone()).unwrap();
        instance.memories[0]
            .lock()
            .unwrap()
            .write_i32(0, 10)
            .unwrap();
        let instance = Arc::new(Mutex::new(instance));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0], WasmValue::I32(10));
    }

    #[test]
    fn test_return_in_nested_call_resumes_caller() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![],
            vec![crate::runtime::ValType::Num(crate::runtime::NumType::I32)],
        ));
        // func 0: call func 1; i32.const 1; i32.add; end
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x10, 0x01, 0x41, 0x01, 0x6A, 0x0B],
        });
        // func 1: i32.const 41; return; end
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x29, 0x0F, 0x0B],
        });

        let module = Arc::new(module);
        let mut interp = Interpreter::new();
        let result = interp.execute_function(&module, 0, &[]);
        assert_eq!(result.unwrap(), vec![WasmValue::I32(42)]);
    }

    #[test]
    fn test_return_from_inside_block_resumes_caller() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![],
            vec![crate::runtime::ValType::Num(crate::runtime::NumType::I32)],
        ));
        // func 0: call func 1; i32.const 1; i32.add; end
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x10, 0x01, 0x41, 0x01, 0x6A, 0x0B],
        });
        // func 1: block { i32.const 41; return }; unreachable
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x02, 0x40, 0x41, 0x29, 0x0F, 0x0B, 0x00, 0x0B],
        });

        let module = Arc::new(module);
        let mut interp = Interpreter::new();
        let result = interp.execute_function(&module, 0, &[]);
        assert_eq!(result.unwrap(), vec![WasmValue::I32(42)]);
    }

    #[test]
    fn test_memory_fill() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            // i32.const 0 (dst); i32.const 5 (val); i32.const 4 (len); memory.fill 0
            body: vec![0x41, 0x00, 0x41, 0x05, 0x41, 0x04, 0xFC, 0x0B, 0x00, 0x0B],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));

        let mut interp = Interpreter::with_instance(instance.clone());
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());

        let memory = instance.lock().unwrap().memories[0].clone();
        let mut buf = [0u8; 4];
        memory.lock().unwrap().read(0, &mut buf).unwrap();
        assert_eq!(buf, [5, 5, 5, 5]);
    }

    #[test]
    fn test_memory_copy() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            // i32.const 4 (dst); i32.const 0 (src); i32.const 4 (len); memory.copy 0 0
            body: vec![
                0x41, 0x04, 0x41, 0x00, 0x41, 0x04, 0xFC, 0x0A, 0x00, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));
        let memory = instance.lock().unwrap().memories[0].clone();
        memory.lock().unwrap().write(0, &[1, 2, 3, 4]).unwrap();

        let mut interp = Interpreter::with_instance(instance.clone());
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());

        let mut buf = [0u8; 4];
        memory.lock().unwrap().read(4, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn test_memory_copy_overlapping_uses_memmove_semantics() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            // dst=1, src=0, len=4: overlapping forward copy
            body: vec![
                0x41, 0x01, 0x41, 0x00, 0x41, 0x04, 0xFC, 0x0A, 0x00, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));
        let memory = instance.lock().unwrap().memories[0].clone();
        memory.lock().unwrap().write(0, &[1, 2, 3, 4]).unwrap();

        let mut interp = Interpreter::with_instance(instance.clone());
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());

        let mut buf = [0u8; 5];
        memory.lock().unwrap().read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 1, 2, 3, 4]);
    }

    #[test]
    fn test_memory_copy_out_of_bounds_traps() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            // dst=65533, src=0, len=4 → one byte past the 64 KiB page
            body: vec![
                0x41, 0xFD, 0xFF, 0x03, 0x41, 0x00, 0x41, 0x04, 0xFC, 0x0A, 0x00, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[]);
        assert!(matches!(
            result,
            Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
        ));
    }

    #[test]
    fn test_memory_init_and_data_drop() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(vec![], vec![]));
        module.memories.push(MemoryType::new(Limits::Min(1)));
        module.data.push(DataSegment {
            kind: DataKind::Passive,
            init: vec![9, 8, 7],
        });
        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            // memory.init 0 0 (dst=0, src=0, len=3); data.drop 0;
            // memory.init 0 0 again (len=1 → traps, segment dropped)
            body: vec![
                0x41, 0x00, 0x41, 0x00, 0x41, 0x03, 0xFC, 0x08, 0x00, 0x00, 0xFC, 0x09, 0x00, 0x41,
                0x00, 0x41, 0x00, 0x41, 0x01, 0xFC, 0x08, 0x00, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Arc::new(Mutex::new(Instance::new(module.clone()).unwrap()));
        let memory = instance.lock().unwrap().memories[0].clone();

        let mut interp = Interpreter::with_instance(instance.clone());
        let result = interp.execute_function(&module, 0, &[]);

        // The second memory.init (after data.drop) must trap.
        assert!(matches!(
            result,
            Err(WasmError::Trap(TrapCode::MemoryOutOfBounds))
        ));
        // The first memory.init succeeded before the drop.
        let mut buf = [0u8; 3];
        memory.lock().unwrap().read(0, &mut buf).unwrap();
        assert_eq!(buf, [9, 8, 7]);
    }

    #[test]
    fn test_atomic_load() {
        use std::sync::Arc;

        let mut module = Module::new();
        module.types.push(FunctionType::new(
            vec![ValType::Num(NumType::I32)],
            vec![ValType::Num(NumType::I32)],
        ));
        module.memories.push(MemoryType::new(Limits::Min(1)));

        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![0x41, 0x00, 0xFE, 0x10, 0x02, 0x00, 0x0B],
        });

        let module = Arc::new(module);
        let instance = Instance::new(module.clone()).unwrap();
        instance.memories[0]
            .lock()
            .unwrap()
            .write_i32(0, 0x12345678)
            .unwrap();
        let instance = Arc::new(Mutex::new(instance));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[WasmValue::I32(0)]);
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0], WasmValue::I32(0x12345678));
    }

    #[test]
    fn test_atomic_rmw_cmpxchg_success() {
        use std::sync::Arc;

        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.memories.push(MemoryType::new(Limits::Min(1)));

        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x41, 0x00, 0x41, 0x0A, 0x41, 0x14, 0xFE, 0x48, 0x02, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Instance::new(module.clone()).unwrap();
        instance.memories[0]
            .lock()
            .unwrap()
            .write_i32(0, 10)
            .unwrap();
        let instance = Arc::new(Mutex::new(instance));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0], WasmValue::I32(10));
    }

    #[test]
    fn test_atomic_rmw_cmpxchg_fail() {
        use std::sync::Arc;

        let mut module = Module::new();
        module
            .types
            .push(FunctionType::new(vec![], vec![ValType::Num(NumType::I32)]));
        module.memories.push(MemoryType::new(Limits::Min(1)));

        module.funcs.push(Func {
            type_idx: 0,
            locals: vec![],
            body: vec![
                0x41, 0x00, 0x41, 0x0B, 0x41, 0x14, 0xFE, 0x48, 0x02, 0x00, 0x0B,
            ],
        });

        let module = Arc::new(module);
        let instance = Instance::new(module.clone()).unwrap();
        instance.memories[0]
            .lock()
            .unwrap()
            .write_i32(0, 10)
            .unwrap();
        let instance = Arc::new(Mutex::new(instance));

        let mut interp = Interpreter::with_instance(instance);
        let result = interp.execute_function(&module, 0, &[]);
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0], WasmValue::I32(10));
    }
}
