// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helpers for proc-style sequential virtual files.

use alloc::{string::String, sync::Arc};
use core::{cmp::min, fmt};

use iov_iter::IovIterDest;
use kpoll::IoEvents;
use ksync::Mutex;

use crate::{
    FileOperations, InodeOperations, Kiocb, Metadata, MetadataUpdate, NodePermission, NodeType,
    VfsError, VfsFile, VfsFileBuilder, VfsInodeInit, VfsResult,
    command::{self, CompleteCommandOptions, EmptyCommandPolicy},
    simple_fs::{SimpleFs, SimpleFsNode},
};

/// Internal buffer size for sequential virtual files.
pub(crate) const SEQ_BUF_SIZE: usize = 0x1000;

/// A large synthetic file size reported to userspace by legacy consumers.
pub(crate) const VIRTUAL_FILE_SIZE: u64 = 1024 * 1024;

/// A fallible text sink for one record.
///
/// Formatters see only `fmt::Write` and `try_reserve`; the concrete buffer
/// keeps the real VFS error private so a formatter cannot consume or clear it.
pub trait SeqTextSink: fmt::Write {
    /// Reserves space for at least `additional` more bytes.
    fn try_reserve(&mut self, additional: usize) -> VfsResult<()>;
}

struct RecordBuffer {
    data: String,
    max_bytes: usize,
    error: Option<VfsError>,
}

impl RecordBuffer {
    fn infallible(capacity: usize) -> Self {
        Self {
            data: String::with_capacity(capacity),
            max_bytes: usize::MAX,
            error: None,
        }
    }

    fn try_new(capacity: usize, max_bytes: usize) -> VfsResult<Self> {
        let mut data = String::new();
        data.try_reserve(capacity).map_err(|_| VfsError::NoMemory)?;
        Ok(Self {
            data,
            max_bytes,
            error: None,
        })
    }

    fn clear(&mut self) {
        self.data.clear();
        self.error = None;
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn take_error(&mut self) -> Option<VfsError> {
        self.error.take()
    }

    fn reserve(&mut self, additional: usize) -> VfsResult<()> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let Some(next) = self.data.len().checked_add(additional) else {
            let error = VfsError::FileTooLarge;
            self.error = Some(error);
            return Err(error);
        };
        if next > self.max_bytes {
            let error = VfsError::FileTooLarge;
            self.error = Some(error);
            return Err(error);
        }
        if self.data.try_reserve(additional).is_err() {
            let error = VfsError::NoMemory;
            self.error = Some(error);
            return Err(error);
        }
        Ok(())
    }
}

impl fmt::Write for RecordBuffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.reserve(value.len()).map_err(|_| fmt::Error)?;
        self.data.push_str(value);
        Ok(())
    }
}

impl SeqTextSink for RecordBuffer {
    fn try_reserve(&mut self, additional: usize) -> VfsResult<()> {
        self.reserve(additional)
    }
}

/// Iterator interface for proc-style files.
pub trait SeqIterator: Send + 'static {
    /// The item emitted on each step.
    ///
    /// The per-open failure retry keeps one failed item until the next read, so
    /// the item must be sendable with the file state.
    type Item: Send;

    /// Rewinds the iterator to the beginning.
    fn rewind(&mut self);

    /// Returns the first item.
    fn start(&mut self) -> Option<Self::Item>;

    /// Returns the next item.
    fn next(&mut self) -> Option<Self::Item>;

    /// Fallible first-item entry. `Ok(None)` is EOF; `Err` is an observation
    /// failure. The default preserves existing iterators.
    fn try_start(&mut self) -> VfsResult<Option<Self::Item>> {
        Ok(self.start())
    }

    /// Fallible next-item entry. `Ok(None)` is EOF; `Err` is a failure.
    fn try_next(&mut self) -> VfsResult<Option<Self::Item>> {
        Ok(self.next())
    }

    /// Formats the current item into `buf`.
    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result;

    /// Formats one record while preserving a filesystem-specific error.
    ///
    /// The default maps formatting failures to `VfsError::Io`. Dynamic files
    /// may override this to report errors such as a reclaimed process.
    fn try_show(&self, item: &Self::Item, buf: &mut String) -> VfsResult<()> {
        self.show(item, buf).map_err(|_| VfsError::Io)
    }

    /// Fallible formatting into the record buffer.
    ///
    /// The default preserves existing `try_show` consumers. A provider that
    /// wants capacity errors preserved should override this method.
    fn try_show_into(&self, item: &Self::Item, out: &mut dyn SeqTextSink) -> VfsResult<()> {
        let mut text = String::new();
        self.try_show(item, &mut text)?;
        out.try_reserve(text.len())?;
        out.write_str(&text).map_err(|_| VfsError::Io)
    }
}

