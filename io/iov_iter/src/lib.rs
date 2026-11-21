// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Iterator-backed I/O buffers.
//!
//! The iterator owns the current progress through a kernel slice, user buffer
//! adapter, or iovec adapter, while the concrete user-memory access remains
//! outside this crate.
//!
//! Start with [`iov_iter_kvec_source`] for a borrowed kernel input slice or
//! [`iov_iter_kvec_dest`] for a borrowed kernel output slice.
//! [`iov_iter_source`] and [`iov_iter_dest`] wrap implementations of
//! [`IovSource`] and [`IovSink`] for other memory-access policies.
//! [`IovIterSource`] supplies bytes to file writes; [`IovIterDest`] receives
//! bytes from file reads. The direction is fixed at construction.
//!
//! # Example
//!
//! Copy a bounded prefix, rewind the source, and consume that byte again.
//!
//! ```
//! use iov_iter::{iov_iter_kvec_dest, iov_iter_kvec_source};
//!
//! let mut source = iov_iter_kvec_source(b"hello");
//! source.truncate(3);
//! let mut bytes = [0; 3];
//! assert_eq!(source.copy_from_iter(&mut bytes).unwrap(), 3);
//! assert_eq!(&bytes, b"hel");
//! source.revert(1).unwrap();
//! let mut last = [0; 1];
//! assert_eq!(source.copy_from_iter(&mut last).unwrap(), 1);
//! assert_eq!(&last, b"l");
//!
//! let mut output = [0; 3];
//! {
//!     let mut dest = iov_iter_kvec_dest(&mut output);
//!     assert_eq!(dest.copy_to_iter(&bytes).unwrap(), 3);
//!     assert_eq!(dest.count(), 0);
//! }
//! assert_eq!(&output, b"hel");
//! ```

#![no_std]

use core::marker::PhantomData;

use kerrno::{KError, KResult};

/// Source adapter supplying bytes for file writes.
///
/// Implementations own their cursor and memory-access policy. Successful copies
/// must report the bytes actually copied, at most the supplied slice length and
/// remaining count, and advance their cursor by that amount. The wrapper trusts
/// this accounting. Execution context and partial effects on failure depend on
/// the implementation and must be documented by implementers.
pub trait IovSource {
    /// Returns the remaining byte count.
    fn count(&self) -> usize;

    /// Copies bytes into `dst`, returning the transferred count and advancing the source.
    ///
    /// Short copies, including zero, are permitted.
    ///
    /// # Errors
    ///
    /// Returns implementation-specific access errors. Implementers must describe
    /// those errors and any buffer or cursor changes made before failure.
    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize>;

    /// Moves the source cursor backward by `count` bytes.
    ///
    /// A successful rewind restores that many bytes to the remaining count.
    /// It does not undo previously copied data. The default implementation only
    /// accepts zero; implementations supporting rewind define its valid range.
    /// See the [crate example](crate#example) for copying before rewinding a slice iterator.
    ///
    /// # Errors
    ///
    /// The default implementation returns [`KError::InvalidInput`] for nonzero
    /// `count`. Overrides must document their rejection conditions and any
    /// state changes on failure.
    fn revert(&mut self, count: usize) -> KResult<()> {
        if count == 0 {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        }
    }
}

/// Destination adapter receiving bytes from file reads.
///
/// Implementations own their cursor and memory-access policy. Successful copies
/// must report the bytes actually copied, at most the supplied slice length and
/// remaining count, and advance their cursor by that amount. The wrapper trusts
/// this accounting. Execution context and partial effects on failure depend on
/// the implementation and must be documented by implementers.
pub trait IovSink {
    /// Returns the remaining byte count.
    fn count(&self) -> usize;

    /// Copies bytes from `src`, returning the transferred count and advancing the sink.
    ///
    /// Short copies, including zero, are permitted.
    ///
    /// # Errors
    ///
    /// Returns implementation-specific access errors. Implementers must describe
    /// those errors and any buffer or cursor changes made before failure.
    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize>;

    /// Copies without resolving user faults or entering a backing provider.
    ///
    /// Advances only by the returned prefix. An inaccessible user page stops
    /// the copy, including with zero bytes, rather than sleeping to populate it.
    /// Kernel-memory sinks can delegate to `copy_to_iter` if it cannot fault.
    ///
    /// # Errors
    /// Returns an access error only when no bytes were transferred. Implementers
    /// must preserve the cursor on error.
    fn copy_to_iter_nofault(&mut self, src: &[u8]) -> KResult<usize>;

