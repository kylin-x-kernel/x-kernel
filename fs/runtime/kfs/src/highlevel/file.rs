// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File abstraction and caching layer.
use alloc::{borrow::Cow, string::ToString, sync::Arc, vec::Vec};
use core::{
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    task::Context,
};

use kerrno::{KError, KResult};
use kfd::{FdTable, FileLike, IoDst, IoSrc, Kstat};
use kio::{SeekFrom, prelude::*};
use kpoll::{IoEvents, Pollable};
use ksync::{Mutex, RwLock};
use ktask::future::{block_on, poll_io};
use kvfs::{
    Location, MmapMapper, NodeFlags, NodePermission, NodeType, VfsError, VfsResult, path::Path,
};
use memaddr::PAGE_SIZE_4K;

use super::{
    FsContext,
    mapping::{EvictRegistration, FileMapping, FileMappingData, PageCache, PageIndex},
};

bitflags::bitflags! {
    /// Access mode flags for an opened file.
    #[derive(Debug, Clone, Copy)]
    pub struct FileFlags: u8 {
        const READ = 1;
        const WRITE = 2;
        const EXECUTE = 4;
        const APPEND = 8;
        const PATH = 16;
    }
}

/// Results returned by [`OpenOptions::open`].
pub enum OpenResult {
    File(File),
    Dir(Location),
}

impl OpenResult {
    /// Convert into a file result, erroring if it is a directory.
    pub fn into_file(self) -> VfsResult<File> {
        match self {
            Self::File(file) => Ok(file),
            Self::Dir(_) => Err(VfsError::IsADirectory),
        }
    }

    /// Convert into a directory result, erroring if it is a file.
    pub fn into_dir(self) -> VfsResult<Location> {
        match self {
            Self::Dir(dir) => Ok(dir),
            Self::File(_) => Err(VfsError::NotADirectory),
        }
    }

    /// Convert into a location regardless of variant.
    pub fn into_location(self) -> Location {
        match self {
            Self::File(file) => file.location().clone(),
            Self::Dir(dir) => dir,
        }
    }
}

/// Options and flags which can be used to configure how a file is opened.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    // generic
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    directory: bool,
    no_follow: bool,
    direct: bool,
    user: Option<(u32, u32)>,
    path: bool,
    node_type: NodeType,
    // system-specific
    mode: u32,
    open_flags: u32,
}

impl OpenOptions {
    /// Creates a blank new set of options ready for configuration.
    pub fn new() -> Self {
        Self {
            // generic
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            directory: false,
            no_follow: false,
            direct: false,
            user: None,
            path: false,
            node_type: NodeType::RegularFile,
            // system-specific
            mode: 0o666,
            open_flags: 0,
        }
    }

    /// Sets the option for read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Sets the option for write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Sets the option for the append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Sets the option for truncating a previous file.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Sets the option to create a new file, or open it if it already exists.
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Sets the option to create a new file, failing if it already exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Sets the option to open directory instead.
    pub fn directory(&mut self, directory: bool) -> &mut Self {
        self.directory = directory;
        self
    }

    /// Sets the option to not follow symlinks.
    pub fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    /// Sets the option to open the file with direct I/O.\
    pub fn direct(&mut self, direct: bool) -> &mut Self {
        self.direct = direct;
        self
    }

    /// Sets the user and group id to open the file with.
    pub fn user(&mut self, uid: u32, gid: u32) -> &mut Self {
        self.user = Some((uid, gid));
        self
    }

    /// Sets the option for path only access.
    pub fn path(&mut self, path: bool) -> &mut Self {
        self.path = path;
        self
    }

    /// Sets the node type for the file.
    ///
    /// This will only be used if the file is created.
    pub fn node_type(&mut self, node_type: NodeType) -> &mut Self {
        self.node_type = node_type;
        self
    }

    /// Sets the mode bits that a new file will be created with.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    /// Sets the raw open flags (e.g., O_RDONLY, O_WRONLY) for the file.
    pub fn open_flags(&mut self, open_flags: u32) -> &mut Self {
        self.open_flags = open_flags;
        self
    }

    fn mutates_existing_file(&self) -> bool {
        self.write || self.append || self.truncate
    }

