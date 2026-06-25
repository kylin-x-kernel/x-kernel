// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File abstraction and caching layer.
use alloc::{borrow::Cow, string::ToString, sync::Arc};
use core::{
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicU8, Ordering},
    task::Context,
};

use kerrno::{KError, KResult};
use kfd::{FdTable, FileLike, IoDst, IoSrc, Kstat};
use kio::{SeekFrom, prelude::*};
use kpoll::{IoEvents, Pollable};
use ksync::RwLock;
use ktask::future::{block_on, poll_io};
pub use kvfs::VfsFileFlags as FileFlags;
use kvfs::{
    DirEntrySink, Location, LookupFlags, LookupIntent, MmapMapper, NodeFlags, NodePermission,
    NodeType, ResolvedObject, VfsError, VfsFile, VfsResult, check_file_size, generic_read_range,
    generic_write_range, lookup_location, lookup_parent, path::Path,
};
use memaddr::PAGE_SIZE_4K;
use memfs::shmem;
use pagecache::Mapping;

use super::{FsContext, mapping};

const MAGIC_LINKS_MAX: usize = 40;

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

    fn _open(&self, loc: Location) -> VfsResult<File> {
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
        let direct = if is_dir {
            false
        } else {
            let metadata = loc.metadata()?;
            let non_cacheable_type = matches!(
                metadata.node_type,
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            );

            non_cacheable_type
                || self.path
                || self.direct
                || loc.flags().contains(NodeFlags::NON_CACHEABLE)
        };
        let direct = direct && !loc.flags().contains(NodeFlags::ALWAYS_CACHE);
        let file = File::open_location(loc, flags, self.open_flags, direct)?;
        if self.truncate {
            file.set_len(0)?;
        }
        Ok(file)
    }

    pub fn open_loc(&self, loc: Location) -> VfsResult<File> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }
        self._open(loc)
    }

    /// Opens a VFS object that has already been resolved by a path lookup.
    ///
    /// Normal locations are opened directly. Magic links are followed with
    /// typed VFS semantics instead of reparsing their display target.
    pub fn open_resolved(&self, resolved: ResolvedObject) -> VfsResult<File> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        let mut current = resolved;
        for _ in 0..MAGIC_LINKS_MAX {
            match current {
                ResolvedObject::Location(loc) => return self._open(loc),
                ResolvedObject::MagicLink(link) => {
                    let flags = if self.no_follow {
                        LookupFlags::no_follow()
                    } else {
                        LookupFlags::follow()
                    };
                    current = link.follow(LookupIntent::Open, flags)?;
                }
            }
        }
        Err(VfsError::FilesystemLoop)
    }

    pub fn open(&self, context: &FsContext, path: impl AsRef<Path>) -> VfsResult<File> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        let lookup_context = context.lookup_context();
        let loc = if self.create || self.create_new {
            match lookup_parent(&lookup_context, path.as_ref(), LookupIntent::Open) {
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
                    if self.no_follow {
                        loc
                    } else {
                        lookup_location(
                            &lookup_context,
                            path,
                            LookupIntent::Open,
                            LookupFlags::follow(),
                        )?
                    }
                }
                Err(VfsError::InvalidInput) => context.root_dir().clone(),
                Err(err) => return Err(err),
            }
        } else {
            let flags = if self.no_follow {
                LookupFlags::no_follow()
            } else {
                LookupFlags::follow()
            };
            lookup_location(&lookup_context, path, LookupIntent::Open, flags)?
        };

        if self.no_follow
            && !self.path
            && (loc.node_type() == NodeType::Symlink || loc.magic_link().is_some())
        {
            return Err(VfsError::FilesystemLoop);
        }

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

/// Provides `std::fs::File`-like interface.
pub struct File {
    vfs_file: VfsFile,
    direct: bool,
    #[cfg(feature = "times")]
    access_flags: AtomicU8,
    #[cfg(feature = "times")]
    is_effectively_readonly: bool,
}

struct PositionUpdatingDirSink<'a> {
    position: &'a mut u64,
    sink: &'a mut dyn DirEntrySink,
}

impl DirEntrySink for PositionUpdatingDirSink<'_> {
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool {
        let accepted = self.sink.accept(name, ino, node_type, offset);
        if accepted {
            *self.position = offset;
        }
        accepted
    }
}

impl File {
    const DIRECT_IO_CHUNK_SIZE: usize = 4096;

    pub fn new(location: Location, flags: FileFlags) -> Self {
        Self::with_open_flags(location, flags, 0)
    }

