use std::io::{Read, Result};

/// Binary reader for WebAssembly data.
///
/// Provides low-level reading operations for WebAssembly binary format,
/// including LEB128 integer encoding and various primitive types.
pub struct BinaryReader<R: Read> {
    data: Vec<u8>,
    position: usize,
    _phantom: std::marker::PhantomData<R>,
}

impl<R: Read> BinaryReader<R> {
    /// Creates a new `BinaryReader`.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        Ok(Self {
            data,
            position: 0,
            _phantom: std::marker::PhantomData,
        })
    }

    /// Creates a reader from owned binary data.
    pub fn from_data(data: Vec<u8>) -> Self {
        Self {
            data,
            position: 0,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Returns the current position.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Returns the remaining length.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    /// Reads byte.
    pub fn read_byte(&mut self) -> Result<u8> {
        if self.position >= self.data.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected end of data",
            ));
        }
        let byte = self.data[self.position];
        self.position += 1;
        Ok(byte)
    }

    /// Reads bytes.
    pub fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
        if self.position + len > self.data.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected end of data",
            ));
        }
        let bytes = self.data[self.position..self.position + len].to_vec();
        self.position += len;
        Ok(bytes)
    }

    /// Reads u8.
    pub fn read_u8(&mut self) -> Result<u8> {
        self.read_byte()
    }

    /// Reads u16.
    pub fn read_u16(&mut self) -> Result<u16> {
        let lo = self.read_u8()? as u16;
        let hi = self.read_u8()? as u16;
        Ok(lo | (hi << 8))
    }

    /// Reads u32.
    pub fn read_u32(&mut self) -> Result<u32> {
        let b0 = self.read_u8()? as u32;
        let b1 = self.read_u8()? as u32;
        let b2 = self.read_u8()? as u32;
        let b3 = self.read_u8()? as u32;
        Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
    }

    /// Reads u64.
    pub fn read_u64(&mut self) -> Result<u64> {
        let lo = self.read_u32()? as u64;
        let hi = self.read_u32()? as u64;
        Ok(lo | (hi << 32))
    }

    /// Reads i32.
    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    /// Reads i64.
    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(self.read_u64()? as i64)
    }

    /// Reads f32.
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    /// Reads f64.
    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    /// Reads uleb128.
    pub fn read_uleb128(&mut self) -> Result<u32> {
        let mut result = 0u64;
        let mut shift = 0;
        let mut byte_count = 0u32;
        let mut last_byte: u8;
        loop {
            let byte = self.read_u8()?;
            byte_count += 1;
            last_byte = byte;
            result |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "uleb128 overflow",
                ));
            }
        }
        // Reject overlong encodings: for a u32, the 5th byte (shift=28) must
        // not have bits beyond bit 3 (i.e. byte & 0xF0 must be 0).
        if byte_count == 5 && (last_byte & 0xF0) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "uleb128 overlong encoding",
            ));
        }
        // Reject values that don't fit in u32
        if result > u32::MAX as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "uleb128 overflow",
            ));
        }
        Ok(result as u32)
    }

    /// Reads sleb128.
    pub fn read_sleb128(&mut self) -> Result<i32> {
        let mut result = 0i32;
        let mut shift = 0;
        let mut byte;
        loop {
            byte = self.read_u8()?;
            result |= ((byte & 0x7F) as i32) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
            if shift > 32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "sleb128 overflow",
                ));
            }
        }
        if shift < 32 && (byte & 0x40) != 0 {
            result |= !0_i32 << shift;
        }
        Ok(result)
    }

    /// Reads sleb128 i64.
    pub fn read_sleb128_i64(&mut self) -> Result<i64> {
        let mut result = 0i64;
        let mut shift = 0;
        let mut byte;
        loop {
            byte = self.read_u8()?;
            result |= ((byte & 0x7F) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
            if shift > 64 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "sleb128 overflow",
                ));
            }
        }
        if shift < 64 && (byte & 0x40) != 0 {
            result |= !0_i64 << shift;
        }
        Ok(result)
    }
}

impl BinaryReader<std::io::Cursor<&[u8]>> {
    /// Creates a reader over the given byte slice.
    pub fn from_slice(data: &[u8]) -> Self {
        Self::from_data(data.to_vec())
    }
}

impl<R: Read> Read for BinaryReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut count = 0;
        for b in buf.iter_mut() {
            match self.read_byte() {
                Ok(byte) => {
                    *b = byte;
                    count += 1;
                }
                Err(_) if count > 0 => return Ok(count),
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        for b in buf.iter_mut() {
            *b = self.read_byte()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_u32() {
        let data = vec![0x01, 0x02, 0x03, 0x04];
        let mut reader = BinaryReader::from_slice(&data);
        assert_eq!(reader.read_u32().unwrap(), 0x04030201);
    }

    #[test]
    fn test_read_uleb128() {
        let data = vec![0xE5, 0x8E, 0x26];
        let mut reader = BinaryReader::from_slice(&data);
        assert_eq!(reader.read_uleb128().unwrap(), 624485);
    }
}
