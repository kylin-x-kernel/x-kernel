// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "alloc")]
use alloc::{collections::vec_deque::VecDeque, vec::Vec};
use core::{io::BorrowedBuf, mem::MaybeUninit};

use crate::{BufReader, BufWriter, DEFAULT_BUF_SIZE, Error, Read, Result, Write};

/// Copies the entire contents of a reader into a writer.
///
/// This function will continuously read data from `reader` and then
/// write it into `writer` in a streaming fashion until `reader`
/// returns EOF.
///
/// On success, the total number of bytes that were copied from
/// `reader` to `writer` is returned.
///
/// See [`std::io::copy`] for more details.
pub fn copy<R, W>(reader: &mut R, writer: &mut W) -> Result<u64>
where
    R: Read + ?Sized,
    W: Write + ?Sized,
{
    let read_buf = BufferedReaderSpec::buffer_size(reader);
    let write_buf = BufferedWriterSpec::buffer_size(writer);

    if read_buf >= DEFAULT_BUF_SIZE && read_buf >= write_buf {
        return BufferedReaderSpec::copy_to(reader, writer);
    }

    BufferedWriterSpec::copy_from(writer, reader)
}

pub fn stack_buffer_copy<R, W>(reader: &mut R, writer: &mut W) -> Result<u64>
where
    R: Read + ?Sized,
    W: Write + ?Sized,
{
    let buf: &mut [_] = &mut [MaybeUninit::uninit(); DEFAULT_BUF_SIZE];
    let mut buf: BorrowedBuf<'_> = buf.into();

    let mut len = 0;

    loop {
        match reader.read_buf(buf.unfilled()) {
            Ok(()) => {}
            Err(e) if e.canonicalize() == Error::Interrupted => continue,
            Err(e) => return Err(e),
        };

        if buf.filled().is_empty() {
            break;
        }

        len += buf.filled().len() as u64;
        writer.write_all(buf.filled())?;
        buf.clear();
    }

    Ok(len)
}

/// Specialization of the read-write loop that reuses the internal
/// buffer of a BufReader. If there's no buffer then the writer side
/// should be used instead.
trait BufferedReaderSpec {
    fn buffer_size(&self) -> usize;

    fn copy_to(&mut self, to: &mut (impl Write + ?Sized)) -> Result<u64>;
}

impl<T> BufferedReaderSpec for T
where
    Self: Read,
    T: ?Sized,
{
    #[inline]
    default fn buffer_size(&self) -> usize {
        0
    }

    default fn copy_to(&mut self, _to: &mut (impl Write + ?Sized)) -> Result<u64> {
        unreachable!("only called from specializations")
    }
}

impl BufferedReaderSpec for &[u8] {
    fn buffer_size(&self) -> usize {
        usize::MAX
    }

    fn copy_to(&mut self, to: &mut (impl Write + ?Sized)) -> Result<u64> {
        let len = self.len();
        to.write_all(self)?;
        *self = &self[len..];
        Ok(len as u64)
    }
}

#[cfg(feature = "alloc")]
impl BufferedReaderSpec for VecDeque<u8> {
    fn buffer_size(&self) -> usize {
        usize::MAX
    }

    fn copy_to(&mut self, to: &mut (impl Write + ?Sized)) -> Result<u64> {
        let len = self.len();
        let (front, back) = self.as_slices();
        to.write_all(front)?;
        to.write_all(back)?;
        self.clear();
        Ok(len as u64)
    }
}

impl<I> BufferedReaderSpec for BufReader<I>
where
    Self: Read,
    I: ?Sized,
{
    fn buffer_size(&self) -> usize {
        self.capacity()
    }

    fn copy_to(&mut self, to: &mut (impl Write + ?Sized)) -> Result<u64> {
        let mut len = 0;

        loop {
            match self.read(&mut []) {
                Ok(_) => {}
                Err(e) if e.canonicalize() == Error::Interrupted => continue,
                Err(e) => return Err(e),
            }
            let buf = self.buffer();
            if self.buffer().is_empty() {
                return Ok(len);
            }

            to.write_all(buf)?;
            len += buf.len() as u64;
            self.discard_buffer();
        }
    }
}

/// Specialization of the read-write loop that either uses a stack buffer
/// or reuses the internal buffer of a BufWriter
trait BufferedWriterSpec: Write {
    fn buffer_size(&self) -> usize;

    fn copy_from<R: Read + ?Sized>(&mut self, reader: &mut R) -> Result<u64>;
}

impl<W: Write + ?Sized> BufferedWriterSpec for W {
    #[inline]
    default fn buffer_size(&self) -> usize {
        0
    }

    default fn copy_from<R: Read + ?Sized>(&mut self, reader: &mut R) -> Result<u64> {
        stack_buffer_copy(reader, self)
    }
}

#[cfg(feature = "alloc")]
impl BufferedWriterSpec for Vec<u8> {
    fn buffer_size(&self) -> usize {
        core::cmp::max(DEFAULT_BUF_SIZE, self.capacity() - self.len())
    }

    fn copy_from<R: Read + ?Sized>(&mut self, reader: &mut R) -> Result<u64> {
        reader
            .read_to_end(self)
            .map(|bytes| u64::try_from(bytes).expect("usize overflowed u64"))
    }
}

impl<I: Write + ?Sized> BufferedWriterSpec for BufWriter<I> {
    fn buffer_size(&self) -> usize {
        self.capacity()
    }

    fn copy_from<R: Read + ?Sized>(&mut self, reader: &mut R) -> Result<u64> {
        if self.capacity() < DEFAULT_BUF_SIZE {
            return stack_buffer_copy(reader, self);
        }

        let mut len = 0;
        #[cfg(borrowedbuf_init)]
        let mut init = 0;

        loop {
            let buf = self.buffer_mut();
            let mut read_buf: BorrowedBuf<'_> = buf.spare_capacity_mut().into();

            #[cfg(borrowedbuf_init)]
            unsafe {
                // SAFETY: init is either 0 or the init_len from the previous iteration.
                read_buf.set_init(init);
            }

            if read_buf.capacity() >= DEFAULT_BUF_SIZE {
                let mut cursor = read_buf.unfilled();
                match reader.read_buf(cursor.reborrow()) {
                    Ok(()) => {
                        let bytes_read = cursor.written();

                        if bytes_read == 0 {
                            return Ok(len);
                        }

                        #[cfg(borrowedbuf_init)]
                        {
                            init = read_buf.init_len() - bytes_read;
                        }
                        len += bytes_read as u64;

                        // SAFETY: BorrowedBuf guarantees all of its filled bytes are init
                        unsafe { buf.set_len(buf.len() + bytes_read) };
                    }
                    Err(ref e) if e.canonicalize() == Error::Interrupted => {}
                    Err(e) => return Err(e),
                }
            } else {
                #[cfg(borrowedbuf_init)]
                {
                    init += buf.len();
                }

                self.flush_buf()?;
            }
        }
    }
}