/// Recovery state after the current buffer stopped at a record failure.
///
/// This is deliberately separate from buffer/offset/EOF state: only one
/// recovery action may be pending, and format failures retain their item while
/// observation failures make the iterator cursor untrusted.
enum ReadRecovery<T> {
    Ready,
    RetryItem(T),
    ReportThenReposition(VfsError),
    Reposition,
}

/// Failure returned by the record-production step.
enum RecordFailure<T> {
    Observe(VfsError),
    Format { item: T, error: VfsError },
}

impl<T> RecordFailure<T> {
    fn error(&self) -> VfsError {
        match self {
            Self::Observe(error) | Self::Format { error, .. } => *error,
        }
    }
}

/// Stateful adapter that manages buffering and read offsets.
pub struct SeqFile<I: SeqIterator> {
    iter: I,
    buf: RecordBuffer,
    recovery: ReadRecovery<I::Item>,
    buf_read_pos: usize,
    last_file_offset: u64,
    started: bool,
    is_eof: bool,
}

impl<I: SeqIterator> SeqFile<I> {
    /// Creates a new sequential file state with legacy infallible initial
    /// buffer allocation.
    pub fn new(iter: I) -> Self {
        Self::new_inner(iter, RecordBuffer::infallible(SEQ_BUF_SIZE))
    }

    /// Creates a sequential state with a fallible initial buffer.
    pub fn try_with_capacity(iter: I, capacity: usize, max_record_bytes: usize) -> VfsResult<Self> {
        let buf = RecordBuffer::try_new(capacity, max_record_bytes)?;
        Ok(Self::new_inner(iter, buf))
    }

    fn new_inner(iter: I, buf: RecordBuffer) -> Self {
        Self {
            iter,
            buf,
            recovery: ReadRecovery::Ready,
            buf_read_pos: 0,
            last_file_offset: 0,
            started: false,
            is_eof: false,
        }
    }

    /// Reads file content into `output` from `offset`.
    pub fn read(&mut self, output: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mut iter = iov_iter::iov_iter_kvec_dest(output);
        self.read_iter(offset, &mut iter)
    }

    /// Reads into an iterator destination. One call pins the same record
    /// buffer; it does not restart the iterator per page.
    pub fn read_iter(&mut self, offset: u64, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        self.prepare_offset(offset)?;

        let mut total = 0;
        while iter.count() != 0 {
            let available = self.buf.data.len().saturating_sub(self.buf_read_pos);
            if available == 0 {
                if self.is_eof {
                    break;
                }
                match self.fill_next_entry() {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(failure) => {
                        let error = failure.error();
                        if total == 0 {
                            self.record_failure(failure, false);
                            return Err(error);
                        }
                        self.record_failure(failure, true);
                        break;
                    }
                }
            }

            let want = min(available, iter.count());
            let copied = match iter.copy_to_iter(
                &self.buf.data.as_bytes()[self.buf_read_pos..self.buf_read_pos + want],
            ) {
                Ok(copied) => copied,
                Err(error) if total == 0 => return Err(error),
                Err(_) => break,
            };
            self.buf_read_pos += copied;
            self.last_file_offset += copied as u64;
            total += copied;
            if copied < want {
                break;
            }
        }
        Ok(total)
    }

    fn prepare_offset(&mut self, offset: u64) -> VfsResult<()> {
        if offset == 0 {
            self.reset();
            return Ok(());
        }

        match core::mem::replace(&mut self.recovery, ReadRecovery::Ready) {
            ReadRecovery::Ready => {}
            ReadRecovery::RetryItem(item) => {
                self.recovery = ReadRecovery::RetryItem(item);
            }
            ReadRecovery::ReportThenReposition(error) => {
                self.recovery = ReadRecovery::Reposition;
                return Err(error);
            }
            ReadRecovery::Reposition => {
                self.reposition_after_observation_error()?;
            }
        }

        if offset != self.last_file_offset {
            self.reset();
            self.seek_to(offset)?;
        }
        Ok(())
    }

