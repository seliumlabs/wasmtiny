use crate::runtime::WasmValue;

#[derive(Debug, Clone)]
/// Operand stack.
pub struct OperandStack {
    slots: Vec<WasmValue>,
    max_size: usize,
}

#[derive(Debug, Clone, Copy)]
/// Kind of control frame active in the interpreter.
pub enum FrameKind {
    /// A function frame.
    Function,
    /// A structured block frame.
    Block,
    /// A loop frame.
    Loop,
}

#[derive(Debug, Clone)]
/// A frame stored on the interpreter control stack.
pub struct ControlFrame {
    /// Kind of control construct represented by this frame.
    pub kind: FrameKind,
    /// Current instruction position within the frame code.
    pub position: usize,
    /// Encoded instruction bytes for this frame.
    pub code: Vec<u8>,
    /// Number of values produced when the frame completes.
    pub arity: usize,
    /// Number of values expected by branches to this frame.
    pub label_arity: usize,
    /// Number of local slots associated with the frame.
    pub local_count: usize,
    /// Operand-stack height when the frame was entered.
    pub height: usize,
    /// Captured local values for the frame.
    pub locals: Vec<WasmValue>,
}

#[derive(Debug, Clone)]
/// Control stack.
pub struct ControlStack {
    frames: Vec<ControlFrame>,
}

impl OperandStack {
    /// Creates a new `OperandStack`.
    pub fn new(max_size: usize) -> Self {
        Self {
            slots: Vec::with_capacity(max_size),
            max_size,
        }
    }

    /// Creates a stack from pre-existing values.
    pub fn from_vec(slots: Vec<WasmValue>, max_size: usize) -> Self {
        Self { slots, max_size }
    }

    /// Returns the maximum size.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Pushes a value onto the stack.
    pub fn push(&mut self, value: WasmValue) -> crate::runtime::Result<()> {
        if self.slots.len() >= self.max_size {
            return Err(crate::runtime::WasmError::Runtime("stack overflow".into()));
        }
        self.slots.push(value);
        Ok(())
    }

    /// Pushes a value without checking capacity limits.
    pub fn push_unchecked(&mut self, value: WasmValue) {
        debug_assert!(
            self.slots.len() < self.max_size,
            "stack overflow (validated module should not reach this)"
        );
        self.slots.push(value);
    }

    /// Pops and returns the top value, if present.
    pub fn pop(&mut self) -> Option<WasmValue> {
        self.slots.pop()
    }

