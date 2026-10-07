// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// This file reuses the implementation from Rust's standard library
// (std::io) for a no_std environment.
// Because this project cannot depend on the standard library directly,
// a local copy is maintained in this repository.
//
// Source: https://github.com/rust-lang/rust/blob/main/library/std/src/io/mod.rs
// License: MIT

#[cfg(feature = "alloc")]
use alloc::{string::String, vec::Vec};
use core::io::BorrowedCursor;

use crate::{Chain, Error, Result, Take};

mod impls;

/// Fills `buf` using repeated [`Read::read`] calls.
///
/// # Errors
///
/// Retries [`Error::Interrupted`], returns [`Error::UnexpectedEof`] on premature
/// EOF, and forwards other read errors. Partial reads are not rolled back.
///
/// # Panics
///
/// Panics if a reader returns a byte count greater than the supplied slice.
pub fn default_read_exact<R: Read + ?Sized>(this: &mut R, mut buf: &mut [u8]) -> Result<()> {
    while !buf.is_empty() {
        match this.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                buf = &mut buf[n..];
            }
            Err(e) if e.is_interrupted() => continue,
            Err(e) => return Err(e),
        }
    }
    if !buf.is_empty() {
        Err(Error::UnexpectedEof)
    } else {
        Ok(())
    }
}