    fn _open(&self, loc: Location) -> VfsResult<OpenResult> {
        let flags = self.to_flags()?;

        let is_dir = loc.is_dir();
        if is_dir && flags.contains(FileFlags::WRITE) {
            return Err(VfsError::IsADirectory);
        }
        if self.directory {
            loc.check_is_dir()?;
        }
        if self.mutates_existing_file() {
            loc.check_writable_mount()?;
        }
        Ok(if is_dir {
            OpenResult::Dir(loc)
        } else {
            // TODO(mivik): is this correct?
            let metadata = loc.metadata()?;
            let non_cacheable_type = matches!(
                metadata.node_type,
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            );

            let direct = non_cacheable_type
                || self.path
                || self.direct
                || loc.flags().contains(NodeFlags::NON_CACHEABLE);
            let backend = if !direct || loc.flags().contains(NodeFlags::ALWAYS_CACHE) {
                FileBackend::new_cached(loc)
            } else {
                FileBackend::new_direct(loc)
            };
            if self.truncate {
                if metadata.node_type == NodeType::RegularFile {
                    CachedFile::get_or_create(backend.location().clone()).set_len(0)?;
                } else {
                    backend.set_len(0)?;
                }
            }
            OpenResult::File(File::with_open_flags(backend, flags, self.open_flags))
        })
    }

    pub fn open_loc(&self, loc: Location) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }
        self._open(loc)
    }

    pub fn open(&self, context: &FsContext, path: impl AsRef<Path>) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        let loc = match context.resolve_parent(path.as_ref()) {
            Ok((parent, name)) => {
                let loc = parent.open_file(
                    &name,
                    &kvfs::OpenOptions {
                        create: self.create,
                        create_new: self.create_new,
                        node_type: self.node_type,
                        permission: NodePermission::from_bits_truncate(self.mode as _),
                        user: self.user,
                    },
                )?;
                if !self.no_follow {
                    context.resolve(path)?
                } else {
                    loc
                }
            }
            Err(VfsError::InvalidInput) => {
                // root directory
                context.root_dir().clone()
            }
            Err(err) => return Err(err),
        };
        self._open(loc)
    }

    pub(crate) fn to_flags(&self) -> VfsResult<FileFlags> {
        Ok(match (self.read, self.write, self.append) {
            (true, false, false) => FileFlags::READ,
            (false, true, false) => FileFlags::WRITE,
            (true, true, false) => FileFlags::READ | FileFlags::WRITE,
            (false, _, true) => FileFlags::WRITE | FileFlags::APPEND,
            (true, _, true) => FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
            (false, false, false) => return Err(VfsError::InvalidInput),
        } | if self.path {
            FileFlags::PATH
        } else {
            FileFlags::empty()
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        if !self.read && !self.write && !self.append {
            return true;
        }
        match (self.write, self.append) {
            (true, false) => {}
            (false, false) => {
                if self.truncate || self.create || self.create_new {
                    return false;
                }
            }
            (_, true) => {
                if self.truncate && !self.create_new {
                    return false;
                }
            }
        }
        true
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CachedFile {
    inner: Location,
    mapping: Arc<FileMapping>,
    in_memory: bool,
    /// Only one thread can append to the file at a time, while multiple writers
    /// are permitted.
    append_lock: RwLock<()>,
}

impl Clone for CachedFile {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            mapping: self.mapping.clone(),
            in_memory: self.in_memory,
            append_lock: RwLock::new(()),
        }
    }
}

impl CachedFile {
    pub fn get_or_create(location: Location) -> Self {
        let in_memory = matches!(location.filesystem().name(), "tmpfs" | "memfs",);

        let mut guard = location.inode_data();
        let mapping = if let Some(mapping) = guard.get::<FileMappingData>() {
            mapping.mapping()
        } else {
            let mapping = if in_memory {
                Arc::new(FileMapping::new_unbounded())
            } else {
                Arc::new(FileMapping::new())
            };
            guard.insert(FileMappingData::new(mapping.clone()));
            mapping
        };
        drop(guard);

        Self {
            inner: location,
            mapping,
            in_memory,
            append_lock: RwLock::new(()),
        }
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mapping, &other.mapping)
    }

    pub fn in_memory(&self) -> bool {
        self.in_memory
    }

    pub fn add_evict_listener<F>(&self, listener: F) -> EvictRegistration
    where
        F: Fn(PageIndex, &PageCache) + Send + Sync + 'static,
    {
        self.mapping.add_evict_listener(listener)
    }

    fn page_start(pn: PageIndex) -> VfsResult<u64> {
        pn.checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)
    }

    pub fn with_page<R>(&self, pn: PageIndex, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        self.mapping.with_page(pn, f)
    }

    pub fn with_page_or_insert<R>(
        &self,
        pn: PageIndex,
        f: impl FnOnce(&mut PageCache, Vec<PageIndex>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let file = self.inner.entry().as_file()?;
        self.mapping
            .with_page_or_insert(file, self.in_memory, pn, f)
    }

    pub fn read_at(&self, mut dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let end = (offset + dst.remaining_mut() as u64).min(len);
        if end <= offset {
            return Ok(0);
        }
        let start_page = offset / PAGE_SIZE_4K as u64;
        let end_page = end.div_ceil(PAGE_SIZE_4K as u64);
        let mut page_offset = (offset % PAGE_SIZE_4K as u64) as usize;
        let mut read = 0;
        let mut chunk = [0u8; PAGE_SIZE_4K];
        let file = self.inner.entry().as_file()?;
        for pn in start_page..end_page {
            let page_start = Self::page_start(pn)?;
            let range = page_offset..(end - page_start).min(PAGE_SIZE_4K as u64) as usize;
            let chunk_len = range.end - range.start;
            self.mapping
                .with_page_or_insert(file, self.in_memory, pn, |page, _| {
                    chunk[..chunk_len].copy_from_slice(&page.data()[range.clone()]);
                    Ok(())
                })?;
            dst.write(&chunk[..chunk_len])?;
            read += chunk_len;
            page_offset = 0;
        }
        Ok(read)
    }

    fn write_at_locked(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let end = offset + buf.remaining() as u64;
        let file = self.inner.entry().as_file()?;
        if end > file.len()? {
            file.set_len(end)?;
        }
        let start_page = offset / PAGE_SIZE_4K as u64;
        let end_page = end.div_ceil(PAGE_SIZE_4K as u64);
        let mut page_offset = (offset % PAGE_SIZE_4K as u64) as usize;
        let mut written = 0;
        let mut chunk = [0u8; PAGE_SIZE_4K];
        for pn in start_page..end_page {
            let page_start = Self::page_start(pn)?;
            let range = page_offset..(end - page_start).min(PAGE_SIZE_4K as u64) as usize;
            let chunk_len = range.end - range.start;
            buf.read(&mut chunk[..chunk_len])?;
            self.mapping
                .with_page_or_insert(file, self.in_memory, pn, |page, _| {
                    page.data()[range.clone()].copy_from_slice(&chunk[..chunk_len]);
                    if !self.in_memory {
                        page.mark_dirty();
                    }
                    Ok(())
                })?;
            written += chunk_len;
            page_offset = 0;
        }
        Ok(written)
    }

    pub fn write_at(&self, buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let _guard = self.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    pub fn append(&self, buf: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        let _guard = self.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        self.write_at_locked(buf, len)
            .map(|written| (written, len + written as u64))
    }

    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
        file.set_len(len)?;
        self.mapping.set_len(file, self.in_memory, old_len, len)
    }

    fn flush_and_evict_from(&self, offset: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        self.mapping
            .flush_and_evict_from(file, self.in_memory, offset)
    }

    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        self.mapping.sync(file, self.in_memory, data_only)
    }

    pub fn location(&self) -> &Location {
        &self.inner
    }
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        if Arc::strong_count(&self.mapping) > 1 {
            // If there are other references to this cached file, we don't
            // need to drop it.
            return;
        }
        if let Err(err) = self.sync(false) {
            warn!("Failed to sync file on drop: {err:?}");
        }
    }
}

