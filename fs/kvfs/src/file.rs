// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open-file objects.
//!
//! `VfsFile` owns opened-file state: `f_path`, `f_inode`, `f_mapping`,
//! `f_op`, `f_flags`, `f_pos`, and `private_data`.

use alloc::sync::Arc;
use core::{
    any::Any,
    sync::atomic::{AtomicU32, Ordering},
    task::Context,
};

use iov_iter::{IovIterDest, IovIterSource, iov_iter_kvec_dest, iov_iter_kvec_source};
use kerrno::LinuxError;
use kpoll::IoEvents;
use linux_raw_sys::general::{
    O_ACCMODE, O_APPEND, O_CREAT, O_DIRECT, O_EXCL, O_NOCTTY, O_NONBLOCK, O_PATH, O_TRUNC,
};
use log::warn;
use pagecache::Mapping;

use crate::{
    AddressSpace, DirContext, Kiocb, MagicLinkOps, MmapMapper, Mutex, MutexGuard, NodeFlags,
    NodeType, Path, TypeMap, VfsError, VfsInode, VfsResult,
};

bitflags::bitflags! {
    /// Open-file mode flags stored in `VfsFile::f_mode`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FMode: u32 {
        /// File may be read.
        const READ = 1 << 0;
        /// File may be written.
        const WRITE = 1 << 1;
        /// File supports `llseek`.
        const LSEEK = 1 << 2;
        /// File supports positioned reads.
        const PREAD = 1 << 3;
        /// File supports positioned writes.
        const PWRITE = 1 << 4;
        /// File was opened for execution.
        const EXEC = 1 << 5;
        /// Path-only file descriptor.
        const PATH = 1 << 14;
        /// File serializes position updates.
        const ATOMIC_POS = 1 << 15;
        /// File holds write access.
        const WRITER = 1 << 16;
        /// File operation table can read.
        const CAN_READ = 1 << 17;
        /// File operation table can write.
        const CAN_WRITE = 1 << 18;
        /// File has completed open.
        const OPENED = 1 << 19;
        /// File was created by this open.
        const CREATED = 1 << 20;
        /// Read/write operations do not consume `f_pos`.
        const STREAM = 1 << 21;
        /// File supports direct I/O.
        const CAN_ODIRECT = 1 << 22;
        /// File is not globally accounted.
        const NOACCOUNT = 1 << 29;
    }
}

impl FMode {
    /// Converts open access-mode bits to `file::f_mode` bits.
    pub fn from_open_flags(flags: u32) -> Self {
        Self::from_bits_truncate(flags.wrapping_add(1) & O_ACCMODE)
    }
}

pub(crate) struct EmptyFileOperations;

/// `SEEK_SET`.
pub const SEEK_SET: i32 = 0;
/// `SEEK_CUR`.
pub const SEEK_CUR: i32 = 1;
/// `SEEK_END`.
pub const SEEK_END: i32 = 2;

/// Directory iteration operations installed on opened directory files.
pub trait FileDirOperations: Send + Sync {
    /// Iterates directory entries for this open file description.
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize>;
}