/// Reads once into the unfilled part of a borrowed cursor, initializing it first.
///
/// The callback follows [`Read::read`]; success advances by its returned count.
///
/// # Errors
///
/// Forwards the callback error, including [`Error::Interrupted`].
///
/// # Panics
///
/// Panics if the callback reports more bytes than the cursor capacity.
pub fn default_read_buf<F>(read: F, mut cursor: BorrowedCursor<'_>) -> Result<()>
where
    F: FnOnce(&mut [u8]) -> Result<usize>,
{
    #[cfg(borrowedbuf_init)]
    {
        let n = read(cursor.ensure_init().init_mut())?;
        cursor.advance(n);
    }
    #[cfg(not(borrowedbuf_init))]
    {
        // SAFETY: zero-filling only initializes the exclusively borrowed unfilled region;
        // it never replaces initialized bytes with uninitialized data.
        let n = read(unsafe { cursor.as_mut().write_filled(0) })?;
        assert!(n <= cursor.capacity());
        // SAFETY: the entire region was zero-filled above, and the count is in bounds.
        unsafe {
            cursor.advance(n);
        }
    }
    Ok(())
}

/// Fills the cursor using repeated [`Read::read_buf`] calls.
///
/// # Errors
///
/// Retries [`Error::Interrupted`], returns [`Error::UnexpectedEof`] on a successful
/// read without progress, and forwards other read errors. Filled bytes remain.
pub fn default_read_buf_exact<R: Read + ?Sized>(
    this: &mut R,
    mut cursor: BorrowedCursor<'_>,
) -> Result<()> {
    while cursor.capacity() > 0 {
        let prev_written = cursor.written();
        match this.read_buf(cursor.reborrow()) {
            Ok(()) => {}
            Err(e) if e.is_interrupted() => continue,
            Err(e) => return Err(e),
        }

        if cursor.written() == prev_written {
            return Err(Error::UnexpectedEof);
        }
    }

    Ok(())
}

/// Appends bytes until EOF, returning the number appended.
///
/// `size_hint` estimates remaining bytes for allocation/read sizing; it is not
/// a limit. Use [`Read::take`] to bound an untrusted stream.
///
/// # Errors
///
/// Retries [`Error::Interrupted`], forwards other read errors, and maps failed
/// `try_reserve` calls to [`Error::NoMemory`]. Appended bytes remain on error.
/// Some growth uses infallible allocation and can panic or terminate on failure.
#[cfg(feature = "alloc")]
pub fn default_read_to_end<R: Read + ?Sized>(
    r: &mut R,
    buf: &mut Vec<u8>,
    size_hint: Option<usize>,
) -> Result<usize> {
    use core::io::BorrowedBuf;

    use crate::DEFAULT_BUF_SIZE;

    let start_len = buf.len();
    let start_cap = buf.capacity();
    let mut max_read_size = size_hint
        .and_then(|s| {
            s.checked_add(1024)?
                .checked_next_multiple_of(DEFAULT_BUF_SIZE)
        })
        .unwrap_or(DEFAULT_BUF_SIZE);

    const PROBE_SIZE: usize = 32;

    fn small_probe_read<R: Read + ?Sized>(r: &mut R, buf: &mut Vec<u8>) -> Result<usize> {
        let mut probe = [0u8; PROBE_SIZE];

        loop {
            match r.read(&mut probe) {
                Ok(n) => {
                    buf.extend_from_slice(&probe[..n]);
                    return Ok(n);
                }
                Err(e) if e.is_interrupted() => continue,
                Err(e) => return Err(e),
            }
        }
    }

    if (size_hint.is_none() || size_hint == Some(0)) && buf.capacity() - buf.len() < PROBE_SIZE {
        let read = small_probe_read(r, buf)?;

        if read == 0 {
            return Ok(0);
        }
    }

    #[cfg(borrowedbuf_init)]
    let mut initialized = 0; // Extra initialized bytes from previous loop iteration
    #[cfg(borrowedbuf_init)]
    let mut consecutive_short_reads = 0;

    loop {
        if buf.len() == buf.capacity() && buf.capacity() == start_cap {
            let read = small_probe_read(r, buf)?;

            if read == 0 {
                return Ok(buf.len() - start_len);
            }
        }

        if buf.len() == buf.capacity() {
            buf.try_reserve(PROBE_SIZE).map_err(|_| Error::NoMemory)?;
        }

        let mut spare = buf.spare_capacity_mut();
        let buf_len = spare.len().min(max_read_size);
        spare = &mut spare[..buf_len];
        let mut read_buf: BorrowedBuf<'_> = spare.into();

        #[cfg(borrowedbuf_init)]
        // SAFETY: These bytes were initialized but not filled in the previous loop
        unsafe {
            read_buf.set_init(initialized);
        }

        let mut cursor = read_buf.unfilled();
        let result = loop {
            match r.read_buf(cursor.reborrow()) {
                Err(e) if e.is_interrupted() => continue,
                res => break res,
            }
        };

        #[cfg(borrowedbuf_init)]
        let unfilled_but_initialized = cursor.init_mut().len();
        let bytes_read = cursor.written();
        #[cfg(borrowedbuf_init)]
        let was_fully_initialized = read_buf.init_len() == buf_len;

        // SAFETY: BorrowedBuf's invariants mean this much memory is initialized.
        unsafe {
            let new_len = bytes_read + buf.len();
            buf.set_len(new_len);
        }

        result?;

        if bytes_read == 0 {
            return Ok(buf.len() - start_len);
        }

        #[cfg(borrowedbuf_init)]
        if bytes_read < buf_len {
            consecutive_short_reads += 1;
        } else {
            consecutive_short_reads = 0;
        }

        #[cfg(borrowedbuf_init)]
        {
            // store how much was initialized but not filled
            initialized = unfilled_but_initialized;
        }

        // Use heuristics to determine the max read size if no initial size hint was provided
        if size_hint.is_none() {
            #[cfg(borrowedbuf_init)]
            // The reader is returning short reads but it doesn't call ensure_init().
            if !was_fully_initialized && consecutive_short_reads > 1 {
                max_read_size = usize::MAX;
            }

            // we have passed a larger buffer than previously and the
            // reader still hasn't returned a short read
            if buf_len >= max_read_size && bytes_read == buf_len {
                max_read_size = max_read_size.saturating_mul(2);
            }
        }
    }
}

/// Appends bytes through a callback while preserving string validity.
///
/// # Safety
///
/// `f` must only append initialized bytes: it must not shrink the vector or
/// modify its original prefix, including when returning an error or unwinding.
/// This preserves the original UTF-8 prefix and makes suffix-only validation
/// and restoration of the original length sound.
#[cfg(feature = "alloc")]
pub(crate) unsafe fn append_to_string<F>(buf: &mut String, f: F) -> Result<usize>
where
    F: FnOnce(&mut Vec<u8>) -> Result<usize>,
{
    struct Guard<'a> {
        buf: &'a mut Vec<u8>,
        len: usize,
    }

    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            // SAFETY: the guard restores the original vector length if the read
            // path exits early before the appended bytes are validated.
            unsafe {
                self.buf.set_len(self.len);
            }
        }
    }

    let mut g = Guard {
        len: buf.len(),
        // SAFETY: this helper temporarily treats the string as a byte vector
        // and re-validates any appended suffix before committing the new length.
        buf: unsafe { buf.as_mut_vec() },
    };
    let ret = f(g.buf);

    // SAFETY: the caller promises to only append data to `buf`
    let appended = unsafe { g.buf.get_unchecked(g.len..) };
    if str::from_utf8(appended).is_err() {
        ret.and(Err(Error::IllegalBytes))
    } else {
        g.len = g.buf.len();
        ret
    }
}

