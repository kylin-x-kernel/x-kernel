// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open-file objects.
//!
//! `VfsFile` owns opened-file state: `f_path`, `f_inode`, `f_mapping`,
//! `f_op`, `f_flags`, `f_pos`, `f_pipe`, and `private_data`.

use alloc::{string::String, sync::Arc};
use core::{
    any::Any,
    ops::Range,
    sync::atomic::{AtomicU32, Ordering},
};

use iov_iter::{IovIterDest, IovIterSource, iov_iter_kvec_dest, iov_iter_kvec_source};
use kcred::Cred;
use kerrno::{KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, WeakPollSet};
use linux_raw_sys::general::O_ACCMODE;
use log::warn;
use pagecache::{FolioGuard, PageIndex};

use crate::{
    AddressSpace, DirContext, FallocateFlags, FileSlot, FileSlots, Kiocb, MagicLinkOps, MmapMapper,
    Mutex, MutexGuard, NodeFlags, NodeType, OpenFlags, Path, TypeMap, VfsError, VfsInode,
    VfsResult,
};

/// A captured, event-filtered wait source independent of current file readiness.
///
/// Retains neither the source owner nor the file. An expired source stays bound
/// to its original identity and does not synthesize readiness or hangup events.
#[derive(Clone)]
pub struct FilePollSource {
    source: WeakPollSet,
    events: IoEvents,
}

impl FilePollSource {
    /// Creates a single-source binding without allocating or registering.
    pub fn new(source: WeakPollSet, events: IoEvents) -> Self {
        Self { source, events }
    }

    /// Registers only when the requested events intersect this source's events.
    ///
    /// Readiness must still be checked through the file. Call without locks that
    /// waker callbacks could reenter, including when the source is being retired.
    ///
    /// # Errors
    /// Propagates registration errors for a live, matching source. An expired
    /// source or disjoint event mask succeeds without registering or allocating.
    pub fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(self.events) {
            context.register_weak(&self.source)?;
        }
        Ok(())
    }
}

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
    pub fn from_open_flags(flags: OpenFlags) -> Self {
        Self::from_bits_truncate(flags.bits().wrapping_add(1) & O_ACCMODE)
    }
}

pub(crate) struct EmptyFileOperations;

/// `SEEK_SET`.
pub(crate) const SEEK_SET: i32 = 0;
/// `SEEK_CUR`.
pub(crate) const SEEK_CUR: i32 = 1;
/// `SEEK_END`.
pub(crate) const SEEK_END: i32 = 2;

/// Directory iteration operations installed on opened directory files.
pub trait FileDirOperations: Send + Sync {
    /// Iterates directory entries for this open file description.
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize>;
}

/// Filesystem preparation for writable shared file-mapping faults.
pub trait FileMappingOperations: Send + Sync {
    /// Prepares the valid file bytes in a cached folio before it becomes writable.
    ///
    /// The address-space invalidate lock is held for reading and the target
    /// folio is locked. Implementations may reserve backing storage but must
    /// not acquire the invalidate lock exclusively.
    fn page_mkwrite(&self, file: &VfsFile, pos: u64, len: usize) -> VfsResult<()>;
}

