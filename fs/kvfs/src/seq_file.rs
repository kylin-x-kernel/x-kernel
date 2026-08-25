// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helpers for proc-style sequential virtual files.

use alloc::{string::String, sync::Arc};
use core::{cmp::min, fmt};

use ksync::Mutex;

use crate::{
    FileOperations, InodeOperations, Metadata, MetadataUpdate, NodePermission, NodeType, VfsError,
    VfsFile, VfsFileBuilder, VfsInodeInit, VfsResult,
    simple_fs::{SimpleFs, SimpleFsNode},
};

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

pub fn seq_open<I: SeqIterator>(file: &mut VfsFileBuilder, iter: I) -> VfsResult<()> {
    file.set_private_data(Arc::new(Mutex::new(SeqFile::new(iter))));
    file.disable_pwrite();
    Ok(())
}

/// File inode wrapper for [`SeqFile`].
pub struct SeqFileInode<I: SeqIterator> {
    node: SimpleFsNode,
    make_iter: Arc<dyn Fn() -> VfsResult<I> + Send + Sync>,
}

impl<I: SeqIterator> SeqFileInode<I> {
    /// Creates a read-only regular sequential file.
    ///
    /// The iterator factory runs at `open` time and may fail: procfs files
    /// backed by a process (e.g. `/proc/<pid>/maps`) must surface
    /// `NoSuchProcess` there instead of opening an empty file once the task
    /// has been reclaimed.
    pub fn new_regular(
        fs: Arc<SimpleFs>,
        make_iter: impl Fn() -> VfsResult<I> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            make_iter: Arc::new(make_iter),
        })
    }

    /// Returns the inode fields used when materializing this sequential file.
    pub fn inode_init(&self) -> VfsInodeInit {
        self.node.inode_init().with_size(VIRTUAL_FILE_SIZE)
    }
}

impl<I: SeqIterator> InodeOperations for SeqFileInode<I> {
    fn getattr(
        &self,
        idmap: &crate::MountIdmap,
        path: Option<&crate::Path>,
        request_mask: crate::GetattrRequestMask,
        query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut metadata = self.node.getattr(idmap, path, request_mask, query_flags)?;
        metadata.size = VIRTUAL_FILE_SIZE;
        Ok(metadata)
    }

    fn setattr(
        &self,
        idmap: &crate::MountIdmap,
        dentry: &crate::Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        self.node.setattr(idmap, dentry, update)
    }
}

impl<I: SeqIterator> FileOperations for SeqFileInode<I> {
    fn open(self: Arc<Self>, _inode: &crate::VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let iter = (self.make_iter)()?;
        seq_open(file, iter)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let state = file
            .private_data_get::<Mutex<SeqFile<I>>>()
            .ok_or(VfsError::InvalidInput)?;
        state.lock().read(buf, offset)
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