/// File operations installed on file-capable inodes.
///
/// The same operation table is attached to the inode and used by opened file
/// descriptions. Implementations that need per-open behavior may update the
/// [`VfsFileBuilder`] during `open`.
pub trait FileOperations: Send + Sync + 'static {
    /// Returns directory iteration operations when this file supports readdir.
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        None
    }

    /// Opens a file description backed by this operation table.
    fn open(self: Arc<Self>, _inode: &VfsInode, _file: &mut VfsFileBuilder) -> VfsResult<()> {
        Ok(())
    }

    /// Returns whether this operation table provides read callbacks.
    fn supports_read(&self) -> bool {
        false
    }

    /// Returns whether this operation table provides write callbacks.
    fn supports_write(&self) -> bool {
        false
    }

    /// Changes this open file description's file offset.
    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        if file.is_stream() || matches!(file.inode().node_type(), NodeType::Fifo | NodeType::Socket)
        {
            return Err(VfsError::from(LinuxError::ESPIPE));
        }

        let mut position = file.position_lock();
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => *position as i64,
            SEEK_END => file.inode().size() as i64,
            _ => return Err(VfsError::InvalidInput),
        };
        let new_offset = base.checked_add(offset).ok_or(VfsError::InvalidInput)?;
        if new_offset < 0 {
            return Err(VfsError::InvalidInput);
        }
        let new_offset = new_offset as u64;
        *position = new_offset;
        Ok(new_offset)
    }

    /// Reads file data for this open file description.
    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    /// Reads file data into an iterator destination.
    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; memaddr::PAGE_SIZE_4K];
        while iter.count() != 0 {
            let want = chunk.len().min(iter.count());
            if want == 0 {
                break;
            }
            let read = self.read(iocb.file(), &mut chunk[..want], iocb.ki_pos())?;
            if read == 0 {
                break;
            }
            let copied = iter.copy_to_iter(&chunk[..read])?;
            total += copied;
            iocb.advance(copied);
            if copied < read || read < want {
                break;
            }
        }
        Ok(total)
    }

    /// Writes file data for this open file description.
    fn write(&self, _file: &VfsFile, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    /// Writes file data from an iterator source.
    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; memaddr::PAGE_SIZE_4K];
        while iter.count() != 0 {
            let want = chunk.len().min(iter.count());
            if want == 0 {
                break;
            }
            let copied = iter.copy_from_iter(&mut chunk[..want])?;
            if copied == 0 {
                break;
            }
            let written = self.write(iocb.file(), &chunk[..copied], iocb.ki_pos())?;
            if written == 0 {
                return Err(VfsError::WriteZero);
            }
            total += written;
            iocb.advance(written);
            iocb.file().update_size_after_write(iocb.ki_pos())?;
            if written < copied {
                break;
            }
        }
        Ok(total)
    }

    /// Manipulates the underlying device parameters of special files.
    fn ioctl(&self, _file: &VfsFile, _cmd: u32, _arg: usize) -> VfsResult<usize> {
        Err(VfsError::NotATty)
    }

    /// Handles mmap for this file through the provided mapper.
    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        if file.inode().node_type() == NodeType::RegularFile {
            mapper.map_file_backed()
        } else {
            Err(VfsError::NoSuchDevice)
        }
    }

    /// Returns magic-link operations when this inode has magic-link
    /// follow semantics.
    fn magic_link(self: Arc<Self>) -> Option<Arc<dyn MagicLinkOps>> {
        None
    }

    /// Flushes an open file description.
    fn flush(&self, _file: &VfsFile) -> VfsResult<()> {
        Ok(())
    }

    /// Releases this open file description.
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        Ok(())
    }

    /// Synchronizes this file's data and optionally metadata.
    fn fsync(&self, _file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    /// Applies filesystem-specific space allocation or range-shift operations.
    fn fallocate(&self, _file: &VfsFile, _mode: u32, _offset: u64, _len: u64) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }

    /// Polls an open file description.
    fn poll(&self, _file: &VfsFile) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    /// Registers a poll waiter for an open file description.
    fn register_poll(
        &self,
        _file: &VfsFile,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) {
    }
}

impl FileOperations for EmptyFileOperations {}

/// Path-derived `struct file` location state.
///
/// `f_path`, `f_inode`, and `f_mapping` are bound together by VFS open logic;
/// callers should not install or update them as independent slots.
#[derive(Clone)]
struct FileLocation {
    path: Path,
    inode: Arc<VfsInode>,
    mapping: Arc<AddressSpace>,
}

impl FileLocation {
    fn from_path(path: Path) -> Self {
        let inode = path.inode();
        let mapping = inode.address_space();
        Self {
            path,
            inode,
            mapping,
        }
    }

    fn cloned_from(file: &VfsFile) -> Self {
        file.location.clone()
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn inode(&self) -> &Arc<VfsInode> {
        &self.inode
    }

    fn mapping(&self) -> Arc<AddressSpace> {
        self.mapping.clone()
    }
}

fn default_direct(path: &Path, open_flags: u32) -> bool {
    let inode = path.inode();
    let direct = !path.is_dir()
        && (open_flags & O_DIRECT != 0
            || open_flags & O_PATH != 0
            || matches!(
                inode.node_type(),
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            )
            || inode.flags().contains(NodeFlags::NON_CACHEABLE));
    direct && !inode.flags().contains(NodeFlags::ALWAYS_CACHE)
}

/// Open-file state while the open path is still installing final fields.
pub struct VfsFileBuilder {
    mode: FMode,
    operations: Option<Arc<dyn FileOperations>>,
    location: Option<FileLocation>,
    private_data: TypeMap,
    flags: u32,
    position: u64,
}

impl VfsFileBuilder {
    /// Creates an unbound open-file builder.
    pub(crate) fn empty(flags: FMode, open_flags: u32) -> Self {
        Self {
            mode: flags,
            operations: None,
            location: None,
            private_data: TypeMap::default(),
            flags: open_flags,
            position: 0,
        }
    }