    pub fn with_open_flags(location: Location, flags: FileFlags, open_flags: u32) -> Self {
        let direct = Self::default_direct(&location, flags);
        Self::with_open_flags_and_direct(location, flags, open_flags, direct)
    }

    pub(crate) fn open_location(
        location: Location,
        flags: FileFlags,
        open_flags: u32,
        direct: bool,
    ) -> VfsResult<Self> {
        let file = Self::with_open_flags_and_direct(location, flags, open_flags, direct);
        if file.uses_page_cache() {
            file.page_cache_mapping()?;
        }
        Ok(file)
    }

    fn with_open_flags_and_direct(
        location: Location,
        flags: FileFlags,
        open_flags: u32,
        direct: bool,
    ) -> Self {
        let vfs_file = VfsFile::with_open_flags(location, flags, open_flags);
        #[cfg(feature = "times")]
        let is_effectively_readonly = vfs_file.location().is_effectively_readonly();
        Self {
            vfs_file,
            direct,
            #[cfg(feature = "times")]
            access_flags: AtomicU8::new(0),
            #[cfg(feature = "times")]
            is_effectively_readonly,
        }
    }

    fn default_direct(location: &Location, flags: FileFlags) -> bool {
        if location.flags().contains(NodeFlags::ALWAYS_CACHE) {
            return false;
        }
        let non_cacheable_type = matches!(
            location.node_type(),
            NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
        );
        non_cacheable_type
            || flags.contains(FileFlags::PATH)
            || location.flags().contains(NodeFlags::NON_CACHEABLE)
    }

    pub fn open(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new().read(true).open(context, path.as_ref())
    }

    pub fn create(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(context, path.as_ref())
    }

    pub fn access(&self, flags: FileFlags) -> VfsResult<()> {
        self.vfs_file.access(flags)
    }

    pub fn is_path(&self) -> bool {
        self.vfs_file.is_path()
    }

    pub fn flags(&self) -> FileFlags {
        self.vfs_file.flags()
    }

    /// Checks the node-level blocking attribute (e.g. regular files are always
    /// blocking). This is independent of the user-facing `O_NONBLOCK` flag
    /// checked by `nonblocking()`. When true, read/write bypass poll_io and
    /// execute directly.
    fn is_blocking(&self) -> bool {
        self.vfs_file.is_blocking()
    }

    fn uses_page_cache(&self) -> bool {
        !self.direct && !self.location().is_dir()
    }

    fn regular_file_mapping(location: &Location) -> Option<Arc<Mapping>> {
        if location.node_type() != NodeType::RegularFile
            || location.flags().contains(NodeFlags::NON_CACHEABLE)
        {
            return None;
        }
        location.address_space().page_cache()
    }