/// File operations installed on file-capable inodes.
///
/// The same operation table is attached to the inode and used by opened file
/// descriptions. Implementations that need per-open behavior may update the
/// [`VfsFileBuilder`] during `open`.
pub trait FileOperations: Send + Sync + 'static {
    /// Returns shared file-mapping fault operations when this file needs them.
    fn mapping_operations(&self) -> Option<&dyn FileMappingOperations> {
        None
    }

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

    /// The table's native chunk-copy primitive, mirroring Linux
    /// `f_op->copy_file_range`: copies up to `len` bytes from `input` at
    /// `input_pos` into this table's file at `output_pos` and returns the
    /// bytes actually copied. `None` means the table carries no native
    /// implementation — the generic buffered pump serves the pair, and only
    /// same-superblock pairs at that. An implementation judges input
    /// compatibility itself and reports `EXDEV` for inputs it cannot serve,
    /// so the VFS performs no identity comparison anywhere. The VFS also
    /// issues one empty-range (`len == 0`) query as pair admission, at the
    /// point where Linux compares the two `f_op->copy_file_range` slots.
    /// That query is the pair's verdict, so an implementation must answer it
    /// without side effects and with the answer a data-carrying call would
    /// give: `None` exactly when the table carries no primitive, and the
    /// input judgement ahead of any empty-range short-circuit.
    fn copy_chunk_from(
        &self,
        input: &VfsFile,
        input_pos: u64,
        output: &VfsFile,
        output_pos: u64,
        len: usize,
    ) -> Option<KResult<usize>> {
        let _ = (input, input_pos, output, output_pos, len);
        None
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
            // The capacity view: `blkdev_llseek` reads `bd_inode`'s size
            // (block/fops.c:533), unlike record locks which read the node
            // inode (locks.c:465) — the two SEEK_END bases differ by design.
            SEEK_END => file.size() as i64,
            _ => return Err(VfsError::InvalidInput),
        };
        let new_offset = base.checked_add(offset).ok_or(VfsError::InvalidInput)?;
        if new_offset < 0 {
            return Err(VfsError::InvalidInput);
        }
        let new_offset = new_offset as u64;
        // Block devices bound seeks to the capacity (`blkdev_llseek` uses
        // `fixed_size_llseek`, EINVAL past the end); regular files may seek
        // past EOF.
        if file.inode().node_type() == NodeType::BlockDevice && new_offset > file.size() {
            return Err(VfsError::from(LinuxError::EINVAL));
        }
        *position = new_offset;
        Ok(new_offset)
    }

    /// Reads file data for this open file description.
    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    /// Reads file data into an iterator destination.
    ///
    /// If a later backing read fails after bytes have already been copied, the
    /// completed byte count is returned to preserve partial-read semantics.
    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; memaddr::PAGE_SIZE_4K];
        while iter.count() != 0 {
            let want = chunk.len().min(iter.count());
            if want == 0 {
                break;
            }
            let read = match self.read(iocb.file(), &mut chunk[..want], iocb.ki_pos()) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(err) => return Err(err),
            };
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
    ///
    /// The backing implementation owns any resulting inode size change.
    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        scalar_write_iter(iocb, iter, |file, buf, offset| {
            self.write(file, buf, offset)
        })
    }

    /// Manipulates device parameters using the current caller's credentials.
    /// `cred` is the current subjective snapshot, not the opening credential.
    fn ioctl(
        &self,
        _file: &VfsFile,
        _cred: &kcred::Cred,
        _cmd: u32,
        _arg: usize,
    ) -> VfsResult<usize> {
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

    /// Queries a record-lock conflict in sleepable task context.
    fn test_lock(
        &self,
        file: &VfsFile,
        request: &crate::FileLock,
    ) -> VfsResult<Option<crate::FileLock>> {
        Ok(file
            .inode()
            .existing_file_locks()
            .and_then(|context| context.test(request)))
    }

    /// Applies a record lock, optionally waiting interruptibly for conflicts.
    /// Callers must release descriptor-table locks before entering this method.
    /// Returns `NoMemory` if the inode's lock context cannot be allocated;
    /// that failure installs no lock and a later call may retry.
    fn lock(&self, file: &VfsFile, request: &crate::FileLock, is_blocking: bool) -> VfsResult<()> {
        if request.kind() == crate::FileLockType::Unlock
            && file.inode().existing_file_locks().is_none()
        {
            return Ok(());
        }
        file.inode().file_locks()?.apply(request, is_blocking)
    }

    /// Applies a BSD lock in the inode's separate whole-file lock domain.
    /// Returns `NoMemory` if the inode's lock context cannot be allocated.
    fn flock(&self, file: &VfsFile, request: &crate::FileLock, is_blocking: bool) -> VfsResult<()> {
        if request.kind() == crate::FileLockType::Unlock
            && file.inode().existing_file_locks().is_none()
        {
            return Ok(());
        }
        file.inode().file_locks()?.apply(request, is_blocking)
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
    ///
    /// `data_only` may omit metadata that is not needed to retrieve file data.
    /// The half-open `range` selects data; `0..u64::MAX` selects the whole file.
    /// Empty data ranges still require applicable metadata and barrier duties.
    /// Implementations compose data write-and-wait with filesystem persistence.
    /// The default is unsupported (`EINVAL`); volatile backends must explicitly
    /// provide their successful no-op. Requires sleepable task context without
    /// spinlocks or IRQ/preemption guards; no current process is required.
    fn fsync(&self, _file: &VfsFile, _range: Range<u64>, _data_only: bool) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    /// fallocate mode bits this backend accepts, mirroring Linux per-filesystem
    /// capability masks.
    ///
    /// Returns: the accepted mask. Plain allocation is the empty flag set and
    /// always passes, so backends supporting only plain allocation keep the
    /// default empty mask.
    ///
    /// Locking: none; [`VfsFile::fallocate`] consults it before taking locks.
    fn supported_fallocate_modes(&self) -> FallocateFlags {
        FallocateFlags::empty()
    }

    /// Applies filesystem-specific space allocation or range-shift operations.
    ///
    /// - `file`: the open file whose inode and address space own the range.
    /// - `flags`: mode bits; all declared by `supported_fallocate_modes`.
    /// - `offset`: range start in bytes.
    /// - `len`: range length in bytes.
    ///
    /// Returns: `()` once the operation is applied.
    ///
    /// Errors: the default implementation answers `EOPNOTSUPP`, matching Linux
    /// when a file has no fallocate handler; implementations return their own
    /// errors.
    ///
    /// Locking: called by [`VfsFile::fallocate`] with the inode data lock and
    /// the address-space invalidate lock held exclusively. Publish size through
    /// [`VfsInode::truncate_setsize`]; the self-locking helpers deadlock here.
    fn fallocate(
        &self,
        _file: &VfsFile,
        _flags: FallocateFlags,
        _offset: u64,
        _len: u64,
    ) -> VfsResult<()> {
        // Linux answers EOPNOTSUPP when a file's operation table has no
        // fallocate handler (fs/open.c:323); the same errno replaces the
        // generic `Unsupported` (ENOSYS) here.
        Err(VfsError::from(LinuxError::EOPNOTSUPP))
    }

    /// Polls an open file description.
    ///
    /// `None` mirrors Linux `file->f_op->poll == NULL`: the file type provides
    /// no poll handler. [`VfsFile::poll`] then falls back to reporting the file
    /// as always ready (`DEFAULT_POLLMASK`, Linux `vfs_poll`), while `epoll_ctl`
    /// refuses to register such a target with `EPERM` (Linux `file_can_poll`,
    /// `fs/eventpoll.c`) — which is how a user-space runtime learns that a
    /// descriptor has to stay in blocking mode (the Go runtime's poller relies
    /// on exactly that for regular files).
    ///
    /// Providing this method *is* the capability: regular files and directories
    /// do not, so no implementation needs to declare pollability separately.
    ///
    /// The handler must be side-effect free apart from polling readiness; both
    /// wait paths (`poll`/`select` and `epoll`) call it to observe readiness.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        None
    }

    /// Registers a poll waiter for an open file description.
    ///
    /// Used when no captured source was provided.
    fn register_poll(
        &self,
        _file: &VfsFile,
        _context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Ok(())
    }

    /// Captures an optional wait-source identity without registering a waiter.
    ///
    /// `None` preserves the ordinary `register_poll` path. A captured source
    /// remains authoritative even after expiry; consumers must not fall back
    /// to recapturing a source in another caller's context.
    ///
    /// # Errors
    /// Returns a registration error when the backend cannot capture its source.
    fn capture_poll_source(
        &self,
        _file: &VfsFile,
    ) -> Result<Option<FilePollSource>, PollRegisterError> {
        Ok(None)
    }
}

