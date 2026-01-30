// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scatter-gather I/O helpers for user memory buffers.

use core::mem::{self, MaybeUninit};

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult};
use kio::prelude::*;
use osvm::{VirtPtr, read_vm_mem, write_vm_mem};

/// I/O vector representing a single buffer segment
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct IoVec {
    /// Base address of the buffer in user memory.
    pub iov_base: *mut u8,
    /// Length of the buffer in bytes.
    pub iov_len: isize,
}

/// A collection of I/O vectors for scatter-gather operations
#[derive(Default)]
pub struct IoVectorBuf {
    /// Pointer to the user-space iovec array.
    iovs: *const IoVec,
    /// Number of iovec entries.
    iovcnt: usize,
    /// Remaining total length across all segments.
    len: usize,
}

impl IoVectorBuf {
    /// Create a new I/O vector buffer from a user-space iovec array
    pub fn new(iovs: *const IoVec, iovcnt: usize) -> KResult<Self> {
        if iovcnt > 1024 {
            return Err(KError::InvalidInput);
        }
        let mut len = 0;
        for i in 0..iovcnt {
            let iov = iovs.wrapping_add(i).read_vm()?;
            if iov.iov_len < 0 {
                return Err(KError::InvalidInput);
            }
            len += iov.iov_len as usize;
        }
        Ok(Self { iovs, iovcnt, len })
    }

    /// Read from iovec segments using a custom function
    pub fn read_with(
        self,
        mut f: impl FnMut(*const u8, usize) -> KResult<usize>,
    ) -> KResult<usize> {
        let mut count = 0;
        for i in 0..self.iovcnt {
            let iov = self.iovs.wrapping_add(i).read_vm()?;
            if iov.iov_len == 0 {
                continue;
            }
            let read = f(iov.iov_base, iov.iov_len as usize)?;
            if read == 0 {
                break;
            }
            count += read;
        }
        Ok(count)
    }

    /// Write to iovec segments using a custom function
    pub fn fill_with(self, mut f: impl FnMut(*mut u8, usize) -> KResult<usize>) -> KResult<usize> {
        let mut count = 0;
        for i in 0..self.iovcnt {
            let iov = self.iovs.wrapping_add(i).read_vm()?;
            if iov.iov_len == 0 {
                continue;
            }
            let written = f(iov.iov_base, iov.iov_len as usize)?;
            if written == 0 {
                break;
            }
            count += written;
        }
        Ok(count)
    }

    /// Convert to a sequential I/O reader/writer over iovec segments
    pub fn into_io(self) -> IoVectorBufIo {
        IoVectorBufIo {
            inner: self,
            start: 0,
            offset: 0,
        }
    }
}

/// Sequential reader/writer for I/O vector buffers
pub struct IoVectorBufIo {
    inner: IoVectorBuf,
    start: usize,
    offset: usize,
}

impl IoVectorBufIo {
    fn skip_empty(&mut self) -> KResult<()> {
        while self.start < self.inner.iovcnt {
            let iov = self.inner.iovs.wrapping_add(self.start).read_vm()?;
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
            if self.start >= self.inner.iovcnt {
                break;
            }
            let iov = self.inner.iovs.wrapping_add(self.start).read_vm()?;
            let len = (iov.iov_len as usize - self.offset).min(buf.len() - count);
            if len == 0 {
                break;
            }
            read_vm_mem(iov.iov_base.wrapping_add(self.offset), unsafe {
                mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[count..count + len])
            })?;
            self.offset += len;
            self.inner.len -= len;
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
            if self.start >= self.inner.iovcnt {
                break;
            }
            let iov = self.inner.iovs.wrapping_add(self.start).read_vm()?;
            let len = (iov.iov_len as usize - self.offset).min(buf.len() - count);
            if len == 0 {
                break;
            }
            write_vm_mem(
                iov.iov_base.wrapping_add(self.offset),
                &buf[count..count + len],
            )?;
            self.offset += len;
            self.inner.len -= len;
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