    pub(crate) fn allocate(open_flags: u32) -> VfsResult<Self> {
        Ok(Self::empty(FMode::from_open_flags(open_flags), open_flags))
    }

    pub(crate) fn from_path_state(
        path: Path,
        flags: FMode,
        open_flags: u32,
        f_op: Arc<dyn FileOperations>,
    ) -> Self {
        let mut file = Self::empty(flags, open_flags);
        file.bind_location(FileLocation::from_path(path));
        file.install_operations(f_op);
        file
    }

    pub(crate) fn cloned_from(
        base: &VfsFile,
        flags: FMode,
        open_flags: u32,
        f_op: Arc<dyn FileOperations>,
    ) -> Self {
        let mut file = Self::empty(flags, open_flags);
        file.bind_location(FileLocation::cloned_from(base));
        file.install_operations(f_op);
        file
    }

    pub(crate) fn mark_opened(&mut self) -> VfsResult<()> {
        self.mode.remove(FMode::CAN_READ | FMode::CAN_WRITE);
        let operations = self.operations.as_ref().ok_or(VfsError::InvalidInput)?;
        if self.mode.contains(FMode::READ) && operations.supports_read() {
            self.mode.insert(FMode::CAN_READ);
        }
        if self.mode.contains(FMode::WRITE) && operations.supports_write() {
            self.mode.insert(FMode::CAN_WRITE);
        }
        self.mode |= FMode::OPENED;
        Ok(())
    }

    pub(crate) fn inode(&self) -> VfsResult<&Arc<VfsInode>> {
        Ok(self.location()?.inode())
    }

    pub(crate) fn mark_created(&mut self) {
        self.mode.insert(FMode::CREATED);
    }

    pub(crate) fn clear_created(&mut self) {
        self.mode.remove(FMode::CREATED);
    }

    pub(crate) fn was_created(&self) -> bool {
        self.mode.contains(FMode::CREATED)
    }

    pub(crate) fn vfs_open(mut self, path: Path) -> VfsResult<Arc<VfsFile>> {
        self.bind_location(FileLocation::from_path(path));
        self.do_dentry_open()
    }

    fn do_dentry_open(mut self) -> VfsResult<Arc<VfsFile>> {
        let location = self.location()?.clone();

        if self.flags & O_PATH != 0 {
            self.mode = FMode::PATH;
            self.install_operations(Arc::new(EmptyFileOperations));
            self.mark_opened()?;
            return self.finish();
        }

        if matches!(
            location.inode().node_type(),
            NodeType::RegularFile | NodeType::Directory
        ) {
            self.mode.insert(FMode::ATOMIC_POS);
        }
        self.mode
            .insert(FMode::LSEEK | FMode::PREAD | FMode::PWRITE);

        let operations = location.inode().open_file_operations().clone();
        self.install_operations(operations.clone());
        if default_direct(location.path(), self.flags) {
            self.mode.insert(FMode::CAN_ODIRECT);
        }
        operations.open(location.inode().as_ref(), &mut self)?;
        self.flags &= !(O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC);
        self.mark_opened()?;
        self.finish()
    }

    fn bind_location(&mut self, location: FileLocation) {
        self.location = Some(location);
    }

    fn location(&self) -> VfsResult<&FileLocation> {
        self.location.as_ref().ok_or(VfsError::InvalidInput)
    }

    fn install_operations(&mut self, operations: Arc<dyn FileOperations>) {
        self.operations = Some(operations);
    }

    /// Replaces the file operation table during open.
    pub fn replace_fops(&mut self, f_op: Arc<dyn FileOperations>) {
        self.install_operations(f_op);
    }

    /// Marks this open file description as a stream.
    pub fn stream_open(&mut self) {
        self.mode &= !(FMode::LSEEK | FMode::PREAD | FMode::PWRITE | FMode::ATOMIC_POS);
        self.mode |= FMode::STREAM;
    }

    pub(crate) fn disable_pwrite(&mut self) {
        self.mode &= !FMode::PWRITE;
    }