    /// Makes the current destination prefix writable without advancing it.
    ///
    /// Call only after releasing folio and backing-provider locks. Returns true
    /// when user pages were checked and a short nofault copy should be retried.
    /// Kernel-memory sinks return false because they have no faults to resolve.
    /// Completion does not pin mappings; subsequent copies must remain nofault.
    ///
    /// # Errors
    /// Returns an access or allocation error when the prefix cannot be prepared.
    fn fault_in_writeable(&mut self, _count: usize) -> KResult<bool> {
        Ok(false)
    }

    /// Moves the sink cursor backward by `count` bytes.
    ///
    /// A successful rewind restores that many bytes to the remaining count.
    /// It does not undo previously copied data. The default implementation only
    /// accepts zero; implementations supporting rewind define its valid range.
    /// See the [crate example](crate#example) for copying before rewinding a slice iterator.
    ///
    /// # Errors
    ///
    /// The default implementation returns [`KError::InvalidInput`] for nonzero
    /// `count`. Overrides must document their rejection conditions and any
    /// state changes on failure.
    fn revert(&mut self, count: usize) -> KResult<()> {
        if count == 0 {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        }
    }
}

/// Marker for [`IovSink`] implementations whose `revert` accepts nonzero
/// counts, rewinding the cursor and restoring the remaining count.
///
/// Direct I/O destinations require this capability so the engine can rewind an
/// end-of-file tail, mirroring Linux `iov_iter_revert`. Implementing this trait
/// asserts [`IovSink::revert`] is a real rewind, not the default that rejects
/// every nonzero count.
pub trait RewindableSink: IovSink {}

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

/// Type-level marker selecting source operations on an iterator.
#[doc(hidden)]
pub enum IovIterSourceDirection {}

/// Type-level marker selecting destination operations on an iterator.
#[doc(hidden)]
pub enum IovIterDestDirection {}

/// Borrowed buffer cursor and remaining transfer budget for file I/O.
///
/// Use [`IovIterSource`] or [`IovIterDest`] and their constructor functions.
/// `Direction` selects the available copy operation at compile time; `'a` ties
/// the iterator to its borrowed slice or adapter. Dropping the iterator ends
/// the borrow without rewinding, clearing data, or releasing the backing storage.
/// The iterator has no internal synchronization; mutation requires exclusive
/// access. Adapter-backed operations inherit the adapter's context constraints.
pub struct IovIter<'a, Direction> {
    inner: IovIterInner<'a>,
    count: usize,
    _direction: PhantomData<Direction>,
}

/// Data source passed to `write_iter`.
pub type IovIterSource<'a> = IovIter<'a, IovIterSourceDirection>;

/// Data destination passed to `read_iter`.
pub type IovIterDest<'a> = IovIter<'a, IovIterDestDirection>;

/// Data destination whose sink supports nonzero rewind.
///
/// Required by consumers that must give back an over-copied end-of-file tail.
/// Only constructible from a [`RewindableSink`] writer or a kernel slice, so the
/// rewind capability is part of the type. Operations forward to the wrapped
/// [`IovIterDest`].
pub struct IovIterDestRewind<'a>(IovIterDest<'a>);

impl IovIterDestRewind<'_> {
    /// Returns the remaining byte count.
    pub fn count(&self) -> usize {
        self.0.count()
    }

    /// Shrinks the remaining transfer budget to at most `count` bytes.
    ///
    /// Leaves the cursor and the wrapped destination unchanged.
    pub fn truncate(&mut self, count: usize) {
        self.0.truncate(count);
    }

    /// Copies bytes into the wrapped destination and advances it.
    ///
    /// Returns the number of bytes copied.
    ///
    /// # Errors
    ///
    /// Propagates the wrapped destination's errors unchanged; the wrapper
    /// count is unchanged on error.
    pub fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        self.0.copy_to_iter(src)
    }

    /// Copies without resolving user faults through the wrapped destination.
    ///
    /// # Errors
    ///
    /// Propagates the wrapped destination's errors unchanged.
    pub fn copy_to_iter_nofault(&mut self, src: &[u8]) -> KResult<usize> {
        self.0.copy_to_iter_nofault(src)
    }

    /// Prepares at most `count` remaining bytes without advancing the cursor.
    ///
    /// # Errors
    ///
    /// Propagates the wrapped destination's access or allocation errors.
    pub fn fault_in_writeable(&mut self, count: usize) -> KResult<bool> {
        self.0.fault_in_writeable(count)
    }

    /// Rewinds the cursor by `count` bytes and restores that much budget.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the remaining count would overflow or
    /// a kernel slice would rewind before its beginning. Adapter errors
    /// propagate unchanged; the wrapper count is unchanged on error.
    pub fn revert(&mut self, count: usize) -> KResult<()> {
        self.0.revert(count)
    }
}

