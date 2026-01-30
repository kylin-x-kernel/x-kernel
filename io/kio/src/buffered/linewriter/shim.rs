// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{BufWriter, Result, Write};

/// Private helper struct for implementing the line-buffered writing logic.
///
/// This shim temporarily wraps a BufWriter, and uses its internals to
/// implement a line-buffered writer (specifically by using the internal
/// methods like write_to_buf and flush_buf). In this way, a more
/// efficient abstraction can be created than one that only had access to
/// `write` and `flush`, without needlessly duplicating a lot of the
/// implementation details of BufWriter. This also allows existing
/// `BufWriters` to be temporarily given line-buffering logic; this is what
/// enables Stdout to be alternately in line-buffered or block-buffered mode.
#[derive(Debug)]
pub struct LineWriterShim<'a, W: ?Sized + Write> {
    buffer: &'a mut BufWriter<W>,
}

impl<'a, W: ?Sized + Write> LineWriterShim<'a, W> {
    pub fn new(buffer: &'a mut BufWriter<W>) -> Self {
        Self { buffer }
    }

    fn inner_mut(&mut self) -> &mut W {
        self.buffer.get_mut()
    }

    fn buffered(&self) -> &[u8] {
        self.buffer.buffer()
    }

    fn flush_if_completed_line(&mut self) -> Result<()> {
        match self.buffered().last().copied() {
            Some(b'\n') => self.buffer.flush_buf(),
            _ => Ok(()),
        }
    }
}

impl<'a, W: ?Sized + Write> Write for LineWriterShim<'a, W> {
    /// Writes some data into this BufWriter with line buffering.
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let newline_idx = match memchr::memrchr(b'\n', buf) {
            // If there are no new newlines just do a regular buffered write
            None => {
                self.flush_if_completed_line()?;
                return self.buffer.write(buf);
            }
            // Otherwise, arrange for the lines to be written directly to the
            // inner writer.
            Some(newline_idx) => newline_idx + 1,
        };

        // Flush existing content to prepare for our write.
        self.buffer.flush_buf()?;

        let lines = &buf[..newline_idx];

        let flushed = self.inner_mut().write(lines)?;

        if flushed == 0 {
            return Ok(0);
        }

        let tail = if flushed >= newline_idx {
            let tail = &buf[flushed..];
            if tail.len() >= self.buffer.capacity() {
                return Ok(flushed);
            }
            tail
        } else if newline_idx - flushed <= self.buffer.capacity() {
            &buf[flushed..newline_idx]
        } else {
            let scan_area = &buf[flushed..];
            let scan_area = &scan_area[..self.buffer.capacity()];
            match memchr::memrchr(b'\n', scan_area) {
                Some(newline_idx) => &scan_area[..newline_idx + 1],
                None => scan_area,
            }
        };

        let buffered = self.buffer.write_to_buf(tail);
        Ok(flushed + buffered)
    }

    fn flush(&mut self) -> Result<()> {
        self.buffer.flush()
    }

    /// Writes some data into this BufWriter with line buffering.
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        match memchr::memrchr(b'\n', buf) {
            None => {
                self.flush_if_completed_line()?;
                self.buffer.write_all(buf)
            }
            Some(newline_idx) => {
                let (lines, tail) = buf.split_at(newline_idx + 1);

                if self.buffered().is_empty() {
                    self.inner_mut().write_all(lines)?;
                } else {
                    self.buffer.write_all(lines)?;
                    self.buffer.flush_buf()?;
                }

                self.buffer.write_all(tail)
            }
        }
    }
}