/// Default [`Read::read_to_string`] implementation with optional size hint.
///
/// # Errors
///
/// Forwards [`default_read_to_end`] errors. Invalid appended UTF-8 is removed
/// and yields [`Error::IllegalBytes`] if reading otherwise succeeded. Existing
/// string contents are preserved; valid appended bytes may remain on error.
#[cfg(feature = "alloc")]
pub fn default_read_to_string<R: Read + ?Sized>(
    r: &mut R,
    buf: &mut String,
    size_hint: Option<usize>,
) -> Result<usize> {
    // Note that we do *not* call `r.read_to_end()` here. We are passing
    // `&mut Vec<u8>` (the raw contents of `buf`) into the `read_to_end`
    // method to fill it up. An arbitrary implementation could overwrite the
    // entire contents of the vector, not just append to it (which is what
    // we are expecting).
    // SAFETY: default_read_to_end only appends initialized bytes and leaves
    // the original prefix untouched, including on error. append_to_string
    // validates the new suffix before committing its length.
    unsafe { append_to_string(buf, |b| default_read_to_end(r, b, size_hint)) }
}

/// The `Read` trait allows for reading bytes from a source.
///
/// See [`std::io::Read`](https://doc.rust-lang.org/std/io/trait.Read.html) for more details.
pub trait Read {
    /// Pulls bytes into `buf`, returning a count in `0..=buf.len()`.
    ///
    /// Only the reported prefix contains newly read data. Short reads are allowed.
    /// Zero usually indicates EOF, or an empty destination. Implementations must
    /// not return an out-of-range count; adapters may panic on such violations.
    ///
    /// # Errors
    ///
    /// Returns the source's I/O error. The default helpers retry
    /// [`Error::Interrupted`] only where explicitly documented.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Read the exact number of bytes required to fill `buf`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnexpectedEof`] before filling `buf`, or forwards a read
    /// error other than [`Error::Interrupted`], which the default implementation retries.
    /// On failure the buffer and source may have advanced.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        default_read_exact(self, buf)
    }

    /// Pull some bytes from this source into the specified buffer.
    ///
    /// # Errors
    ///
    /// Forwards [`Read::read`] errors in the default implementation, without retry.
    /// Overrides may append initialized bytes before returning an error.
    fn read_buf(&mut self, buf: BorrowedCursor<'_>) -> Result<()> {
        default_read_buf(|b| self.read(b), buf)
    }

    /// Reads the exact number of bytes required to fill `cursor`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnexpectedEof`] when a successful read makes no progress,
    /// or forwards non-interruption read errors. Already filled bytes remain.
    fn read_buf_exact(&mut self, cursor: BorrowedCursor<'_>) -> Result<()> {
        default_read_buf_exact(self, cursor)
    }

    /// Read all bytes until EOF in this source, placing them into `buf`.
    ///
    /// # Errors
    ///
    /// The default implementation retries [`Error::Interrupted`], forwards other
    /// read errors, and returns [`Error::NoMemory`] for fallible reservation failure.
    /// Previously appended bytes remain. Infallible allocation can still terminate.
    #[cfg(feature = "alloc")]
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        default_read_to_end(self, buf, None)
    }

    /// Read all bytes until EOF in this source, appending them to `buf`.
    ///
    /// # Errors
    ///
    /// The default implementation forwards read/allocation errors and reports
    /// [`Error::IllegalBytes`] for invalid appended UTF-8 when reading otherwise
    /// succeeds. Invalid suffixes are removed; an existing read error takes precedence.
    #[cfg(feature = "alloc")]
    fn read_to_string(&mut self, buf: &mut String) -> Result<usize> {
        default_read_to_string(self, buf, None)
    }

    /// Creates a "by reference" adapter for this instance of `Read`.
    fn by_ref(&mut self) -> &mut Self
    where
        Self: Sized,
    {
        self
    }

    /// Creates an adapter which will chain this stream with another.
    fn chain<R: Read>(self, next: R) -> Chain<Self, R>
    where
        Self: Sized,
    {
        Chain::new(self, next)
    }

    /// Creates an adapter which will read at most `limit` bytes from it.
    fn take(self, limit: u64) -> Take<Self>
    where
        Self: Sized,
    {
        Take::new(self, limit)
    }
}