/// Creates a source iterator over a kernel byte slice.
///
/// Retains an immutable borrow of `buf`, starts at offset zero, and sets the
/// remaining count to `buf.len()`. No allocation or copying occurs.
pub fn iov_iter_kvec_source(buf: &[u8]) -> IovIterSource<'_> {
    IovIter {
        inner: IovIterInner::Source(IovSourceInner::Kvec { buf, offset: 0 }),
        count: buf.len(),
        _direction: PhantomData,
    }
}

/// Creates a source iterator borrowing `reader` exclusively.
///
/// Snapshots [`IovSource::count`] as the transfer budget without resetting the
/// adapter cursor. Subsequent copies and rewinds are delegated to `reader`.
pub fn iov_iter_source(reader: &mut dyn IovSource) -> IovIterSource<'_> {
    let count = reader.count();
    IovIter {
        inner: IovIterInner::Source(IovSourceInner::Reader(reader)),
        count,
        _direction: PhantomData,
    }
}

/// Creates a destination iterator over a mutable kernel byte slice.
///
/// Retains an exclusive mutable borrow of `buf`, starts at offset zero, and sets the
/// remaining count to `buf.len()`. No allocation or copying occurs.
pub fn iov_iter_kvec_dest(buf: &mut [u8]) -> IovIterDest<'_> {
    let count = buf.len();
    IovIter {
        inner: IovIterInner::Dest(IovSinkInner::Kvec { buf, offset: 0 }),
        count,
        _direction: PhantomData,
    }
}

/// Creates a destination iterator borrowing `writer` exclusively.
///
/// Snapshots [`IovSink::count`] as the transfer budget without resetting the
/// adapter cursor. Subsequent copies and rewinds are delegated to `writer`.
pub fn iov_iter_dest(writer: &mut dyn IovSink) -> IovIterDest<'_> {
    let count = writer.count();
    IovIter {
        inner: IovIterInner::Dest(IovSinkInner::Writer(writer)),
        count,
        _direction: PhantomData,
    }
}

/// Creates a rewindable destination iterator over a mutable kernel byte slice.
///
/// Retains an exclusive mutable borrow of `buf`, starts at offset zero, and sets
/// the remaining count to `buf.len()`. No allocation or copying occurs.
pub fn iov_iter_kvec_dest_rewind(buf: &mut [u8]) -> IovIterDestRewind<'_> {
    IovIterDestRewind(iov_iter_kvec_dest(buf))
}

/// Creates a rewindable destination iterator borrowing `writer` exclusively.
///
/// Snapshots [`IovSink::count`] as the transfer budget without resetting the
/// adapter cursor. Subsequent copies and rewinds are delegated to `writer`.
pub fn iov_iter_dest_rewind(writer: &mut dyn RewindableSink) -> IovIterDestRewind<'_> {
    let sink: &mut dyn IovSink = writer;
    IovIterDestRewind(iov_iter_dest(sink))
}

impl<Direction> IovIter<'_, Direction> {
    /// Returns the remaining byte count.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Shrinks the remaining transfer budget to at most `count` bytes.
    ///
    /// Leaves the cursor and backing adapter unchanged. A larger value does not
    /// expand the budget. A later successful rewind adds bytes to this budget;
    /// truncation is not a permanent upper bound and cannot itself be undone.
    pub fn truncate(&mut self, count: usize) {
        self.count = self.count.min(count);
    }
}

impl IovIter<'_, IovIterSourceDirection> {
    fn advance(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Rewinds the cursor by `count` bytes and adds them to the transfer budget.
    ///
    /// Previously copied bytes are not restored or cleared. For a kernel slice,
    /// `count` must not exceed the bytes already consumed. For an adapter, the
    /// adapter determines the allowed rewind range. See the [crate example](crate#example)
    /// for the copy-then-rewind sequence.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the remaining count would overflow or
    /// a kernel slice would rewind before its beginning. Adapter rewind errors
    /// are propagated unchanged (see the corresponding adapter trait's `revert`
    /// contract). The wrapper count is unchanged on error; adapter state changes
    /// on failure are governed by the adapter.
    ///
    /// # Panics
    ///
    /// An internal direction mismatch triggers an unreachable assertion. Public
    /// constructors preserve the direction invariant. An adapter may also panic.
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
            IovIterInner::Dest(_) => {
                unreachable!("source iterator stores source state")
            }
        }
        self.count = new_count;
        Ok(())
    }

    /// Copies bytes from this iterator into `dst` and advances it.
    ///
    /// Returns the number of bytes copied, reducing the remaining budget by
    /// that amount. The request is limited to `dst.len()` and the budget;
    /// a zero-length request returns zero without calling an adapter. Kernel
    /// slices copy the full limited request; adapters may return a short copy.
    /// The adapter's reported count is trusted, not validated by this wrapper.
    ///
    /// # Errors
    ///
    /// Kernel slices do not return errors. Adapter errors are propagated from
    /// [`IovSource::copy_from_iter`] without changing the wrapper count. The
    /// adapter may already have changed data or its cursor; no rollback occurs.
    ///
    /// # Panics
    ///
    /// An internal direction mismatch triggers an unreachable assertion. Public
    /// constructors preserve the direction invariant. An adapter may also panic.
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
            IovIterInner::Dest(_) => {
                unreachable!("source iterator stores source state")
            }
        }?;

        self.advance(copied);
        Ok(copied)
    }
}