    fn record_failure(&mut self, failure: RecordFailure<I::Item>, had_prefix: bool) {
        self.recovery = match failure {
            RecordFailure::Observe(error) if had_prefix => {
                ReadRecovery::ReportThenReposition(error)
            }
            RecordFailure::Observe(_) => ReadRecovery::Reposition,
            RecordFailure::Format { item, .. } => ReadRecovery::RetryItem(item),
        };
    }

    fn reposition_after_observation_error(&mut self) -> VfsResult<()> {
        let target = self.last_file_offset;
        self.iter.rewind();
        self.buf.clear();
        self.recovery = ReadRecovery::Ready;
        self.buf_read_pos = 0;
        self.last_file_offset = 0;
        self.started = false;
        self.is_eof = false;
        self.seek_to(target)
    }

    fn fill_next_entry(&mut self) -> Result<bool, RecordFailure<I::Item>> {
        if self.is_eof {
            return Ok(false);
        }

        self.buf.clear();
        self.buf_read_pos = 0;
        let next_item = match core::mem::replace(&mut self.recovery, ReadRecovery::Ready) {
            ReadRecovery::RetryItem(item) => Some(item),
            _ if !self.started => {
                self.started = true;
                match self.iter.try_start() {
                    Ok(item) => item,
                    Err(error) => {
                        self.buf.clear();
                        return Err(RecordFailure::Observe(error));
                    }
                }
            }
            _ => match self.iter.try_next() {
                Ok(item) => item,
                Err(error) => {
                    self.buf.clear();
                    return Err(RecordFailure::Observe(error));
                }
            },
        };

        match next_item {
            Some(item) => {
                let result = self.iter.try_show_into(&item, &mut self.buf);
                if let Some(error) = self.buf.take_error() {
                    self.buf.clear();
                    return Err(RecordFailure::Format { item, error });
                }
                if let Err(error) = result {
                    self.buf.clear();
                    return Err(RecordFailure::Format { item, error });
                }
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
            let available = self.buf.data.len().saturating_sub(self.buf_read_pos);
            if available == 0 {
                match self.fill_next_entry() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(failure) => {
                        let error = failure.error();
                        self.record_failure(failure, false);
                        return Err(error);
                    }
                }
            }

            let available = self.buf.data.len().saturating_sub(self.buf_read_pos);
            let to_skip = min((offset - self.last_file_offset) as usize, available);
            self.buf_read_pos += to_skip;
            self.last_file_offset += to_skip as u64;
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.iter.rewind();
        self.buf.clear();
        self.recovery = ReadRecovery::Ready;
        self.buf_read_pos = 0;
        self.last_file_offset = 0;
        self.started = false;
        self.is_eof = false;
    }
}

/// Installs a ready sequential file state on an open description.
pub fn seq_open<I: SeqIterator>(file: &mut VfsFileBuilder, iter: I) -> VfsResult<()> {
    file.set_private_data(Arc::new(Mutex::new(SeqFile::new(iter))));
    file.disable_pwrite();
    Ok(())
}

/// Seek behavior for a `SeqFileInode`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekEndPolicy {
    /// Reject `SEEK_END` with `EINVAL`; the inode does not expose a synthesized
    /// size.
    Reject,
    /// Preserve the legacy `file.size()` based `SEEK_END`.
    Legacy,
}

/// Per-open command side of a readable+writable sequential file.
pub type SeqValidateFn = dyn Fn() -> VfsResult<()> + Send + Sync;
pub type SeqWriteFn = dyn Fn(&VfsFile, &[u8]) -> VfsResult<()> + Send + Sync;

pub struct SeqCommandOptions {
    /// Maximum complete command size.
    pub max_command_bytes: usize,
    /// Optional pre-write validation.
    pub validate_write: Option<Arc<SeqValidateFn>>,
    /// Provider callback. Exactly one complete command is passed.
    pub write: Arc<SeqWriteFn>,
    /// Empty-command policy.
    pub empty: EmptyCommandPolicy,
}