pub(crate) fn scalar_write_iter(
    iocb: &mut Kiocb<'_>,
    iter: &mut IovIterSource<'_>,
    mut write: impl FnMut(&VfsFile, &[u8], u64) -> VfsResult<usize>,
) -> VfsResult<usize> {
    let mut total = 0usize;
    let mut chunk = [0u8; memaddr::PAGE_SIZE_4K];
    while iter.count() != 0 {
        let want = chunk.len().min(iter.count());
        let copied = match iter.copy_from_iter(&mut chunk[..want]) {
            Ok(copied) => copied,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        if copied == 0 {
            break;
        }
        let written = match write(iocb.file(), &chunk[..copied], iocb.ki_pos()) {
            Ok(written) => written,
            Err(error) => {
                iter.revert(copied)?;
                if total != 0 {
                    break;
                }
                return Err(error);
            }
        };
        if written > copied {
            iter.revert(copied)?;
            return Err(VfsError::InvalidData);
        }
        iter.revert(copied - written)?;
        if written == 0 {
            if total != 0 {
                break;
            }
            return Err(VfsError::WriteZero);
        }
        total += written;
        iocb.advance(written);
        if written < copied {
            break;
        }
    }
    Ok(total)
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

    fn mapping(&self) -> &Arc<AddressSpace> {
        &self.mapping
    }
}

fn default_direct(path: &Path, open_flags: OpenFlags) -> bool {
    let inode = path.inode();
    let direct = !path.is_dir()
        && (open_flags.intersects(OpenFlags::DIRECT | OpenFlags::PATH)
            || matches!(
                inode.node_type(),
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            )
            || inode.flags().contains(NodeFlags::NON_CACHEABLE));
    direct && !inode.flags().contains(NodeFlags::ALWAYS_CACHE)
}

/// Open-file state while the open path is still installing final fields.
pub struct VfsFileBuilder {
    cred: Arc<Cred>,
    mode: FMode,
    operations: Option<Arc<dyn FileOperations>>,
    location: Option<FileLocation>,
    private_data: TypeMap,
    slots: FileSlots,
    flags: OpenFlags,
    position: u64,
    /// FIFO writer-generation snapshot, corresponding to Linux `file::f_pipe`.
    pipe_generation: u64,
    /// Identity of the directory that received a newly created entry, plus the
    /// name it was created under, captured inside the creation's namespace
    /// lock. The created dentry's own location is mutable afterwards, so event
    /// attribution must use this snapshot, not a later read.
    created_under: Option<(Arc<VfsInode>, String)>,
}

impl VfsFileBuilder {
    /// Creates an unbound open-file builder.
    pub(crate) fn empty(flags: FMode, open_flags: OpenFlags, cred: Arc<Cred>) -> Self {
        Self {
            cred,
            mode: flags,
            operations: None,
            location: None,
            private_data: TypeMap::default(),
            slots: FileSlots::default(),
            flags: open_flags,
            position: 0,
            pipe_generation: 0,
            created_under: None,
        }
    }

    pub(crate) fn allocate(open_flags: OpenFlags, cred: Arc<Cred>) -> VfsResult<Self> {
        Ok(Self::empty(
            FMode::from_open_flags(open_flags),
            open_flags,
            cred,
        ))
    }

    pub(crate) fn from_path_state(
        path: Path,
        flags: FMode,
        open_flags: OpenFlags,
        f_op: Arc<dyn FileOperations>,
        cred: Arc<Cred>,
    ) -> Self {
        let mut file = Self::empty(flags, open_flags, cred);
        file.bind_location(FileLocation::from_path(path));
        file.install_operations(f_op);
        file
    }

    pub(crate) fn cloned_from(
        base: &VfsFile,
        flags: FMode,
        open_flags: OpenFlags,
        f_op: Arc<dyn FileOperations>,
    ) -> Self {
        let mut file = Self::empty(flags, open_flags, base.cred.clone());
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

    /// Records where and under what name this open created its entry.
    ///
    /// Called from the creation branch while the parent's namespace lock is
    /// still held; the created dentry's own location is mutable afterwards, so
    /// attribution uses this snapshot.
    pub(crate) fn set_created_under(&mut self, parent: Arc<VfsInode>, name: &str) {
        self.created_under = Some((parent, String::from(name)));
    }

    pub(crate) fn clear_created(&mut self) {
        self.mode.remove(FMode::CREATED);
    }

    /// Returns the access mode requested for this in-progress open.
    pub fn mode(&self) -> FMode {
        self.mode
    }

    /// Returns the open flags supplied for this in-progress open.
    pub fn flags(&self) -> OpenFlags {
        self.flags
    }

    /// Returns whether this open request has normalized nonblocking semantics.
    pub fn is_nonblocking(&self) -> bool {
        self.flags.contains(OpenFlags::NONBLOCK)
    }

    /// Returns whether this open request forbids controlling-terminal assignment.
    ///
    /// Device `open` callbacks use this before the transient `O_NOCTTY` flag is
    /// removed from the finalized open-file status flags.
    pub fn requests_no_controlling_tty(&self) -> bool {
        self.flags.contains(OpenFlags::NO_CONTROLLING_TTY)
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

        if self.flags.contains(OpenFlags::PATH) {
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
        let holds_write_access = self.mode.contains(FMode::WRITE)
            && !matches!(
                location.inode().node_type(),
                NodeType::CharacterDevice
                    | NodeType::BlockDevice
                    | NodeType::Fifo
                    | NodeType::Socket
            );
        let _write_guard = if holds_write_access {
            Some(location.path().lock_writable_mount()?)
        } else {
            None
        };
        if holds_write_access {
            location.inode().acquire_write_access();
            self.mode.insert(FMode::WRITER);
        }
        if let Err(error) = operations.open(location.inode().as_ref(), &mut self) {
            if holds_write_access {
                location.inode().release_write_access();
            }
            return Err(error);
        }
        self.flags.remove(
            OpenFlags::CREATE
                | OpenFlags::EXCLUSIVE
                | OpenFlags::NO_CONTROLLING_TTY
                | OpenFlags::TRUNCATE,
        );
        if let Err(error) = self.mark_opened() {
            if holds_write_access {
                location.inode().release_write_access();
            }
            return Err(error);
        }
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

    pub(crate) fn set_pipe_generation(&mut self, generation: u64) {
        self.pipe_generation = generation;
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

    /// Stores a kernel-wide framework object in `slot` on the new file.
    ///
    /// This is how a creator (a dma-buf exporter, for example) publishes an
    /// object that any other subsystem must find through the descriptor, as
    /// Linux lets a subsystem own a file through its `f_op`.
    pub fn set_slot(&mut self, slot: FileSlot, value: Arc<dyn Any + Send + Sync>) {
        self.slots.insert(slot, value);
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&mut self, flag: bool) {
        if flag {
            self.flags.insert(OpenFlags::NONBLOCK);
        } else {
            self.flags.remove(OpenFlags::NONBLOCK);
        }
    }

    pub(crate) fn finish(self) -> VfsResult<Arc<VfsFile>> {
        let Self {
            cred,
            mode,
            operations,
            location,
            private_data,
            slots,
            flags,
            position,
            pipe_generation,
            created_under,
        } = self;

        let operations = operations.ok_or(VfsError::InvalidInput)?;
        let location = location.ok_or(VfsError::InvalidInput)?;
        let readahead = crate::file_ra::FileRaState::new(location.inode().readahead_pages());
        let writeback_error_cursor = location.mapping().writeback_errors().sample();
        let filesystem_error_cursor = location.path().super_block().writeback_errors().sample();
        Ok(Arc::new(VfsFile {
            cred,
            mode: AtomicU32::new(mode.bits()),
            operations,
            location,
            private_data: Mutex::new(private_data),
            slots: Mutex::new(slots),
            flags: AtomicU32::new(flags.bits()),
            position: Mutex::new(position),
            dir_position: Mutex::new(()),
            readahead: Mutex::new(readahead),
            writeback_error_cursor: Mutex::new(writeback_error_cursor),
            filesystem_error_cursor: Mutex::new(filesystem_error_cursor),
            pipe_generation,
            created_under,
        }))
    }
}

/// VFS-owned open-file state.
pub struct VfsFile {
    cred: Arc<Cred>,
    mode: AtomicU32,
    operations: Arc<dyn FileOperations>,
    location: FileLocation,
    private_data: Mutex<TypeMap>,
    /// Kernel-wide framework slots ([`crate::FileSlot`]).
    slots: Mutex<FileSlots>,
    flags: AtomicU32,
    position: Mutex<u64>,
    /// Serializes directory position operations. `position` remains the sole
    /// f_pos storage; this lock only prevents iterate/seek from interleaving
    /// their read-execute-commit sequences.
    dir_position: Mutex<()>,
    pub(crate) readahead: Mutex<crate::file_ra::FileRaState>,
    writeback_error_cursor: Mutex<u64>,
    filesystem_error_cursor: Mutex<u64>,
    /// FIFO writer-generation snapshot, corresponding to Linux `file::f_pipe`.
    pipe_generation: u64,
    /// Where and under what name the open that produced this file created its
    /// entry; `None` unless that open actually created it.
    created_under: Option<(Arc<VfsInode>, String)>,
}

impl VfsFile {
    /// Returns the credentials captured when this file was opened.
    pub fn cred(&self) -> &Arc<Cred> {
        &self.cred
    }

    /// Allocates an opened file sharing the path and mapping of another file.
    pub fn alloc_clone(
        &self,
        flags: FMode,
        open_flags: OpenFlags,
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
        open_flags: OpenFlags,
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

    /// Calls this file's native chunk-copy primitive, if its operation
    /// table carries one; `None` means no native implementation.
    pub fn copy_chunk_from(
        &self,
        input: &VfsFile,
        input_pos: u64,
        output_pos: u64,
        len: usize,
    ) -> Option<KResult<usize>> {
        self.operations()
            .copy_chunk_from(input, input_pos, self, output_pos, len)
    }

    /// Returns this file's size.
    ///
    /// Block devices report their live capacity here, mirroring Linux's
    /// `bd_inode` size that backs `blkdev_llseek`'s `SEEK_END`; the special
    /// node's own metadata stays zero-sized. The node size (via getattr) and
    /// the capacity (via this query) are intentionally separate views, and
    /// the capacity tracks changes after open, e.g. loop attach/detach.
    pub fn size(&self) -> u64 {
        let inode = self.inode();
        if inode.node_type() == NodeType::BlockDevice
            && let Some(claim) = self.private_data_get::<block::BlockDeviceClaim>()
        {
            return claim.device().size();
        }
        inode.size()
    }

    /// Borrows this file's inode address space (`f_mapping`).
    ///
    /// Clone the returned [`Arc`] only when the mapping must outlive this
    /// `VfsFile` borrow.
    pub fn mapping(&self) -> &Arc<AddressSpace> {
        self.location.mapping()
    }

    /// Updates timestamps, then prepares a folio for a shared writable fault.
    ///
    /// The supplied closure updates the page table while the folio remains
    /// locked. A fault beyond the current EOF returns [`VfsError::BadAddress`].
    pub fn page_mkwrite<R>(
        &self,
        index: PageIndex,
        update_mapping: impl FnOnce(&mut FolioGuard<'_>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let _write_guard = self.path().super_block().try_start_write()?;
        self.path().check_writable_mount()?;
        self.verify_mode(FMode::WRITE)?;
        let _data_guard = self.inode().try_lock_data_shared()?;
        self.inode().check_mutable()?;
        // Timestamp policy may acquire filesystem locks; keep it outside the folio.
        self.file_update_time()?;
        self.mapping().page_mkwrite(
            index,
            |pos, len| {
                if let Some(ops) = self.operations().mapping_operations() {
                    ops.page_mkwrite(self, pos, len)?;
                }
                Ok(())
            },
            update_mapping,
        )
    }

    /// Waits for the resources needed by a shared write fault without an MM lock.
    ///
    /// Requires sleepable task context with IRQs/preemption enabled and no MM,
    /// inode-data or non-sleepable lock. Uses the existing admission locks and
    /// their wait queues. Completion grants no access: the caller must look up
    /// the current VMA and recheck permissions and inode policy before retrying.
    pub fn wait_for_shared_mapping_write(&self) {
        let _write_admission = self.path().super_block().lock_write_admission();
        let _data_guard = self.inode().lock_data_shared();
    }

    /// Writes and waits for cached file data without filesystem persistence.
    pub fn writeback_mapping(&self) -> VfsResult<()> {
        self.mapping().write_and_wait_range(0..u64::MAX)
    }

    /// Writes and waits for cached data intersecting `[start, start + len)`.
    /// Does not commit filesystem metadata or flush a device.
    ///
    /// # Errors
    /// Returns `InvalidInput` on range overflow, otherwise data writeback errors.
    pub fn writeback_mapping_range(&self, start: u64, len: usize) -> VfsResult<()> {
        let end = start
            .checked_add(len as u64)
            .ok_or(VfsError::InvalidInput)?;
        self.mapping().write_and_wait_range(start..end)
    }

    /// Returns the maximum regular-file size allowed for this open file.
    pub fn max_file_size(&self) -> u64 {
        self.path().max_file_size()
    }

    /// Synchronizes the filesystem containing this open file.
    pub fn sync_filesystem(&self) -> VfsResult<()> {
        let result = self.path().sync_filesystem();
        let recorded = self
            .path()
            .super_block()
            .writeback_errors()
            .check_and_advance(&mut self.filesystem_error_cursor.lock());
        result.and(recorded)
    }

    fn operations(&self) -> &dyn FileOperations {
        self.operations.as_ref()
    }

    /// Returns this file's access mode flags.
    pub fn mode(&self) -> FMode {
        FMode::from_bits_truncate(self.mode.load(Ordering::Acquire))
    }

    /// Returns whether the open that produced this file created the entry.
    ///
    /// `O_CREAT` on an existing path is an ordinary open; callers that report
    /// or act on creations (for example the inotify directory-entry hook) use
    /// this to tell the two apart.
    pub fn was_created(&self) -> bool {
        self.mode().contains(FMode::CREATED)
    }

    /// Returns the parent identity and the name this file's entry was created
    /// with, when the open that produced this file created it.
    ///
    /// Both were captured inside the creation's namespace lock, so they stay
    /// correct even if the entry is renamed before the caller reads them.
    pub fn created_under(&self) -> Option<(&Arc<VfsInode>, &str)> {
        self.created_under
            .as_ref()
            .map(|(parent, name)| (parent, name.as_str()))
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

    /// Changes the length of a file opened for writing.
    ///
    /// Descriptor-based truncation uses the authority established by the open
    /// file description and therefore does not repeat pathname DAC checks.
    pub fn truncate(&self, len: u64) -> VfsResult<()> {
        self.verify_mode(FMode::WRITE)?;
        self.path().truncate_opened(len)
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
        let mode = self.mode();
        if !mode.contains(FMode::READ) || mode.contains(FMode::PATH) {
            return Err(VfsError::BadFileDescriptor);
        }
        if !mode.contains(FMode::CAN_READ) {
            return Err(VfsError::InvalidInput);
        }
        self.verify_io_area(pos, count)
    }

    fn verify_write_area(&self, pos: u64, count: usize) -> VfsResult<()> {
        let mode = self.mode();
        if !mode.contains(FMode::WRITE) || mode.contains(FMode::PATH) {
            return Err(VfsError::BadFileDescriptor);
        }
        if !mode.contains(FMode::CAN_WRITE) {
            return Err(VfsError::InvalidInput);
        }
        self.verify_io_area(pos, count)?;
        if self.node_type().is_special() {
            Ok(())
        } else {
            self.path().check_writable_mount()
        }
    }

    /// Reads from this open file and advances `f_pos` when applicable.
    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let mut iter = iov_iter_kvec_dest(buf);
        self.read_iter(&mut iter)
    }

    /// Reads into an iterator, locking and advancing `f_pos` only for non-streams.
    pub fn read_iter(&self, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        if self.is_stream() {
            return self.read_iter_from(iter, &mut 0);
        }
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
        let mut iter = iov_iter_kvec_source(buf);
        self.write_iter(&mut iter)
    }

    /// Writes to this open file using a caller-owned position.
    pub fn write_from(&self, buf: &[u8], pos: &mut u64) -> VfsResult<usize> {
        let mut iter = iov_iter_kvec_source(buf);
        self.write_iter_from(&mut iter, pos)
    }

    /// Writes from an iterator, locking and advancing `f_pos` only for non-streams.
    pub fn write_iter(&self, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        if self.is_stream() {
            return self.write_iter_from(iter, &mut 0);
        }
        let mut pos = self.position_lock();
        self.write_iter_from(iter, &mut pos)
    }

    /// Writes to this open file from an iterator source using a caller-owned position.
    pub fn write_iter_from(&self, iter: &mut IovIterSource<'_>, pos: &mut u64) -> VfsResult<usize> {
        let _write_guard = if self.mode().contains(FMode::WRITER) {
            Some(self.path().lock_writable_mount()?)
        } else {
            None
        };
        if self.is_stream() {
            self.verify_write_area(0, iter.count())?;
            let mut iocb = Kiocb::new(self, 0);
            self.operations().write_iter(&mut iocb, iter)
        } else if self.flags().contains(OpenFlags::APPEND)
            && self.inode().node_type() != NodeType::BlockDevice
        {
            // Block devices keep the caller's position for appends: Linux
            // `blkdev_write_iter` writes at `ki_pos` and never repositions to
            // the capacity end like regular-file append handling does.
            let end = self.size();
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

    pub(crate) fn file_accessed(&self) {
        if !self.flags().contains(OpenFlags::NO_ATIME) {
            self.path().touch_atime();
        }
    }

    pub(crate) fn file_update_time(&self) -> VfsResult<()> {
        // Regular-file write operations already hold write admission.
        // Special files need admission while changing inode timestamps.
        let _write_guard = if self.mode().contains(FMode::WRITER) {
            None
        } else {
            match self.path().lock_writable_mount() {
                Ok(guard) => Some(guard),
                Err(error) if error == VfsError::ReadOnlyFilesystem => return Ok(()),
                Err(error) => return Err(error),
            }
        };
        self.path().update_cmtime()
    }

    /// Runs directory iteration for this open file description.
    ///
    /// Requires sleepable task context with IRQs/preemption enabled, outside
    /// non-sleepable RCU sections and locks needed by reclaim, IO or filesystem
    /// callbacks. Directory implementations may allocate with the default
    /// KERNEL policy.
    ///
    /// Directory iteration holds `dir_position` across reading f_pos, calling
    /// the implementation and committing `ctx.pos()`, including error exits.
    /// `position` itself is only held for the short read/commit steps; the
    /// operation lock is what prevents another iterate or llseek from
    /// interleaving with this sequence. Directory callbacks must not re-enter
    /// iterate/llseek on the same open description.
    pub fn iterate_dir(&self, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        if self.inode().node_type() != NodeType::Directory {
            return Err(VfsError::NotADirectory);
        }
        self.verify_mode(FMode::READ)?;

        let _dir_position = self.dir_position.lock();
        ctx.set_pos(self.position());
        let Some(directory) = self.operations().dir_operations() else {
            self.set_position(ctx.pos());
            return Err(VfsError::NotADirectory);
        };
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
    ///
    /// This is the single position storage, but it is not the directory
    /// operation lock. Directory position changes must go through
    /// [`Self::iterate_dir`] or [`Self::llseek`], which hold `dir_position`.
    /// Direct callers in production must not bypass that sequence.
    pub fn position_lock(&self) -> MutexGuard<'_, u64> {
        self.position.lock()
    }

    /// Returns this file's `f_pos`.
    pub fn position(&self) -> u64 {
        *self.position.lock()
    }

    pub(crate) fn pipe_generation(&self) -> u64 {
        self.pipe_generation
    }

    /// Sets this file's `f_pos`.
    ///
    /// Directory iteration is the only production caller; tests may use it
    /// directly. Other production code must use [`Self::llseek`] so the
    /// directory operation lock is respected.
    pub fn set_position(&self, position: u64) {
        *self.position_lock() = position;
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&self, flag: bool) {
        if flag {
            self.flags
                .fetch_or(OpenFlags::NONBLOCK.bits(), Ordering::AcqRel);
        } else {
            self.flags
                .fetch_and(!OpenFlags::NONBLOCK.bits(), Ordering::AcqRel);
        }
    }

    /// Returns the open-file nonblocking flag.
    pub fn is_nonblocking(&self) -> bool {
        self.flags().contains(OpenFlags::NONBLOCK)
    }

    /// Returns this file's `f_flags`.
    pub fn flags(&self) -> OpenFlags {
        OpenFlags::from_bits_retain(self.flags.load(Ordering::Acquire))
    }

    /// Replaces selected open-file status flags.
    pub fn replace_flags(&self, mask: OpenFlags, flags: OpenFlags) {
        let mask = mask.bits();
        let flags = flags.bits();
        // The closure always returns Some; callers need only the updated flags.
        let _ = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some((flags & mask) | (current & !mask))
            });
    }

    /// Attaches typed filesystem or kernel-object state to this open file.
    pub fn set_private_data<T>(&self, value: Arc<T>)
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

    /// Stores a kernel-wide framework object in `slot` on this open file.
    ///
    /// A slot is the file-level contract between subsystems that do not share
    /// types (see [`FileSlot`]); a creator installs it, every reader finds it.
    pub fn set_slot(&self, slot: FileSlot, value: Arc<dyn Any + Send + Sync>) {
        self.slots.lock().insert(slot, value);
    }

    /// Returns the framework object stored in `slot` as a dynamic object.
    ///
    /// The caller performs the narrowing it needs (`downcast` for a concrete
    /// type, or a runtime cast for a trait object), so one slot can be read by
    /// crates that share no types.
    pub fn slot_dyn(&self, slot: FileSlot) -> Option<Arc<dyn Any + Send + Sync>> {
        self.slots.lock().get(slot)
    }

    /// Returns the framework object stored in `slot`, downcast to `T`.
    pub fn slot_get<T>(&self, slot: FileSlot) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.slots.lock().get(slot)?.downcast::<T>().ok()
    }

    /// Closes a descriptor reference for the given files owner. POSIX locks
    /// are removed even when flush fails; other file references may remain.
    pub fn close_file(self: Arc<Self>, owner: crate::FileLockOwner) -> VfsResult<()> {
        let result = self.flush();
        if !self.is_path() {
            self.remove_posix_locks(owner);
        }
        result
    }

    /// Queries a record-lock conflict. The request already contains kernel
    /// offsets and owner identity; no user memory is accessed here.
    pub fn test_lock(&self, request: &crate::FileLock) -> VfsResult<Option<crate::FileLock>> {
        self.operations().test_lock(self, request)
    }

    /// Applies an advisory lock in sleepable task context, with optional
    /// interruptible waiting. No fd-table lock may be held across this call.
    pub fn set_lock(&self, request: &crate::FileLock, is_blocking: bool) -> VfsResult<()> {
        if matches!(request.owner(), crate::FileLockOwner::Flock(_)) {
            self.operations().flock(self, request, is_blocking)
        } else {
            self.operations().lock(self, request, is_blocking)
        }
    }

    /// Removes all record locks for an owner. Descriptor close supplies the
    /// files owner; final release supplies the OFD owner. Cleanup cannot fail
    /// to the caller and does not allocate in the local lock implementation.
    pub fn remove_posix_locks(&self, owner: crate::FileLockOwner) {
        // Close has no lock-error channel and must release even after a failed
        // filesystem flush. Dispatch through the same operation as acquisition.
        let _ = self
            .operations()
            .lock(self, &crate::FileLock::unlock(owner), false);
    }

    /// Flushes this open file.
    pub fn flush(&self) -> VfsResult<()> {
        self.operations().flush(self)
    }

    /// Synchronizes this open file.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::BadFileDescriptor`] for a path-only open, otherwise
    /// propagates the file operation's synchronization error unchanged.
    pub fn fsync(&self, data_only: bool) -> VfsResult<()> {
        self.fsync_range(0..u64::MAX, data_only)
    }

    /// Synchronizes data in `range` and the required filesystem metadata.
    ///
    /// Uses a half-open range; an empty range still dispatches filesystem sync.
    /// `data_only` may omit metadata unrelated to retrieving the file's data.
    /// Requires sleepable task context without spinlocks or IRQ/preemption
    /// guards; no current process is required. Backend errors take precedence
    /// while this file's independent writeback-error cursor is advanced.
    ///
    /// # Errors
    /// Returns `BadFileDescriptor` for a path-only open, `InvalidInput` for a
    /// backwards range, nonsleepable context or missing filesystem capability,
    /// otherwise the backend or a recorded writeback error.
    pub fn fsync_range(&self, range: Range<u64>, data_only: bool) -> VfsResult<()> {
        if self.is_path() {
            return Err(VfsError::BadFileDescriptor);
        }
        if range.start > range.end {
            return Err(VfsError::InvalidInput);
        }
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        let result = self.operations().fsync(self, range, data_only);
        let recorded = self
            .mapping()
            .writeback_errors()
            .check_and_advance(&mut self.writeback_error_cursor.lock());
        result.and(recorded)
    }

    /// Changes this open file offset.
    ///
    /// Directory seeks take the same `dir_position` operation lock as
    /// [`Self::iterate_dir`], so a seek cannot land between iteration's f_pos
    /// read and commit. The callback still owns the actual position update.
    pub fn llseek(&self, offset: i64, whence: i32) -> VfsResult<u64> {
        if !self.mode().contains(FMode::LSEEK) {
            return Err(VfsError::from(LinuxError::ESPIPE));
        }
        let is_directory = self.inode().node_type() == NodeType::Directory;
        let _dir_position = if is_directory {
            Some(self.dir_position.lock())
        } else {
            None
        };
        self.operations().llseek(self, offset, whence)
    }

    /// Dispatches an ioctl using the current caller's subjective credentials.
    /// The caller must supply its current snapshot, not the opening credential.
    pub fn ioctl(&self, cred: &Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.operations().ioctl(self, cred, cmd, arg)
    }

    /// Creates a mapping for this open file.
    pub fn mmap(&self, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.operations().mmap(self, mapper)
    }

    /// Applies filesystem-specific space allocation or range-shift operations.
    ///
    /// - `flags`: mode bits; must be within the backend's
    ///   `supported_fallocate_modes` declaration.
    /// - `offset`: range start in bytes.
    /// - `len`: range length in bytes.
    ///
    /// Returns: `()` once the backend completed the operation.
    ///
    /// Errors: `EOPNOTSUPP` when the backend does not declare a requested mode
    /// bit; backend errors propagate unchanged.
    ///
    /// Locking: holds the inode data lock and the address-space invalidate
    /// lock exclusively across the backend call, like `set_len`; writeback
    /// samples the visible size under the invalidate read lock, so excluding it
    /// keeps a batch from carrying a stale size snapshot past a disk-size
    /// advance.
    pub fn fallocate(&self, flags: FallocateFlags, offset: u64, len: u64) -> VfsResult<()> {
        let _write_guard = self.path().lock_writable_mount()?;
        let inode = self.inode();
        let operations = self.operations();
        if !operations.supported_fallocate_modes().contains(flags) {
            return Err(VfsError::from(LinuxError::EOPNOTSUPP));
        }
        let _data_guard = inode.lock_data();
        inode.check_mutable()?;
        let _invalidate_guard = self.mapping().lock_invalidate_exclusive();
        operations.fallocate(self, flags, offset, len)
    }

    /// Returns this file's backend fallocate capability mask.
    ///
    /// Returns: the declared mask; empty means the backend has no real
    /// fallocate, so the syscall layer may emulate plain allocation.
    ///
    /// Locking: none.
    pub fn supported_fallocate_modes(&self) -> FallocateFlags {
        self.operations().supported_fallocate_modes()
    }

    /// Polls this open file (Linux `vfs_poll`).
    ///
    /// A file type without a poll handler reports itself as always ready
    /// (`DEFAULT_POLLMASK`); `epoll` uses [`Self::can_poll`] to refuse such a
    /// target instead of falling back.
    pub fn poll(&self) -> IoEvents {
        self.poll_handler().unwrap_or(IoEvents::IN | IoEvents::OUT)
    }

    /// Runs this file's poll handler, or `None` when the file type provides
    /// none (Linux `f_op->poll == NULL`).
    pub fn poll_handler(&self) -> Option<IoEvents> {
        self.operations().poll(self)
    }

    /// Returns whether this open file may be registered with `epoll_ctl`.
    ///
    /// Mirrors Linux `file_can_poll`: an unpollable target is rejected with
    /// `EPERM`. The capability is exactly "the file type provides a poll
    /// handler", so it cannot drift from [`Self::poll_handler`].
    pub fn can_poll(&self) -> bool {
        self.poll_handler().is_some()
    }

    /// Registers a poll waiter for this open file.
    pub fn register_poll(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.operations().register_poll(self, context, events)
    }

    /// Captures the backend's optional wait source, independently of readiness.
    ///
    /// # Errors
    /// Propagates the backend's capture error without installing registrations.
    pub fn capture_poll_source(&self) -> Result<Option<FilePollSource>, PollRegisterError> {
        self.operations().capture_poll_source(self)
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        self.remove_posix_locks(crate::FileLockOwner::ofd(self));
        let _ = self.operations().flock(
            self,
            &crate::FileLock::unlock(crate::FileLockOwner::flock(self)),
            false,
        );
        let old_mode = self.mode.fetch_and(!FMode::OPENED.bits(), Ordering::AcqRel);
        let old_mode = FMode::from_bits_truncate(old_mode);
        let release_result = if old_mode.contains(FMode::OPENED) {
            self.operations().release(self.inode(), self)
        } else {
            Ok(())
        };
        if old_mode.contains(FMode::WRITER) {
            self.inode().release_write_access();
        }

        if let Err(err) = release_result {
            let path = self
                .path()
                .display_path()
                .unwrap_or_else(|_| "<error>".into());
            warn!("failed to release VFS file {}: {err:?}", path);
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/file.rs");