impl IovIter<'_, IovIterDestDirection> {
    fn advance(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Rewinds the cursor by `count` bytes and adds them to the transfer budget.
    ///
    /// Previously copied bytes are not restored or cleared. For a kernel slice,
    /// `count` must not exceed the bytes already consumed. For an adapter, the
    /// adapter determines the allowed rewind range. See the [crate example](crate#example)
    /// for the copy-then-rewind sequence.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the remaining count would overflow or
    /// a kernel slice would rewind before its beginning. Adapter rewind errors
    /// are propagated unchanged (see the corresponding adapter trait's `revert`
    /// contract). The wrapper count is unchanged on error; adapter state changes
    /// on failure are governed by the adapter.
    ///
    /// # Panics
    ///
    /// An internal direction mismatch triggers an unreachable assertion. Public
    /// constructors preserve the direction invariant. An adapter may also panic.
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
    ///
    /// Returns the number of bytes copied, reducing the remaining budget by
    /// that amount. The request is limited to `src.len()` and the budget;
    /// a zero-length request returns zero without calling an adapter. Kernel
    /// slices copy the full limited request; adapters may return a short copy.
    /// The adapter's reported count is trusted, not validated by this wrapper.
    ///
    /// # Errors
    ///
    /// Kernel slices do not return errors. Adapter errors are propagated from
    /// [`IovSink::copy_to_iter`] without changing the wrapper count. The
    /// adapter may already have changed data or its cursor; no rollback occurs.
    ///
    /// # Panics
    ///
    /// An internal direction mismatch triggers an unreachable assertion. Public
    /// constructors preserve the direction invariant. An adapter may also panic.
    pub fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        self.copy_to_iter_with(src, |writer, bytes| writer.copy_to_iter(bytes))
    }

    /// Copies a prefix without resolving user faults, advancing by that prefix.
    ///
    /// Kernel slices use the ordinary copy; adapters obey
    /// [`IovSink::copy_to_iter_nofault`]. Callers can release their locks and use
    /// [`Self::fault_in_writeable`] before retrying a short copy.
    ///
    /// # Errors
    /// Propagates adapter errors without advancing the iterator.
    pub fn copy_to_iter_nofault(&mut self, src: &[u8]) -> KResult<usize> {
        self.copy_to_iter_with(src, |writer, bytes| writer.copy_to_iter_nofault(bytes))
    }

    /// Prepares at most `count` remaining bytes without advancing the cursor.
    ///
    /// Returns false for kernel slices; adapters follow
    /// [`IovSink::fault_in_writeable`]. May sleep and must run outside folio and
    /// backing-provider locks. A later copy must still tolerate mapping changes.
    ///
    /// # Errors
    /// Propagates adapter access or allocation errors.
    pub fn fault_in_writeable(&mut self, count: usize) -> KResult<bool> {
        let count = count.min(self.count);
        if count == 0 {
            return Ok(false);
        }
        match &mut self.inner {
            IovIterInner::Dest(IovSinkInner::Kvec { .. }) => Ok(false),
            IovIterInner::Dest(IovSinkInner::Writer(writer)) => writer.fault_in_writeable(count),
            IovIterInner::Source(_) => {
                unreachable!("destination iterator stores destination state")
            }
        }
    }

    fn copy_to_iter_with(
        &mut self,
        src: &[u8],
        copy: impl FnOnce(&mut dyn IovSink, &[u8]) -> KResult<usize>,
    ) -> KResult<usize> {
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
            IovIterInner::Dest(IovSinkInner::Writer(writer)) => copy(*writer, &src[..max_len]),
            IovIterInner::Source(_) => {
                unreachable!("destination iterator stores destination state")
            }
        }?;

        self.advance(copied);
        Ok(copied)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/rewind.rs");