/// Low-level interface for file operations.
#[derive(Clone)]
pub enum FileBackend {
    Cached(CachedFile),
    Direct(Location),
}

impl FileBackend {
    const DIRECT_IO_CHUNK_SIZE: usize = 4096;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    pub fn read_at(&self, mut dst: impl Write + IoBufMut, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at(dst, offset),
            Self::Direct(loc) => {
                let mut total = 0usize;
                let file = loc.entry().as_file()?;
                let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

                while !dst.is_full() {
                    let want = chunk.len().min(dst.remaining_mut());
                    if want == 0 {
                        break;
                    }

                    let read = file.read_at(&mut chunk[..want], offset)?;
                    if read == 0 {
                        break;
                    }
                    offset += read as u64;

                    let mut consumed = 0usize;
                    while consumed < read {
                        let written = dst.write(&chunk[consumed..read])?;
                        if written == 0 {
                            return Err(VfsError::WriteZero);
                        }
                        consumed += written;
                    }

                    total += read;

                    if read < want {
                        break;
                    }
                }

                Ok(total)
            }
        }
    }

    pub fn write_at(&self, mut src: impl Read + IoBuf, mut offset: u64) -> VfsResult<usize> {
        self.location().check_writable_mount()?;
        match self {
            Self::Cached(cached) => cached.write_at(src, offset),
            Self::Direct(loc) => {
                let mut total = 0usize;
                let file = loc.entry().as_file()?;
                let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

                while !src.is_empty() {
                    let read = src.read(&mut chunk)?;
                    if read == 0 {
                        break;
                    }

                    let mut written_in_chunk = 0usize;
                    while written_in_chunk < read {
                        let written = file.write_at(&chunk[written_in_chunk..read], offset)?;
                        if written == 0 {
                            return Err(VfsError::WriteZero);
                        }
                        written_in_chunk += written;
                        offset += written as u64;
                    }

                    total += read;
                }

                Ok(total)
            }
        }
    }

    pub fn append(&self, mut src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.location().check_writable_mount()?;
        match self {
            Self::Cached(cached) => cached.append(src),
            Self::Direct(loc) => {
                let mut total = 0usize;
                let mut end = loc.len()?;
                let mut chunk = [0u8; 4096];

                while !src.is_empty() {
                    let read = src.read(&mut chunk)?;
                    if read == 0 {
                        break;
                    }

                    let mut written_in_chunk = 0usize;
                    while written_in_chunk < read {
                        let (written, new_end) = loc
                            .entry()
                            .as_file()?
                            .append(&chunk[written_in_chunk..read])?;
                        if written == 0 {
                            return Err(VfsError::WriteZero);
                        }
                        written_in_chunk += written;
                        end = new_end;
                    }

                    total += read;
                }

                Ok((total, end))
            }
        }
    }

    pub fn location(&self) -> &Location {
        match self {
            Self::Cached(cached) => cached.location(),
            Self::Direct(loc) => loc,
        }
    }

    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.sync(data_only),
            Self::Direct(loc) => loc.entry().as_file()?.sync(data_only),
        }
    }

    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        self.location().check_writable_mount()?;
        match self {
            Self::Cached(cached) => cached.set_len(len),
            Self::Direct(loc) => loc.entry().as_file()?.set_len(len),
        }
    }

    pub fn collapse_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.location().check_writable_mount()?;
        if let Self::Cached(cached) = self {
            cached.flush_and_evict_from(offset)?;
        }
        crate::fs::range_shift(self.location(), offset, len, false)
    }

    pub fn insert_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.location().check_writable_mount()?;
        if let Self::Cached(cached) = self {
            cached.flush_and_evict_from(offset)?;
        }
        crate::fs::range_shift(self.location(), offset, len, true)
    }
}