    /// Sets file-private data for the open file being built.
    pub fn set_private_data<T>(&mut self, value: Arc<T>)
    where
        T: Any + Send + Sync + 'static,
    {
        self.private_data.insert_arc(value);
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&mut self, flag: bool) {
        if flag {
            self.flags |= O_NONBLOCK;
        } else {
            self.flags &= !O_NONBLOCK;
        }
    }

    pub(crate) fn finish(self) -> VfsResult<Arc<VfsFile>> {
        let Self {
            mode,
            operations,
            location,
            private_data,
            flags,
            position,
        } = self;

        let operations = operations.ok_or(VfsError::InvalidInput)?;
        let location = location.ok_or(VfsError::InvalidInput)?;
        Ok(Arc::new(VfsFile {
            mode: AtomicU32::new(mode.bits()),
            operations,
            location,
            private_data: Mutex::new(private_data),
            flags: AtomicU32::new(flags),
            position: Mutex::new(position),
        }))
    }
}

/// VFS-owned open-file state.
pub struct VfsFile {
    mode: AtomicU32,
    operations: Arc<dyn FileOperations>,
    location: FileLocation,
    private_data: Mutex<TypeMap>,
    flags: AtomicU32,
    position: Mutex<u64>,
}

impl VfsFile {
    /// Allocates an opened file sharing the path and mapping of another file.
    pub fn alloc_clone(
        &self,
        flags: FMode,
        open_flags: u32,
        f_op: Arc<dyn FileOperations>,
    ) -> VfsResult<Arc<Self>> {
        let mut file = VfsFileBuilder::cloned_from(self, flags, open_flags, f_op);
        file.mark_opened()?;
        file.finish()
    }

    /// Allocates a cloned open file with typed private data installed.
    pub fn alloc_clone_with_private_data<T>(
        &self,
        flags: FMode,
        open_flags: u32,
        f_op: Arc<dyn FileOperations>,
        private_data: Arc<T>,
    ) -> VfsResult<Arc<Self>>
    where
        T: Any + Send + Sync + 'static,
    {
        let mut file = VfsFileBuilder::cloned_from(self, flags, open_flags, f_op);
        file.set_private_data(private_data);
        file.mark_opened()?;
        file.finish()
    }

    /// Returns this file's `f_path`.
    pub fn path(&self) -> &Path {
        self.location.path()
    }

    /// Returns the inode backing this open file description.
    pub fn inode(&self) -> &Arc<VfsInode> {
        self.location.inode()
    }

    /// Returns this file's node type.
    pub fn node_type(&self) -> NodeType {
        self.inode().node_type()
    }

    /// Returns whether this file's inode is a directory.
    pub fn is_dir(&self) -> bool {
        self.node_type() == NodeType::Directory
    }

    /// Returns whether this file's inode is a regular file.
    pub fn is_regular_file(&self) -> bool {
        self.node_type() == NodeType::RegularFile
    }

    /// Returns this file's inode size.
    pub fn size(&self) -> u64 {
        self.inode().size()
    }

    pub(crate) fn mapping(&self) -> Arc<AddressSpace> {
        self.location.mapping()
    }

    /// Returns the page-cache object backing file mappings.
    pub fn page_cache(&self) -> Arc<Mapping> {
        self.mapping().page_cache()
    }

    /// Writes all dirty cached pages for this file.
    pub fn writeback_mapping(&self, data_only: bool) -> VfsResult<()> {
        self.mapping().writepages(data_only)
    }

    /// Writes dirty cached pages intersecting `[start, start + len)`.
    pub fn writeback_mapping_range(
        &self,
        start: u64,
        len: usize,
        data_only: bool,
    ) -> VfsResult<()> {
        self.mapping().writepages_range(start, len, data_only)
    }

    /// Returns the maximum regular-file size allowed for this open file.
    pub fn max_file_size(&self) -> u64 {
        self.path().max_file_size()
    }

    /// Synchronizes the filesystem containing this open file.
    pub fn sync_filesystem(&self) -> VfsResult<()> {
        self.path().sync_filesystem()
    }

    fn operations(&self) -> &dyn FileOperations {
        self.operations.as_ref()
    }

    /// Returns this file's access mode flags.
    pub fn mode(&self) -> FMode {
        FMode::from_bits_truncate(self.mode.load(Ordering::Acquire))
    }

    /// Returns whether this file descriptor is path-only.
    pub fn is_path(&self) -> bool {
        self.mode().contains(FMode::PATH)
    }

    /// Returns whether read/write should ignore this file's `f_pos`.
    pub fn is_stream(&self) -> bool {
        self.mode().contains(FMode::STREAM)
    }