impl Clone for SeqCommandOptions {
    fn clone(&self) -> Self {
        Self {
            max_command_bytes: self.max_command_bytes,
            validate_write: self.validate_write.clone(),
            write: self.write.clone(),
            empty: self.empty,
        }
    }
}

/// Options for `SeqFileInode`.
pub struct SeqFileOptions {
    /// Mode bits for the regular inode.
    pub permission: NodePermission,
    /// Do not create the iterator until the first read.
    pub lazy_open: bool,
    /// Maximum one-record size; `usize::MAX` preserves legacy consumers.
    pub max_record_bytes: usize,
    /// Reported inode size.
    pub reported_size: u64,
    /// `SEEK_END` behavior.
    pub seek_end: SeekEndPolicy,
    /// Open-time identity validation; must not generate content.
    pub validate_open: Option<Arc<SeqValidateFn>>,
    /// Per-read identity validation.
    pub validate_read: Option<Arc<SeqValidateFn>>,
    /// Optional command side.
    pub command: Option<SeqCommandOptions>,
}

impl Clone for SeqFileOptions {
    fn clone(&self) -> Self {
        Self {
            permission: self.permission,
            lazy_open: self.lazy_open,
            max_record_bytes: self.max_record_bytes,
            reported_size: self.reported_size,
            seek_end: self.seek_end,
            validate_open: self.validate_open.clone(),
            validate_read: self.validate_read.clone(),
            command: self.command.clone(),
        }
    }
}

impl SeqFileOptions {
    fn legacy() -> Self {
        Self {
            permission: NodePermission::from_bits_truncate(0o444),
            lazy_open: false,
            max_record_bytes: usize::MAX,
            reported_size: VIRTUAL_FILE_SIZE,
            seek_end: SeekEndPolicy::Legacy,
            validate_open: None,
            validate_read: None,
            command: None,
        }
    }
}

struct SeqFileState<I: SeqIterator> {
    make_iter: Option<Arc<dyn Fn() -> VfsResult<I> + Send + Sync>>,
    seq: Option<SeqFile<I>>,
    max_record_bytes: usize,
}

impl<I: SeqIterator> SeqFileState<I> {
    fn ready(iter: I, max_record_bytes: usize) -> VfsResult<Self> {
        let seq = if max_record_bytes == usize::MAX {
            SeqFile::new(iter)
        } else {
            SeqFile::try_with_capacity(iter, SEQ_BUF_SIZE, max_record_bytes)?
        };
        Ok(Self {
            make_iter: None,
            seq: Some(seq),
            max_record_bytes,
        })
    }

    fn lazy(
        make_iter: Arc<dyn Fn() -> VfsResult<I> + Send + Sync>,
        max_record_bytes: usize,
    ) -> Self {
        Self {
            make_iter: Some(make_iter),
            seq: None,
            max_record_bytes,
        }
    }

    fn ensure(&mut self) -> VfsResult<&mut SeqFile<I>> {
        if self.seq.is_none() {
            let make_iter = self
                .make_iter
                .as_ref()
                .ok_or(VfsError::InvalidInput)?
                .clone();
            let iter = make_iter()?;
            let seq = if self.max_record_bytes == usize::MAX {
                SeqFile::new(iter)
            } else {
                SeqFile::try_with_capacity(iter, SEQ_BUF_SIZE, self.max_record_bytes)?
            };
            self.seq = Some(seq);
        }
        self.seq.as_mut().ok_or(VfsError::InvalidInput)
    }
}

/// File inode wrapper for [`SeqFile`].
pub struct SeqFileInode<I: SeqIterator> {
    node: SimpleFsNode,
    make_iter: Arc<dyn Fn() -> VfsResult<I> + Send + Sync>,
    options: SeqFileOptions,
}

impl<I: SeqIterator> SeqFileInode<I> {
    /// Creates a read-only regular sequential file with legacy eager-open
    /// iterator creation.
    pub fn new_regular(
        fs: Arc<SimpleFs>,
        make_iter: impl Fn() -> VfsResult<I> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::new_regular_with_options(fs, make_iter, SeqFileOptions::legacy())
    }

