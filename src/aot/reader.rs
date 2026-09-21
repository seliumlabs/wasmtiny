//! A strict, bounds-checked reader for artifact parsing.
//!
//! Every read fails with a structured [`WasmError::Load`] rather than
//! panicking on truncated or malformed input.

use crate::runtime::{Result, WasmError};

/// A cursor over an artifact byte slice. Reads are little-endian.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// The current read position within the underlying slice.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// True when the whole slice has been consumed.
    pub fn at_end(&self) -> bool {
        self.remaining() == 0
    }

    /// Consumes and returns the next `n` bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(truncated)?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads a single byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    /// Reads a length-prefixed byte slice, bounded by `max`.
    pub fn read_len_bytes(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = self.read_u32()? as usize;
        if len > max {
            return Err(load_error(format!(
                "declared length {len} exceeds bound {max}"
            )));
        }
        self.read_bytes(len)
    }

    /// Reads a length-prefixed UTF-8 string, bounded by `max`.
    pub fn read_string(&mut self, max: usize) -> Result<&'a str> {
        let bytes = self.read_len_bytes(max)?;
        std::str::from_utf8(bytes)
            .map_err(|_| load_error("invalid UTF-8 in string field".to_string()))
    }

    /// Reads a length-prefixed count, bounded by the remaining bytes so a
    /// crafted artifact cannot drive unbounded allocation.
    pub fn read_count(&mut self, min_item_size: usize) -> Result<u32> {
        let count = self.read_u32()? as usize;
        if min_item_size > 0 && count > self.remaining() / min_item_size {
            return Err(load_error(format!(
                "count {count} is inconsistent with remaining {} bytes",
                self.remaining()
            )));
        }
        u32::try_from(count).map_err(|_| load_error("count overflow".to_string()))
    }
}

/// A structured load error.
pub(crate) fn load_error(message: String) -> WasmError {
    WasmError::Load(message)
}

fn truncated() -> WasmError {
    load_error("unexpected end of artifact".to_string())
}