    /// Checks that the file has the requested access mode.
    pub fn verify_mode(&self, flags: FMode) -> VfsResult<()> {
        if self.mode().contains(flags) && !self.is_path() {
            Ok(())
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    fn verify_io_area(&self, pos: u64, count: usize) -> VfsResult<()> {
        if count > isize::MAX as usize {
            return Err(VfsError::InvalidInput);
        }
        let count = u64::try_from(count).map_err(|_| VfsError::InvalidInput)?;
        pos.checked_add(count).ok_or(VfsError::InvalidInput)?;
        if self.inode().node_type() == NodeType::Directory {
            return Err(VfsError::IsADirectory);
        }
        Ok(())
    }

    fn verify_read_area(&self, pos: u64, count: usize) -> VfsResult<()> {
        self.verify_mode(FMode::READ)?;
        if !self.mode().contains(FMode::CAN_READ) {
            return Err(VfsError::InvalidInput);
        }
        self.verify_io_area(pos, count)
    }

    fn verify_write_area(&self, pos: u64, count: usize) -> VfsResult<()> {
        self.verify_mode(FMode::WRITE)?;
        if !self.mode().contains(FMode::CAN_WRITE) {
            return Err(VfsError::InvalidInput);
        }
        self.verify_io_area(pos, count)?;
        self.path().check_writable_mount()
    }

    /// Reads from this open file and advances `f_pos` when applicable.
    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let mut pos = self.position_lock();
        self.read_from(buf, &mut pos)
    }

    /// Reads from this open file into an iterator destination and advances `f_pos`.
    pub fn read_iter(&self, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let mut pos = self.position_lock();
        self.read_iter_from(iter, &mut pos)
    }

    /// Reads from this open file using a caller-owned position.
    pub fn read_from(&self, buf: &mut [u8], pos: &mut u64) -> VfsResult<usize> {
        let mut iter = iov_iter_kvec_dest(buf);
        self.read_iter_from(&mut iter, pos)
    }

    /// Reads from this open file into an iterator destination using a caller-owned position.
    pub fn read_iter_from(&self, iter: &mut IovIterDest<'_>, pos: &mut u64) -> VfsResult<usize> {
        if self.is_stream() {
            self.verify_read_area(0, iter.count())?;
            let mut iocb = Kiocb::new(self, 0);
            self.operations().read_iter(&mut iocb, iter)
        } else {
            self.verify_read_area(*pos, iter.count())?;
            let mut iocb = Kiocb::new(self, *pos);
            let read = self.operations().read_iter(&mut iocb, iter)?;
            *pos = iocb.ki_pos();
            Ok(read)
        }
    }

    /// Writes to this open file and advances `f_pos` when applicable.
    pub fn write(&self, buf: &[u8]) -> VfsResult<usize> {
        let mut pos = self.position_lock();
        self.write_from(buf, &mut pos)
    }

    /// Writes to this open file using a caller-owned position.
    pub fn write_from(&self, buf: &[u8], pos: &mut u64) -> VfsResult<usize> {
        let mut iter = iov_iter_kvec_source(buf);
        self.write_iter_from(&mut iter, pos)
    }

    /// Writes to this open file from an iterator source and advances `f_pos`.
    pub fn write_iter(&self, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        let mut pos = self.position_lock();
        self.write_iter_from(iter, &mut pos)
    }

    /// Writes to this open file from an iterator source using a caller-owned position.
    pub fn write_iter_from(&self, iter: &mut IovIterSource<'_>, pos: &mut u64) -> VfsResult<usize> {
        if self.is_stream() {
            self.verify_write_area(0, iter.count())?;
            let mut iocb = Kiocb::new(self, 0);
            self.operations().write_iter(&mut iocb, iter)
        } else if self.flags() & O_APPEND != 0 {
            let end = self.inode().size();
            self.verify_write_area(end, iter.count())?;
            let mut iocb = Kiocb::new(self, end);
            let written = self.operations().write_iter(&mut iocb, iter)?;
            if written != 0 {
                *pos = iocb.ki_pos();
            }
            Ok(written)
        } else {
            self.verify_write_area(*pos, iter.count())?;
            let mut iocb = Kiocb::new(self, *pos);
            let written = self.operations().write_iter(&mut iocb, iter)?;
            if written != 0 {
                *pos = iocb.ki_pos();
            }
            Ok(written)
        }
    }

    pub(crate) fn update_size_after_write(&self, end: u64) -> VfsResult<()> {
        if self.is_stream() || self.inode().node_type() != NodeType::RegularFile {
            return Ok(());
        }

        let inode = self.inode();
        if end > inode.size() {
            inode.update_size_after_backing_change(end)?;
        }
        Ok(())
    }

    /// Runs directory iteration for this open file description.
    pub fn iterate_dir(&self, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        if self.inode().node_type() != NodeType::Directory {
            return Err(VfsError::NotADirectory);
        }
        self.verify_mode(FMode::READ)?;

        ctx.set_pos(self.position());
        let directory = self
            .operations()
            .dir_operations()
            .ok_or(VfsError::NotADirectory)?;
        let result = directory.iterate_shared(self, ctx);
        self.set_position(ctx.pos());
        result
    }

    /// Returns whether the underlying node is always blocking.
    ///
    /// This is distinct from the open-file `O_NONBLOCK` state. Regular-file
    /// operations are blocking at the inode operation boundary.
    pub fn is_blocking(&self) -> bool {
        self.inode().flags().contains(NodeFlags::BLOCKING)
    }

    /// Locks this file's `f_pos`.
    pub fn position_lock(&self) -> MutexGuard<'_, u64> {
        self.position.lock()
    }

