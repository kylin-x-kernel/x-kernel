// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Iterator-backed I/O buffers.
//!
//! The iterator owns the current progress through a kernel slice, user buffer
//! adapter, or iovec adapter, while the concrete user-memory access remains
//! outside this crate.

#![no_std]

use core::marker::PhantomData;

use kerrno::{KError, KResult};

/// Source iterator for data being written to a file.
pub trait IovSource {
    /// Returns the remaining byte count.
    fn count(&self) -> usize;

    /// Copies bytes from this source into `dst` and advances it.
    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize>;

    /// Moves the source cursor backward.
    fn revert(&mut self, count: usize) -> KResult<()> {
        if count == 0 {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        }
    }
}

/// Destination iterator for data being read from a file.
pub trait IovSink {
    /// Returns the remaining byte count.
    fn count(&self) -> usize;

    /// Copies bytes from `src` into this sink and advances it.
    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize>;

    /// Moves the sink cursor backward.
    fn revert(&mut self, count: usize) -> KResult<()> {
        if count == 0 {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        }
    }
}

enum IovSourceInner<'a> {
    Kvec { buf: &'a [u8], offset: usize },
    Reader(&'a mut dyn IovSource),
}

enum IovSinkInner<'a> {
    Kvec { buf: &'a mut [u8], offset: usize },
    Writer(&'a mut dyn IovSink),
}

enum IovIterInner<'a> {
    Source(IovSourceInner<'a>),
    Dest(IovSinkInner<'a>),
}

#[doc(hidden)]
pub enum IovIterSourceDirection {}

#[doc(hidden)]
pub enum IovIterDestDirection {}

/// Iterator state passed to `read_iter` and `write_iter`.
pub struct IovIter<'a, Direction> {
    inner: IovIterInner<'a>,
    count: usize,
    _direction: PhantomData<Direction>,
}

/// Data source passed to `write_iter`.
pub type IovIterSource<'a> = IovIter<'a, IovIterSourceDirection>;

/// Data destination passed to `read_iter`.
pub type IovIterDest<'a> = IovIter<'a, IovIterDestDirection>;

/// Creates a source iterator over a kernel byte slice.
pub fn iov_iter_kvec_source(buf: &[u8]) -> IovIterSource<'_> {
    IovIter {
        inner: IovIterInner::Source(IovSourceInner::Kvec { buf, offset: 0 }),
        count: buf.len(),
        _direction: PhantomData,
    }
}

/// Creates a source iterator over an abstract source.
pub fn iov_iter_source(reader: &mut dyn IovSource) -> IovIterSource<'_> {
    let count = reader.count();
    IovIter {
        inner: IovIterInner::Source(IovSourceInner::Reader(reader)),
        count,
        _direction: PhantomData,
    }
}

/// Creates a destination iterator over a mutable kernel byte slice.
pub fn iov_iter_kvec_dest(buf: &mut [u8]) -> IovIterDest<'_> {
    let count = buf.len();
    IovIter {
        inner: IovIterInner::Dest(IovSinkInner::Kvec { buf, offset: 0 }),
        count,
        _direction: PhantomData,
    }
}

/// Creates a destination iterator over an abstract sink.
pub fn iov_iter_dest(writer: &mut dyn IovSink) -> IovIterDest<'_> {
    let count = writer.count();
    IovIter {
        inner: IovIterInner::Dest(IovSinkInner::Writer(writer)),
        count,
        _direction: PhantomData,
    }
}

impl<Direction> IovIter<'_, Direction> {
    /// Returns the remaining byte count.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Limits the remaining byte count.
    pub fn truncate(&mut self, count: usize) {
        self.count = self.count.min(count);
    }
}

impl IovIter<'_, IovIterSourceDirection> {
    fn advance(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Moves this iterator backward by `count` bytes.
    pub fn revert(&mut self, count: usize) -> KResult<()> {
        let new_count = self.count.checked_add(count).ok_or(KError::InvalidInput)?;
        match &mut self.inner {
            IovIterInner::Source(IovSourceInner::Kvec { offset, .. }) => {
                if count > *offset {
                    return Err(KError::InvalidInput);
                }
                *offset -= count;
            }
            IovIterInner::Source(IovSourceInner::Reader(reader)) => {
                reader.revert(count)?;
            }
            IovIterInner::Dest(_) => unreachable!("source iterator stores source state"),
        }
        self.count = new_count;
        Ok(())
    }

    /// Copies bytes from this iterator into `dst` and advances it.
    pub fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize> {
        let max_len = dst.len().min(self.count);
        if max_len == 0 {
            return Ok(0);
        }

        let copied = match &mut self.inner {
            IovIterInner::Source(IovSourceInner::Kvec { buf, offset }) => {
                let len = max_len.min(buf.len().saturating_sub(*offset));
                if len == 0 {
                    return Ok(0);
                }
                dst[..len].copy_from_slice(&buf[*offset..*offset + len]);
                *offset += len;
                Ok(len)
            }
            IovIterInner::Source(IovSourceInner::Reader(reader)) => {
                reader.copy_from_iter(&mut dst[..max_len])
            }
            IovIterInner::Dest(_) => unreachable!("source iterator stores source state"),
        }?;

        self.advance(copied);
        Ok(copied)
    }
}

impl IovIter<'_, IovIterDestDirection> {
    fn advance(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Moves this iterator backward by `count` bytes.
    pub fn revert(&mut self, count: usize) -> KResult<()> {
        let new_count = self.count.checked_add(count).ok_or(KError::InvalidInput)?;
        match &mut self.inner {
            IovIterInner::Dest(IovSinkInner::Kvec { offset, .. }) => {
                if count > *offset {
                    return Err(KError::InvalidInput);
                }
                *offset -= count;
            }
            IovIterInner::Dest(IovSinkInner::Writer(writer)) => {
                writer.revert(count)?;
            }
            IovIterInner::Source(_) => {
                unreachable!("destination iterator stores destination state")
            }
        }
        self.count = new_count;
        Ok(())
    }

    /// Copies bytes from `src` into this iterator and advances it.
    pub fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        let max_len = src.len().min(self.count);
        if max_len == 0 {
            return Ok(0);
        }

        let copied = match &mut self.inner {
            IovIterInner::Dest(IovSinkInner::Kvec { buf, offset }) => {
                let len = max_len.min(buf.len().saturating_sub(*offset));
                if len == 0 {
                    return Ok(0);
                }
                buf[*offset..*offset + len].copy_from_slice(&src[..len]);
                *offset += len;
                Ok(len)
            }
            IovIterInner::Dest(IovSinkInner::Writer(writer)) => {
                writer.copy_to_iter(&src[..max_len])
            }
            IovIterInner::Source(_) => {
                unreachable!("destination iterator stores destination state")
            }
        }?;

        self.advance(copied);
        Ok(copied)
    }
}
