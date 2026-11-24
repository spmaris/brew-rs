//! A reader that fails if its input continues past a byte budget.

use std::io::{self, Read};

/// A reader that fails if its input continues after a byte budget.
pub(super) struct LimitedReader<R> {
    pub(super) inner: R,
    pub(super) remaining: u64,
}

impl<R> LimitedReader<R> {
    pub(super) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(
                    "bottle archive expanded beyond the decompressed byte limit",
                )),
            };
        }
        let allowed = self.remaining.min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}