    /// Pops and returns an `i32` value.
    pub fn pop_i32(&mut self) -> crate::runtime::Result<i32> {
        match self.pop() {
            Some(WasmValue::I32(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Pops and returns an `i64` value.
    pub fn pop_i64(&mut self) -> crate::runtime::Result<i64> {
        match self.pop() {
            Some(WasmValue::I64(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Pops and returns an `f32` value.
    pub fn pop_f32(&mut self) -> crate::runtime::Result<f32> {
        match self.pop() {
            Some(WasmValue::F32(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Pops and returns an `f64` value.
    pub fn pop_f64(&mut self) -> crate::runtime::Result<f64> {
        match self.pop() {
            Some(WasmValue::F64(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Returns the length.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` if this value is empty.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Clears all stored values.
    pub fn clear(&mut self) {
        self.slots.clear();
    }

    /// Truncates the collection to the given length.
    pub fn truncate(&mut self, len: usize) {
        self.slots.truncate(len);
    }

    /// Returns the stored values as a vector.
    pub fn to_vec(&self) -> Vec<WasmValue> {
        self.slots.clone()
    }
}

impl FrameKind {
    /// Decodes this value from its compact byte representation.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => FrameKind::Function,
            1 => FrameKind::Block,
            2 => FrameKind::Loop,
            _ => FrameKind::Block,
        }
    }
}

impl ControlFrame {
    /// Creates a new `ControlFrame`.
    pub fn new(
        kind: FrameKind,
        param_count: u32,
        result_count: u32,
        label_count: u32,
        code: Vec<u8>,
        locals: Vec<WasmValue>,
    ) -> Self {
        Self {
            kind,
            position: 0,
            code,
            arity: result_count as usize,
            label_arity: label_count as usize,
            local_count: param_count as usize,
            height: 0,
            locals,
        }
    }

    /// Returns i32.
    pub fn get_i32(&self, stack: &mut OperandStack) -> crate::runtime::Result<i32> {
        let idx = match stack.pop() {
            Some(WasmValue::I32(v)) => v as u32,
            Some(_) => {
                return Err(crate::runtime::WasmError::Runtime(
                    "type mismatch".to_string(),
                ));
            }
            None => {
                return Err(crate::runtime::WasmError::Runtime(
                    "stack underflow".to_string(),
                ));
            }
        };
        Ok(idx as i32)
    }

    /// Returns i64.
    pub fn get_i64(&self, stack: &mut OperandStack) -> crate::runtime::Result<i64> {
        match stack.pop() {
            Some(WasmValue::I64(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Returns f32.
    pub fn get_f32(&self, stack: &mut OperandStack) -> crate::runtime::Result<f32> {
        match stack.pop() {
            Some(WasmValue::F32(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }

    /// Returns f64.
    pub fn get_f64(&self, stack: &mut OperandStack) -> crate::runtime::Result<f64> {
        match stack.pop() {
            Some(WasmValue::F64(v)) => Ok(v),
            Some(_) => Err(crate::runtime::WasmError::Runtime(
                "type mismatch".to_string(),
            )),
            None => Err(crate::runtime::WasmError::Runtime(
                "stack underflow".to_string(),
            )),
        }
    }
}

impl ControlStack {
    /// Creates a new `ControlStack`.
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    /// Pushes a value onto the stack.
    pub fn push(&mut self, frame: ControlFrame) {
        self.frames.push(frame);
    }

    /// Pops and returns the top value, if present.
    pub fn pop(&mut self) -> Option<ControlFrame> {
        self.frames.pop()
    }

    /// Pops and returns the top control frame, if present.
    pub fn pop_frame(&mut self) -> Option<ControlFrame> {
        self.frames.pop()
    }

    /// Returns the length.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Returns `true` if this value is empty.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Clears all stored values.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Returns the last value, if any.
    pub fn last(&self) -> Option<&ControlFrame> {
        self.frames.last()
    }

    /// Returns the last value mutably, if any.
    pub fn last_mut(&mut self) -> Option<&mut ControlFrame> {
        self.frames.last_mut()
    }

    /// Returns the current control frames.
    pub fn frames(&self) -> &[ControlFrame] {
        &self.frames
    }

    /// Truncates the collection to the given length.
    pub fn truncate(&mut self, len: usize) {
        self.frames.truncate(len);
    }

    /// Returns the value at the given index.
    pub fn get(&self, idx: usize) -> Option<&ControlFrame> {
        self.frames.get(idx)
    }

    /// Returns mut.
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut ControlFrame> {
        self.frames.get_mut(idx)
    }

    /// Serialises this value into bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        use std::io::Write;
        let mut bytes = Vec::new();
        for frame in &self.frames {
            bytes
                .write_all(&(frame.position as u32).to_le_bytes())
                .unwrap();
            bytes
                .write_all(&(frame.arity as u32).to_le_bytes())
                .unwrap();
            bytes
                .write_all(&(frame.label_arity as u32).to_le_bytes())
                .unwrap();
            bytes
                .write_all(&(frame.height as u32).to_le_bytes())
                .unwrap();
            bytes
                .write_all(&(frame.local_count as u32).to_le_bytes())
                .unwrap();
            bytes.push(frame.kind as u8);
        }
        bytes
    }

    /// Deserialises this value from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut frames = Vec::new();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if cursor + 21 > bytes.len() {
                break;
            }
            let position = u32::from_le_bytes([
                bytes[cursor],
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
            ]) as usize;
            let arity = u32::from_le_bytes([
                bytes[cursor + 4],
                bytes[cursor + 5],
                bytes[cursor + 6],
                bytes[cursor + 7],
            ]) as usize;
            let label_arity = u32::from_le_bytes([
                bytes[cursor + 8],
                bytes[cursor + 9],
                bytes[cursor + 10],
                bytes[cursor + 11],
            ]) as usize;
            let height = u32::from_le_bytes([
                bytes[cursor + 12],
                bytes[cursor + 13],
                bytes[cursor + 14],
                bytes[cursor + 15],
            ]) as usize;
            let local_count = u32::from_le_bytes([
                bytes[cursor + 16],
                bytes[cursor + 17],
                bytes[cursor + 18],
                bytes[cursor + 19],
            ]) as usize;
            let kind = FrameKind::from_u8(bytes[cursor + 20]);
            cursor += 21;

            frames.push(ControlFrame {
                kind,
                position,
                code: Vec::new(),
                arity,
                label_arity,
                local_count,
                height,
                locals: Vec::new(),
            });
        }
        Self { frames }
    }
}

impl Default for ControlStack {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operand_stack() {
        let mut stack = OperandStack::new(100);
        stack.push_unchecked(WasmValue::I32(42));
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.pop(), Some(WasmValue::I32(42)));
        assert!(stack.is_empty());
    }

    #[test]
    fn test_control_stack() {
        let mut stack = ControlStack::new();
        let frame = ControlFrame::new(FrameKind::Function, 0, 0, 0, vec![], vec![]);
        stack.push(frame);
        assert_eq!(stack.len(), 1);
    }
}
