// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sequential scatter-gather I/O helpers over kernel-owned iovecs.

extern crate alloc;

use alloc::vec::Vec;
use core::mem::{self, MaybeUninit};

use kerrno::{KError, KResult};
use kio::prelude::*;
use osvm::{read_vm_mem, write_vm_mem};

use super::IoVec;

/// A collection of I/O vectors for scatter-gather operations.
#[derive(Default)]
pub struct IoVectorBuf {
    /// Kernel-owned descriptors copied from the syscall boundary.
    iovs: Vec<IoVec>,
    /// Remaining total length across all segments.
    len: usize,
}

impl IoVectorBuf {
    /// Creates an I/O vector buffer from kernel-owned iovec descriptors.
    pub fn from_iovecs(iovs: Vec<IoVec>) -> KResult<Self> {
        if iovs.len() > 1024 {
            return Err(KError::InvalidInput);
        }

        let mut len = 0usize;
        for iov in &iovs {
            if iov.iov_len < 0 {
                return Err(KError::InvalidInput);
            }
            len = len
                .checked_add(iov.iov_len as usize)
                .ok_or(KError::InvalidInput)?;
        }

        Ok(Self { iovs, len })
    }

    /// Reads from iovec segments using a custom function.
    ///
    /// The pointers passed to `read_fn` point into user memory.
    /// The closure must use `read_vm_mem` or equivalent to access them safely.
    pub fn read_with(
        self,
        mut read_fn: impl FnMut(*const u8, usize) -> KResult<usize>,
    ) -> KResult<usize> {
        let mut count = 0;
        for iov in &self.iovs {
            if iov.iov_len == 0 {
                continue;
            }
            let read = read_fn(iov.iov_base, iov.iov_len as usize)?;
            if read == 0 {
                break;
            }
            count += read;
        }
        Ok(count)
    }

    /// Writes to iovec segments using a custom function.
    ///
    /// The pointers passed to `write_fn` point into user memory.
    /// The closure must use `write_vm_mem` or equivalent to access them safely.
    pub fn fill_with(
        self,
        mut write_fn: impl FnMut(*mut u8, usize) -> KResult<usize>,
    ) -> KResult<usize> {
        let mut count = 0;
        for iov in &self.iovs {
            if iov.iov_len == 0 {
                continue;
            }
            let written = write_fn(iov.iov_base, iov.iov_len as usize)?;
            if written == 0 {
                break;
            }
            count += written;
        }
        Ok(count)
    }

    /// Converts to a sequential I/O reader/writer over iovec segments.
    pub fn into_io(self) -> IoVectorBufIo {
        IoVectorBufIo {
            inner: self,
            start: 0,
            offset: 0,
        }
    }
}

/// Sequential reader/writer for I/O vector buffers.
pub struct IoVectorBufIo {
    inner: IoVectorBuf,
    start: usize,
    offset: usize,
}

impl IoVectorBufIo {
    fn skip_empty(&mut self) -> KResult<()> {
        while self.start < self.inner.iovs.len() {
            let iov = self.inner.iovs[self.start];
            if iov.iov_len as usize > self.offset {
                break;
            }
            self.offset = 0;
            self.start += 1;
        }
        Ok(())
    }
}

impl Read for IoVectorBufIo {
    fn read(&mut self, buf: &mut [u8]) -> KResult<usize> {
        let mut count = 0;
        loop {
            self.skip_empty()?;
            if self.start >= self.inner.iovs.len() {
                break;
            }
            let iov = self.inner.iovs[self.start];
            let len = (iov.iov_len as usize - self.offset).min(buf.len() - count);
            if len == 0 {
                break;
            }
            read_vm_mem(iov.iov_base.wrapping_add(self.offset), unsafe {
                mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[count..count + len])
            })?;
            self.offset += len;
            self.inner.len = self.inner.len.saturating_sub(len);
            count += len;
        }
        Ok(count)
    }
}

impl Write for IoVectorBufIo {
    fn write(&mut self, buf: &[u8]) -> KResult<usize> {
        let mut count = 0;
        loop {
            self.skip_empty()?;
            if self.start >= self.inner.iovs.len() {
                break;
            }
            let iov = self.inner.iovs[self.start];
            let len = (iov.iov_len as usize - self.offset).min(buf.len() - count);
            if len == 0 {
                break;
            }
            write_vm_mem(
                iov.iov_base.wrapping_add(self.offset),
                &buf[count..count + len],
            )?;
            self.offset += len;
            self.inner.len = self.inner.len.saturating_sub(len);
            count += len;
        }
        Ok(count)
    }

    fn flush(&mut self) -> KResult {
        Ok(())
    }
}

impl IoBuf for IoVectorBufIo {
    fn remaining(&self) -> usize {
        self.inner.len
    }
}

impl IoBufMut for IoVectorBufIo {
    fn remaining_mut(&self) -> usize {
        self.inner.len
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_io_vector_buf_default() {
        let buf = IoVectorBuf::default();
        assert_eq!(buf.iovs.len(), 0);
        assert_eq!(buf.len, 0);
    }

    #[def_test]
    fn test_io_vector_buf_io_remaining() {
        let buf = IoVectorBuf {
            iovs: Vec::new(),
            len: 42,
        };
        let io = buf.into_io();
        assert_eq!(io.remaining(), 42);
        assert_eq!(io.remaining_mut(), 42);
    }
}