/// Provides `std::fs::File`-like interface.
pub struct File {
    inner: FileBackend,
    flags: FileFlags,
    position: Option<Mutex<u64>>,
    open_flags: u32,
    nonblock: AtomicBool,
    #[cfg(feature = "times")]
    access_flags: AtomicU8,
    #[cfg(feature = "times")]
    is_effectively_readonly: bool,
}

impl File {
    pub fn new(inner: FileBackend, flags: FileFlags) -> Self {
        Self::with_open_flags(inner, flags, 0)
    }

    pub fn with_open_flags(inner: FileBackend, flags: FileFlags, open_flags: u32) -> Self {
        let position = if inner.location().flags().contains(NodeFlags::STREAM) {
            None
        } else {
            Some(Mutex::new(if flags.contains(FileFlags::APPEND) {
                inner.location().len().unwrap_or_default()
            } else {
                0
            }))
        };
        #[cfg(feature = "times")]
        let is_effectively_readonly = inner.location().is_effectively_readonly();
        Self {
            inner,
            flags,
            position,
            open_flags,
            nonblock: AtomicBool::new(false),
            #[cfg(feature = "times")]
            access_flags: AtomicU8::new(0),
            #[cfg(feature = "times")]
            is_effectively_readonly,
        }
    }

    pub fn open(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new()
            .read(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    pub fn create(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    pub fn access(&self, flags: FileFlags) -> VfsResult<&FileBackend> {
        if self.flags.contains(flags) && !self.is_path() {
            Ok(&self.inner)
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    pub fn is_path(&self) -> bool {
        self.flags.contains(FileFlags::PATH)
    }

    pub fn flags(&self) -> FileFlags {
        self.flags
    }

    /// Checks the node-level blocking attribute (e.g. regular files are always
    /// blocking). This is independent of the user-facing `O_NONBLOCK` flag
    /// checked by `nonblocking()`. When true, read/write bypass poll_io and
    /// execute directly — matching Linux behavior where `O_NONBLOCK` has no
    /// effect on regular files.
    fn is_blocking(&self) -> bool {
        self.inner.location().flags().contains(NodeFlags::BLOCKING)
    }

    pub fn backend(&self) -> VfsResult<&FileBackend> {
        self.access(FileFlags::empty())?;
        Ok(&self.inner)
    }

    pub fn location(&self) -> &Location {
        self.inner.location()
    }

    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.access(FileFlags::READ)?.read_at(dst, offset)
    }

    /// Writes a number of bytes starting from a given offset.
    pub fn write_at(&self, src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        self.access(FileFlags::WRITE)?.write_at(src, offset)
    }

    /// Attempts to sync OS-internal file content and metadata to disk.
    ///
    /// If `data_only` is `true`, only the file data is synced, not the
    /// metadata.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        self.inner.sync(data_only)
    }

    pub fn read(&self, dst: impl Write + IoBufMut) -> kio::Result<usize> {
        #[cfg(feature = "times")]
        {
            self.access_flags.fetch_or(1, Ordering::AcqRel);
        }
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at(dst, 0)
        }
    }

    pub fn write(&self, src: impl Read + IoBuf) -> kio::Result<usize> {
        #[cfg(feature = "times")]
        {
            self.access_flags.fetch_or(3, Ordering::AcqRel);
        }
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if let Ok(f) = self.access(FileFlags::APPEND) {
                f.append(src).map(|(written, new_size)| {
                    *pos = new_size;
                    written
                })
            } else {
                self.write_at(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            }
        } else {
            self.write_at(src, 0)
        }
    }

    pub fn flush(&self) -> kio::Result {
        self.access(FileFlags::empty())?;
        Ok(())
    }
}

impl Read for &File {
    fn read(&mut self, buf: &mut [u8]) -> kio::Result<usize> {
        (*self).read(buf)
    }
}

impl Write for &File {
    fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
        (*self).write(buf)
    }

    fn flush(&mut self) -> kio::Result {
        (*self).flush()
    }
}

impl Seek for &File {
    fn seek(&mut self, pos: SeekFrom) -> kio::Result<u64> {
        self.access(FileFlags::empty())?;

        if let Some(guard) = self.position.as_ref() {
            let mut guard = guard.lock();
            let new_pos = match pos {
                SeekFrom::Start(pos) => pos,
                SeekFrom::End(off) => {
                    let size = self.access(FileFlags::empty())?.location().len()?;
                    size.checked_add_signed(off).ok_or(VfsError::InvalidInput)?
                }
                SeekFrom::Current(off) => guard
                    .checked_add_signed(off)
                    .ok_or(VfsError::InvalidInput)?,
            };
            *guard = new_pos;
            Ok(new_pos)
        } else {
            Ok(0)
        }
    }
}

impl Pollable for File {
    fn poll(&self) -> IoEvents {
        self.inner.location().poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner.location().register(context, events)
    }
}

use super::path_for;

impl FileLike for File {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        if likely(self.is_blocking()) {
            self.read(dst)
        } else {
            block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
                self.read(&mut *dst)
            }))
        }
    }

    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        if likely(self.is_blocking()) {
            self.write(src)
        } else {
            block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
                self.write(&mut *src)
            }))
        }
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat::from(self.location().metadata()?))
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        self.backend()?.location().ioctl(cmd, arg)
    }

    fn set_nonblocking(&self, flag: bool) -> KResult {
        self.nonblock.store(flag, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    fn open_flags(&self) -> u32 {
        self.open_flags
    }

    fn path(&self) -> Cow<'_, str> {
        path_for(self.location())
    }

    fn from_fd(fd_table: &RwLock<FdTable>, fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        fd_table
            .read()
            .get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::InvalidInput)
    }

    fn mmap(&self, mapper: &mut dyn MmapMapper) -> KResult<()> {
        match &self.inner {
            FileBackend::Cached(_) => mapper.map_file_backed()?,
            FileBackend::Direct(loc) => match loc.node_type() {
                NodeType::CharacterDevice | NodeType::BlockDevice => loc.mmap(mapper)?,
                _ => mapper.map_file_backed()?,
            },
        }
        Ok(())
    }
}

#[cfg(feature = "times")]
impl Drop for File {
    fn drop(&mut self) {
        let flags = self.access_flags.load(Ordering::Acquire);
        if flags != 0 {
            if self.is_effectively_readonly {
                return;
            }
            let mut update = kvfs::MetadataUpdate::default();
            if flags & 1 != 0 {
                update.atime = Some(khal::time::wall_time());
            }
            if flags & 2 != 0 {
                update.mtime = Some(khal::time::wall_time());
            }
            if let Err(err) = self.inner.location().update_metadata(update) {
                warn!("Failed to update file times on drop: {err:?}");
            }
        }
    }
}