    /// Creates a sequential file with explicit lazy/open/command behavior.
    pub fn new_regular_with_options(
        fs: Arc<SimpleFs>,
        make_iter: impl Fn() -> VfsResult<I> + Send + Sync + 'static,
        options: SeqFileOptions,
    ) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(fs, NodeType::RegularFile, options.permission),
            make_iter: Arc::new(make_iter),
            options,
        })
    }

    /// Returns the inode fields used when materializing this sequential file.
    pub fn inode_init(&self) -> VfsInodeInit {
        self.node.inode_init().with_size(self.options.reported_size)
    }

    fn validate_read(&self) -> VfsResult<()> {
        if let Some(validate) = &self.options.validate_read {
            validate()?;
        }
        Ok(())
    }

    fn command(&self) -> VfsResult<&SeqCommandOptions> {
        self.options
            .command
            .as_ref()
            .ok_or(VfsError::BadFileDescriptor)
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
        metadata.size = self.options.reported_size;
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
        if let Some(validate) = &self.options.validate_open {
            validate()?;
        }

        let state = if self.options.lazy_open {
            SeqFileState::lazy(self.make_iter.clone(), self.options.max_record_bytes)
        } else {
            let iter = (self.make_iter)()?;
            SeqFileState::ready(iter, self.options.max_record_bytes)?
        };
        file.set_private_data(Arc::new(Mutex::new(state)));
        if self.options.command.is_none() {
            file.disable_pwrite();
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        self.options.command.is_some()
    }

    /// Implements the poll operation every `/proc` file carries.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(IoEvents::IN | IoEvents::OUT)
    }

    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        if whence == crate::file::SEEK_END && self.options.seek_end == SeekEndPolicy::Reject {
            return Err(VfsError::InvalidInput);
        }
        let mut position = file.position_lock();
        let base = match whence {
            crate::file::SEEK_SET => 0,
            crate::file::SEEK_CUR => {
                i64::try_from(*position).map_err(|_| VfsError::InvalidInput)?
            }
            crate::file::SEEK_END => {
                i64::try_from(file.size()).map_err(|_| VfsError::InvalidInput)?
            }
            _ => return Err(VfsError::InvalidInput),
        };
        let new_offset = base.checked_add(offset).ok_or(VfsError::InvalidInput)?;
        if new_offset < 0 {
            return Err(VfsError::InvalidInput);
        }
        *position = new_offset as u64;
        Ok(new_offset as u64)
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.validate_read()?;
        let state = file
            .private_data_get::<Mutex<SeqFileState<I>>>()
            .ok_or(VfsError::InvalidInput)?;
        state.lock().ensure()?.read(buf, offset)
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        self.validate_read()?;
        let state = iocb
            .file()
            .private_data_get::<Mutex<SeqFileState<I>>>()
            .ok_or(VfsError::InvalidInput)?;
        let read = state.lock().ensure()?.read_iter(iocb.ki_pos(), iter)?;
        iocb.advance(read);
        Ok(read)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let command = self.command()?;
        if let Some(validate) = &command.validate_write {
            validate()?;
        }
        if buf.len() > command.max_command_bytes {
            return Err(VfsError::FileTooLarge);
        }
        if buf.is_empty() && command.empty == EmptyCommandPolicy::NoOp {
            return Ok(0);
        }
        (command.write)(file, buf)?;
        Ok(buf.len())
    }

    fn write_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut iov_iter::IovIterSource<'_>,
    ) -> VfsResult<usize> {
        if iter.count() == 0 {
            return Ok(0);
        }
        let command = self.command()?;
        if let Some(validate) = &command.validate_write {
            validate()?;
        }
        let count = iter.count();
        let Some(data) = command::copy_complete_command(
            iter,
            CompleteCommandOptions {
                max_bytes: command.max_command_bytes,
                empty: command.empty,
            },
        )?
        else {
            return Ok(0);
        };
        let written = match (command.write)(iocb.file(), &data) {
            Ok(()) => data.len(),
            Err(error) => {
                iter.revert(count)?;
                return Err(error);
            }
        };
        if written != count {
            iter.revert(count)?;
            return Err(VfsError::InvalidData);
        }
        // Command writes are independent of the read cursor. The complete
        // command is the write boundary; it must not move f_pos.
        Ok(written)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/seq_file.rs");