    /// Resizes the file after checking write access.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        self.location().check_writable_mount()?;
        check_file_size(self.location(), len)?;
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }
        if self.uses_page_cache() {
            shmem::check_resize_allowed(self.location(), self.location().len()?, len)?;
            return mapping::set_len(self.location(), len);
        }
        shmem::check_resize_allowed(self.location(), self.location().len()?, len)?;
        self.location().entry().as_file()?.set_len(len)?;
        if let Some(mapping) = Self::regular_file_mapping(self.location()) {
            mapping.set_len(len)?;
        }
        Ok(())
    }

    /// Removes a byte range and shifts the following bytes left.
    pub fn collapse_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        self.location().check_writable_mount()?;
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }
        if self.uses_page_cache() {
            mapping::flush_and_evict_from(self.location(), offset)?;
        }
        crate::fs::range_shift(self.location(), offset, len, false)
    }

    /// Inserts a zero-filled byte range and shifts the following bytes right.
    pub fn insert_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        self.location().check_writable_mount()?;
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }
        let new_len = self
            .location()
            .len()?
            .checked_add(len)
            .ok_or(VfsError::FileTooLarge)?;
        check_file_size(self.location(), new_len)?;
        if self.uses_page_cache() {
            mapping::flush_and_evict_from(self.location(), offset)?;
        }
        crate::fs::range_shift(self.location(), offset, len, true)
    }

    pub fn location(&self) -> &Location {
        self.vfs_file.location()
    }

    pub fn is_dir(&self) -> bool {
        self.location().is_dir()
    }

    pub fn page_cache_mapping(&self) -> VfsResult<Arc<Mapping>> {
        self.access(FileFlags::empty())?;
        mapping::mapping_for_location(self.location())
    }

    pub fn len(&self) -> VfsResult<u64> {
        self.location().len()
    }

    pub fn is_empty(&self) -> VfsResult<bool> {
        self.len().map(|len| len == 0)
    }

    pub fn check_is_dir(&self) -> VfsResult<()> {
        self.location().check_is_dir()
    }

    /// Returns memfd seal bits if this file is backed by a shmem object.
    pub fn shmem_seal_bits(&self) -> VfsResult<u32> {
        shmem::seal_bits_for_location(self.location())
    }

    /// Adds memfd seals if this file is backed by a shmem object.
    pub fn add_shmem_seals(&self, seal_bits: u32) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        shmem::add_seals_for_location(self.location(), seal_bits)
    }

    /// Checks whether this file may create a writable shared mapping.
    pub fn check_shmem_shared_writable_mapping_allowed(&self) -> VfsResult<()> {
        shmem::check_shared_writable_mapping_allowed(self.location())
    }

    /// Checks whether an existing shared mapping may satisfy a write fault.
    pub fn check_shmem_shared_write_fault_allowed(&self) -> VfsResult<()> {
        shmem::check_shared_write_fault_allowed(self.location())
    }

    /// Registers active shared mapping pages for this file.
    pub fn register_shmem_shared_pages(&self, pages: usize) -> VfsResult<()> {
        shmem::register_shared_pages(self.location(), pages)
    }

    /// Unregisters active shared mapping pages for this file.
    pub fn unregister_shmem_shared_pages(&self, pages: usize) {
        shmem::unregister_shared_pages(self.location(), pages);
    }

    /// Registers active writable shared mapping pages for this file.
    pub fn register_shmem_writable_shared_pages(&self, pages: usize) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        shmem::register_writable_shared_pages(self.location(), pages)
    }

    /// Unregisters active writable shared mapping pages for this file.
    pub fn unregister_shmem_writable_shared_pages(&self, pages: usize) {
        shmem::unregister_writable_shared_pages(self.location(), pages);
    }

    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.access(FileFlags::READ)?;
        if self.uses_page_cache() {
            return Self::read_page_cache_at(self.location(), dst, offset);
        }
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }
        if self.location().flags().contains(NodeFlags::STREAM) {
            return self.read_direct_at(dst, offset, None, false);
        }
        let Some(range) = generic_read_range(
            self.location(),
            offset,
            dst.remaining_mut(),
            self.location().len()?,
        )?
        else {
            return Ok(0);
        };
        if let Some(mapping) = Self::regular_file_mapping(self.location()) {
            return Self::read_mapping_at(&mapping, dst, range.offset(), range.len());
        }
        self.read_direct_at(dst, range.offset(), Some(range.len()), true)
    }

    /// Writes a number of bytes starting from a given offset.
    pub fn write_at(&self, src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        self.access(FileFlags::WRITE)?;
        self.write_at_checked(src, offset)
    }

    fn write_at_checked(&self, mut src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        self.location().check_writable_mount()?;
        shmem::check_write_allowed(self.location())?;
        if self.uses_page_cache() {
            return Self::write_page_cache_at(self.location(), src, offset);
        }
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }
        if self.location().flags().contains(NodeFlags::STREAM) {
            return self.write_direct_at(src, offset, None, false);
        }
        let Some(range) = generic_write_range(self.location(), offset, src.remaining())? else {
            return Ok(0);
        };
        let old_len = self.location().len()?;
        let requested_end = range.end();
        if requested_end > old_len {
            shmem::check_resize_allowed(self.location(), old_len, requested_end)?;
        }
        let mapping = Self::regular_file_mapping(self.location());
        let mut total = 0usize;
        let mut offset = range.offset();
        let mut remaining = range.len();
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining > 0 && !src.is_empty() {
            let want = chunk.len().min(remaining);
            let read = src.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }

            let mut written_in_chunk = 0usize;
            while written_in_chunk < read {
                let write_offset = offset;
                let written = self
                    .vfs_file
                    .operations()
                    .write_at(&chunk[written_in_chunk..read], offset)?;
                if written == 0 {
                    return Err(VfsError::WriteZero);
                }
                if mapping.is_some() {
                    self.location().address_space().write_from(
                        write_offset,
                        &chunk[written_in_chunk..written_in_chunk + written],
                    )?;
                }
                written_in_chunk += written;
                offset += written as u64;
            }

            total += read;
            remaining -= read;
        }

        Ok(total)
    }

    fn read_page_cache_at(
        loc: &Location,
        dst: impl Write + IoBufMut,
        offset: u64,
    ) -> VfsResult<usize> {
        let Some(range) = generic_read_range(loc, offset, dst.remaining_mut(), loc.len()?)? else {
            return Ok(0);
        };
        let mapping = mapping::mapping_for_location(loc)?;
        Self::read_mapping_at(&mapping, dst, range.offset(), range.len())
    }

    fn read_mapping_at(
        mapping: &Mapping,
        mut dst: impl Write + IoBufMut,
        mut offset: u64,
        mut remaining: usize,
    ) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];
        while remaining > 0 && !dst.is_full() {
            let want = chunk.len().min(remaining).min(dst.remaining_mut());
            if want == 0 {
                break;
            }
            let read = mapping.read_into_or_create(offset, &mut chunk[..want])?;
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
            remaining -= read;
            if read < want {
                break;
            }
        }
        Ok(total)
    }

    fn read_direct_at(
        &self,
        mut dst: impl Write + IoBufMut,
        mut offset: u64,
        mut remaining: Option<usize>,
        advance_offset: bool,
    ) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining != Some(0) && !dst.is_full() {
            let want = remaining
                .map_or(chunk.len(), |remaining| chunk.len().min(remaining))
                .min(dst.remaining_mut());
            if want == 0 {
                break;
            }

            let read = self
                .vfs_file
                .operations()
                .read_at(&mut chunk[..want], offset)?;
            if read == 0 {
                break;
            }
            if advance_offset {
                offset += read as u64;
            }

            let mut consumed = 0usize;
            while consumed < read {
                let written = dst.write(&chunk[consumed..read])?;
                if written == 0 {
                    return Err(VfsError::WriteZero);
                }
                consumed += written;
            }

            total += read;
            if let Some(left) = remaining.as_mut() {
                *left -= read;
            }

            if read < want {
                break;
            }
        }

        Ok(total)
    }

    fn write_page_cache_at(
        loc: &Location,
        mut src: impl Read + IoBuf,
        offset: u64,
    ) -> VfsResult<usize> {
        let Some(range) = generic_write_range(loc, offset, src.remaining())? else {
            return Ok(0);
        };
        let old_len = loc.len()?;
        let requested_end = range.end();
        if requested_end > old_len {
            shmem::check_resize_allowed(loc, old_len, requested_end)?;
            mapping::set_len(loc, requested_end)?;
        }

        let _mapping = mapping::mapping_for_location(loc)?;
        let address_space = loc.address_space();
        let mut total = 0usize;
        let mut offset = range.offset();
        let mut remaining = range.len();
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining > 0 && !src.is_empty() {
            let want = chunk.len().min(remaining);
            let read = src.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }
            address_space.write_from(offset, &chunk[..read])?;
            offset += read as u64;
            total += read;
            remaining -= read;
        }

        Ok(total)
    }

    fn write_direct_at(
        &self,
        mut src: impl Read + IoBuf,
        mut offset: u64,
        mut remaining: Option<usize>,
        advance_offset: bool,
    ) -> VfsResult<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining != Some(0) && !src.is_empty() {
            let want = remaining.map_or(chunk.len(), |remaining| chunk.len().min(remaining));
            let read = src.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }

            let mut written_in_chunk = 0usize;
            while written_in_chunk < read {
                let written = self
                    .vfs_file
                    .operations()
                    .write_at(&chunk[written_in_chunk..read], offset)?;
                if written == 0 {
                    return Err(VfsError::WriteZero);
                }
                written_in_chunk += written;
                if advance_offset {
                    offset += written as u64;
                }
            }

            total += read;
            if let Some(left) = remaining.as_mut() {
                *left -= read;
            }
        }

        Ok(total)
    }

    /// Attempts to sync OS-internal file content and metadata to disk.
    ///
    /// If `data_only` is `true`, only the file data is synced, not the
    /// metadata.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        if self.uses_page_cache() {
            mapping::sync(self.location(), data_only)
        } else if self.location().is_dir() {
            self.location().sync(data_only)
        } else {
            self.vfs_file.operations().fsync(data_only)
        }
    }

    pub fn read_dir(&self, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.access(FileFlags::READ)?;
        self.location().check_is_dir()?;
        let mut pos = self.vfs_file.position_lock_or_espipe()?;
        let start = *pos;
        let mut sink = PositionUpdatingDirSink {
            position: &mut pos,
            sink,
        };
        self.location().read_dir(start, &mut sink)
    }

    pub fn read(&self, dst: impl Write + IoBufMut) -> kio::Result<usize> {
        #[cfg(feature = "times")]
        {
            self.access_flags.fetch_or(1, Ordering::AcqRel);
        }
        if let Some(mut pos) = self.vfs_file.position_lock() {
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
        if let Some(mut pos) = self.vfs_file.position_lock() {
            if self.access(FileFlags::APPEND).is_ok() {
                self.append(src).map(|(written, new_size)| {
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

    fn append(&self, mut src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.access(FileFlags::WRITE)?;
        self.location().check_writable_mount()?;
        shmem::check_write_allowed(self.location())?;
        if self.uses_page_cache() {
            return Self::append_page_cache(self.location(), src);
        }
        if self.location().is_dir() {
            return Err(VfsError::IsADirectory);
        }

        let mut total = 0usize;
        let mut end = self.location().len()?;
        let Some(range) = generic_write_range(self.location(), end, src.remaining())? else {
            return Ok((0, end));
        };
        let mut remaining = range.len();
        let requested_end = range.end();
        if requested_end > end {
            shmem::check_resize_allowed(self.location(), end, requested_end)?;
        }
        let mapping = Self::regular_file_mapping(self.location());
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining > 0 && !src.is_empty() {
            let want = chunk.len().min(remaining);
            let read = src.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }

            let mut written_in_chunk = 0usize;
            while written_in_chunk < read {
                let (written, new_end) = self
                    .location()
                    .entry()
                    .as_file()?
                    .append(&chunk[written_in_chunk..read])?;
                if written == 0 {
                    return Err(VfsError::WriteZero);
                }
                if mapping.is_some() {
                    self.location().address_space().write_from(
                        end + written_in_chunk as u64,
                        &chunk[written_in_chunk..written_in_chunk + written],
                    )?;
                }
                written_in_chunk += written;
                end = new_end;
            }

            total += read;
            remaining -= read;
        }

        Ok((total, end))
    }

    fn append_page_cache(loc: &Location, mut src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        let mut total = 0usize;
        let mut end = loc.len()?;
        let Some(range) = generic_write_range(loc, end, src.remaining())? else {
            return Ok((0, end));
        };
        let requested_end = range.end();
        if requested_end > end {
            shmem::check_resize_allowed(loc, end, requested_end)?;
            mapping::set_len(loc, requested_end)?;
        }

        let _mapping = mapping::mapping_for_location(loc)?;
        let address_space = loc.address_space();
        let mut remaining = range.len();
        let mut chunk = [0u8; Self::DIRECT_IO_CHUNK_SIZE];

        while remaining > 0 && !src.is_empty() {
            let want = chunk.len().min(remaining);
            let read = src.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }
            address_space.write_from(end, &chunk[..read])?;
            end += read as u64;
            total += read;
            remaining -= read;
        }

        Ok((total, end))
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

        let mut guard = self.vfs_file.position_lock_or_espipe()?;
        let new_pos = match pos {
            SeekFrom::Start(pos) => pos,
            SeekFrom::End(off) => {
                let size = self.location().len()?;
                size.checked_add_signed(off).ok_or(VfsError::InvalidInput)?
            }
            SeekFrom::Current(off) => guard
                .checked_add_signed(off)
                .ok_or(VfsError::InvalidInput)?,
        };
        *guard = new_pos;
        Ok(new_pos)
    }
}

impl Pollable for File {
    fn poll(&self) -> IoEvents {
        self.location().poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.location().register(context, events)
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
        self.access(FileFlags::empty())?;
        self.location().ioctl(cmd, arg)
    }

    fn set_nonblocking(&self, flag: bool) -> KResult {
        self.vfs_file.set_nonblocking(flag);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.vfs_file.nonblocking()
    }

    fn open_flags(&self) -> u32 {
        self.vfs_file.open_flags()
    }

    fn path(&self) -> Cow<'_, str> {
        path_for(self.location())
    }

    fn vfs_location(&self) -> Option<Location> {
        Some(self.location().clone())
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
        if self.location().is_dir() {
            return Err(KError::NoSuchDevice);
        }
        if self.direct
            && matches!(
                self.location().node_type(),
                NodeType::CharacterDevice | NodeType::BlockDevice
            )
        {
            self.location().mmap(mapper)?;
        } else {
            mapper.map_file_backed()?;
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
            if let Err(err) = self.location().update_metadata(update) {
                warn!("Failed to update file times on drop: {err:?}");
            }
        }
    }
}