    /// Returns this file's `f_pos`.
    pub fn position(&self) -> u64 {
        *self.position.lock()
    }

    /// Sets this file's `f_pos`.
    pub fn set_position(&self, position: u64) {
        *self.position_lock() = position;
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&self, flag: bool) {
        if flag {
            self.flags.fetch_or(O_NONBLOCK, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!O_NONBLOCK, Ordering::AcqRel);
        }
    }

    /// Returns the open-file nonblocking flag.
    pub fn is_nonblocking(&self) -> bool {
        self.flags() & O_NONBLOCK != 0
    }

    /// Returns this file's `f_flags`.
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// Replaces selected open-file status flags.
    pub fn replace_flags(&self, mask: u32, flags: u32) {
        let _ = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some((flags & mask) | (current & !mask))
            });
    }

    pub(crate) fn set_private_data<T>(&self, value: Arc<T>)
    where
        T: Any + Send + Sync + 'static,
    {
        self.private_data.lock().insert_arc(value);
    }

    /// Returns typed file-private data attached to this open file.
    pub fn private_data_get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.private_data.lock().get::<T>()
    }

    /// Closes this file reference.
    pub fn close_file(self: Arc<Self>) -> VfsResult<()> {
        self.flush()
    }

    /// Flushes this open file.
    pub fn flush(&self) -> VfsResult<()> {
        self.operations().flush(self)
    }

    /// Synchronizes this open file.
    pub fn fsync(&self, data_only: bool) -> VfsResult<()> {
        self.operations().fsync(self, data_only)
    }

    /// Changes this open file offset.
    pub fn llseek(&self, offset: i64, whence: i32) -> VfsResult<u64> {
        if !self.mode().contains(FMode::LSEEK) {
            return Err(VfsError::from(LinuxError::ESPIPE));
        }
        self.operations().llseek(self, offset, whence)
    }

    /// Dispatches an ioctl to this open file.
    pub fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.operations().ioctl(self, cmd, arg)
    }

    /// Creates a mapping for this open file.
    pub fn mmap(&self, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.operations().mmap(self, mapper)
    }

    /// Applies filesystem-specific space allocation or range-shift operations.
    pub fn fallocate(&self, mode: u32, offset: u64, len: u64) -> VfsResult<()> {
        self.operations().fallocate(self, mode, offset, len)
    }

    /// Polls this open file.
    pub fn poll(&self) -> IoEvents {
        self.operations().poll(self)
    }

    /// Registers a poll waiter for this open file.
    pub fn register_poll(&self, context: &mut Context<'_>, events: IoEvents) {
        self.operations().register_poll(self, context, events)
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        let old_mode = self.mode.fetch_and(!FMode::OPENED.bits(), Ordering::AcqRel);
        let release_result = if FMode::from_bits_truncate(old_mode).contains(FMode::OPENED) {
            self.operations().release(self.inode(), self)
        } else {
            Ok(())
        };

        if let Err(err) = release_result {
            let path = self
                .path()
                .display_path()
                .unwrap_or_else(|_| "<error>".into());
            warn!("failed to release VFS file {}: {err:?}", path);
        }
    }
}