/// Reads all bytes from a [reader][Read] into a new [`String`].
///
/// This is a convenience function for [`Read::read_to_string`].
///
/// See [`std::io::read_to_string`](https://doc.rust-lang.org/std/io/fn.read_to_string.html)
/// for more details.
///
/// # Errors
///
/// Forwards [`Read::read_to_string`] errors from `reader`.
#[cfg(feature = "alloc")]
pub fn read_to_string<R: Read>(mut reader: R) -> Result<String> {
    let mut buf = String::new();
    reader.read_to_string(&mut buf)?;
    Ok(buf)
}

/// A `BufRead` is a type of `Read`er which has an internal buffer, allowing it
/// to perform extra ways of reading.
///
/// See [`std::io::BufRead`](https://doc.rust-lang.org/std/io/trait.BufRead.html)
/// for more details.
pub trait BufRead: Read {
    /// Returns the contents of the internal buffer, filling it with more data, via `Read` methods,
    /// if empty.
    ///
    /// # Errors
    ///
    /// Returns the underlying source error if filling fails. An empty successful
    /// slice indicates EOF or a zero-capacity buffer.
    fn fill_buf(&mut self) -> Result<&[u8]>;

    /// Marks the given `amount` of additional bytes from the internal buffer as having been read.
    /// Subsequent calls to `read` only return bytes that have not been marked as read.
    /// `amount` must not exceed the slice returned by the last `fill_buf` call.
    /// Violating this contract may panic or corrupt logical position accounting.
    fn consume(&mut self, amount: usize);

    /// Checks if there is any data left to be `read`.
    ///
    /// # Errors
    ///
    /// Forwards [`BufRead::fill_buf`] errors without retry.
    fn has_data_left(&mut self) -> Result<bool> {
        self.fill_buf().map(|b| !b.is_empty())
    }

    /// Skips bytes through the delimiter `byte` or EOF, returning bytes consumed.
    /// The delimiter, if found, is included in the count.
    ///
    /// # Errors
    ///
    /// Forwards [`BufRead::fill_buf`] errors, including interruptions, without
    /// retry. Bytes consumed before an error remain consumed.
    fn skip_until(&mut self, byte: u8) -> Result<usize> {
        let mut read = 0;
        loop {
            let (done, used) = {
                let available = self.fill_buf()?;
                match memchr::memchr(byte, available) {
                    Some(i) => (true, i + 1),
                    None => (false, available.len()),
                }
            };
            self.consume(used);
            read += used;
            if done || used == 0 {
                return Ok(read);
            }
        }
    }

