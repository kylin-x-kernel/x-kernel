// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helpers for proc-style sequential virtual files.

use alloc::{string::String, sync::Arc};
use core::{any::Any, cmp::min, fmt, task::Context};

use inherit_methods_macro::inherit_methods;
use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use kvfs::{
    FileNodeOps, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, VfsError,
    VfsResult,
};

use super::{SimpleFs, SimpleFsNode};

/// Internal buffer size for sequential virtual files.
pub const SEQ_BUF_SIZE: usize = 0x1000;

/// A large synthetic file size reported to userspace.
pub const VIRTUAL_FILE_SIZE: u64 = 1024 * 1024;

/// Iterator interface for proc-style files.
pub trait SeqIterator: Send + 'static {
    /// The item emitted on each step.
    type Item;

    /// Rewinds the iterator to the beginning.
    fn rewind(&mut self);

    /// Returns the first item.
    fn start(&mut self) -> Option<Self::Item>;

    /// Returns the next item.
    fn next(&mut self) -> Option<Self::Item>;

    /// Formats the current item into `buf`.
    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result;
}

/// Stateful adapter that manages buffering and read offsets.
pub struct SeqFile<I: SeqIterator> {
    iter: I,
    buf: String,
    buf_read_pos: usize,
    last_file_offset: u64,
    started: bool,
    is_eof: bool,
}

impl<I: SeqIterator> SeqFile<I> {
    /// Creates a new sequential file state.
    pub fn new(iter: I) -> Self {
        Self {
            iter,
            buf: String::with_capacity(SEQ_BUF_SIZE),
            buf_read_pos: 0,
            last_file_offset: 0,
            started: false,
            is_eof: false,
        }
    }

    /// Reads file content into `output` from `offset`.
    pub fn read(&mut self, output: &mut [u8], offset: u64) -> VfsResult<usize> {
        if offset == 0 {
            self.reset();
        } else if offset != self.last_file_offset {
            self.reset();
            self.seek_to(offset)?;
        }

        let mut total_written = 0;
        let mut output_cursor = 0;

        while output_cursor < output.len() {
            let available = self.buf.len().saturating_sub(self.buf_read_pos);
            if available > 0 {
                let to_copy = min(available, output.len() - output_cursor);
                output[output_cursor..output_cursor + to_copy].copy_from_slice(
                    &self.buf.as_bytes()[self.buf_read_pos..self.buf_read_pos + to_copy],
                );
                output_cursor += to_copy;
                self.buf_read_pos += to_copy;
                self.last_file_offset += to_copy as u64;
                total_written += to_copy;
                continue;
            }

            if self.is_eof {
                break;
            }

            if !self.fill_next_entry()? {
                break;
            }
        }

        Ok(total_written)
    }

    fn fill_next_entry(&mut self) -> VfsResult<bool> {
        if self.is_eof {
            return Ok(false);
        }

        self.buf.clear();
        self.buf_read_pos = 0;
        let next_item = if !self.started {
            self.started = true;
            self.iter.start()
        } else {
            self.iter.next()
        };

        match next_item {
            Some(item) => {
                self.iter
                    .show(&item, &mut self.buf)
                    .map_err(|_| VfsError::Io)?;
                if self.buf.is_empty() {
                    return self.fill_next_entry();
                }
                Ok(true)
            }
            None => {
                self.is_eof = true;
                Ok(false)
            }
        }
    }

    fn seek_to(&mut self, offset: u64) -> VfsResult<()> {
        while self.last_file_offset < offset {
            if self.buf.len().saturating_sub(self.buf_read_pos) == 0 && !self.fill_next_entry()? {
                break;
            }

            let available = self.buf.len().saturating_sub(self.buf_read_pos);
            let to_skip = min((offset - self.last_file_offset) as usize, available);
            self.buf_read_pos += to_skip;
            self.last_file_offset += to_skip as u64;
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.iter.rewind();
        self.buf.clear();
        self.buf_read_pos = 0;
        self.last_file_offset = 0;
        self.started = false;
        self.is_eof = false;
    }
}

/// File node wrapper for [`SeqFile`].
pub struct SeqFileNode<I: SeqIterator> {
    node: SimpleFsNode,
    inner: Mutex<SeqFile<I>>,
}

impl<I: SeqIterator> SeqFileNode<I> {
    /// Creates a read-only regular sequential file.
    pub fn new_regular(fs: Arc<SimpleFs>, iter: I) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            inner: Mutex::new(SeqFile::new(iter)),
        })
    }
}

#[inherit_methods(from = "self.node")]
impl<I: SeqIterator> NodeOps for SeqFileNode<I> {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata>;

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn sync(&self, data_only: bool) -> VfsResult<()>;

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(VIRTUAL_FILE_SIZE)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

impl<I: SeqIterator> Pollable for SeqFileNode<I> {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl<I: SeqIterator> FileNodeOps for SeqFileNode<I> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.inner.lock().read(buf, offset)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::PermissionDenied)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::PermissionDenied)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::string::String;
    use core::fmt;

    use unittest::{assert_eq, def_test};

    use super::{SeqFile, SeqIterator};

    struct LinesIter {
        lines: [&'static str; 2],
        next_index: usize,
    }

    impl LinesIter {
        fn new() -> Self {
            Self {
                lines: ["hello\n", "world\n"],
                next_index: 0,
            }
        }
    }

    impl SeqIterator for LinesIter {
        type Item = &'static str;

        fn rewind(&mut self) {
            self.next_index = 0;
        }

        fn start(&mut self) -> Option<Self::Item> {
            self.rewind();
            self.next()
        }

        fn next(&mut self) -> Option<Self::Item> {
            let item = self.lines.get(self.next_index).copied();
            if item.is_some() {
                self.next_index += 1;
            }
            item
        }

        fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
            buf.push_str(item);
            Ok(())
        }
    }

    #[def_test]
    fn test_seq_file_reads_across_multiple_entries() {
        let mut seq = SeqFile::new(LinesIter::new());
        let mut buf = [0; 12];
        assert_eq!(seq.read(&mut buf, 0).unwrap(), 12);
        assert_eq!(&buf, b"hello\nworld\n");
    }

    #[def_test]
    fn test_seq_file_rewinds_on_zero_offset() {
        let mut seq = SeqFile::new(LinesIter::new());
        let mut buf = [0; 6];
        assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
        assert_eq!(&buf, b"hello\n");
        assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
        assert_eq!(&buf, b"hello\n");
    }

    #[def_test]
    fn test_seq_file_seeks_forward_from_beginning() {
        let mut seq = SeqFile::new(LinesIter::new());
        let mut buf = [0; 4];
        assert_eq!(seq.read(&mut buf, 4).unwrap(), 4);
        assert_eq!(&buf, b"o\nwo");
    }

    #[def_test]
    fn test_seq_file_rewinds_and_seeks_from_middle_offset() {
        let mut seq = SeqFile::new(LinesIter::new());
        let mut buf = [0; 5];
        assert_eq!(seq.read(&mut buf, 7).unwrap(), 5);
        assert_eq!(&buf, b"orld\n");
    }
}