    /// Appends bytes through delimiter `byte` or EOF and returns the appended count.
    /// The delimiter, if found, is included in both the buffer and count.
    /// Implementations must only append: existing bytes must remain unchanged,
    /// including on error or unwind, because `read_line` relies on this contract.
    ///
    /// # Errors
    ///
    /// Forwards [`BufRead::fill_buf`] errors without retry. Bytes already appended
    /// and consumed remain so. Allocation uses infallible vector growth.
    #[cfg(feature = "alloc")]
    fn read_until(&mut self, byte: u8, buf: &mut Vec<u8>) -> Result<usize> {
        let mut read = 0;
        loop {
            let (done, used) = {
                let available = self.fill_buf()?;
                match memchr::memchr(byte, available) {
                    Some(i) => {
                        buf.extend_from_slice(&available[..=i]);
                        (true, i + 1)
                    }
                    None => {
                        buf.extend_from_slice(available);
                        (false, available.len())
                    }
                }
            };
            self.consume(used);
            read += used;
            if done || used == 0 {
                return Ok(read);
            }
        }
    }

    /// Read all bytes until a newline (the `0xA` byte) is reached, and append
    /// them to the provided `String` buffer.
    ///
    /// # Errors
    ///
    /// Forwards [`BufRead::read_until`] errors. On successful reading, invalid
    /// appended UTF-8 produces [`Error::IllegalBytes`]; an invalid suffix is removed.
    /// The underlying stream is not rewound.
    #[cfg(feature = "alloc")]
    fn read_line(&mut self, buf: &mut String) -> Result<usize> {
        // SAFETY: the read_until contract requires append-only mutation even
        // on error or unwind. append_to_string validates that suffix before
        // exposing it through the String.
        unsafe { super::append_to_string(buf, |b| self.read_until(b'\n', b)) }
    }

    /// Returns an iterator over the contents of this reader split on the byte
    /// `byte`.
    #[cfg(feature = "alloc")]
    fn split(self, byte: u8) -> Split<Self>
    where
        Self: Sized,
    {
        Split {
            buf: self,
            delim: byte,
        }
    }

    /// Returns an iterator over the lines of this reader.
    #[cfg(feature = "alloc")]
    fn lines(self) -> Lines<Self>
    where
        Self: Sized,
    {
        Lines { buf: self }
    }
}

/// An iterator over the contents of an instance of `BufRead` split on a
/// particular byte.
///
/// This struct is generally created by calling [`split`] on a `BufRead`.
/// Please see the documentation of [`split`] for more details.
///
/// [`split`]: BufRead::split
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct Split<B> {
    buf: B,
    delim: u8,
}

#[cfg(feature = "alloc")]
impl<B: BufRead> Iterator for Split<B> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Result<Vec<u8>>> {
        let mut buf = Vec::new();
        match self.buf.read_until(self.delim, &mut buf) {
            Ok(0) => None,
            Ok(_n) => {
                if buf[buf.len() - 1] == self.delim {
                    buf.pop();
                }
                Some(Ok(buf))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// An iterator over the lines of an instance of `BufRead`.
///
/// This struct is generally created by calling [`lines`] on a `BufRead`.
/// Please see the documentation of [`lines`] for more details.
///
/// [`lines`]: BufRead::lines
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct Lines<B> {
    buf: B,
}

#[cfg(feature = "alloc")]
impl<B: BufRead> Iterator for Lines<B> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Result<String>> {
        let mut buf = String::new();
        match self.buf.read_line(&mut buf) {
            Ok(0) => None,
            Ok(_n) => {
                if buf.ends_with('\n') {
                    buf.pop();
                    if buf.ends_with('\r') {
                        buf.pop();
                    }
                }
                Some(Ok(buf))
            }
            Err(e) => Some(Err(e)),
        }
    }
}
